//! Portable merge regressions: independent object checkouts have no inode lineage.
use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile_path("object-merge")?;
        for name in ["base", "ours", "theirs", "result"] {
            std::fs::create_dir_all(root.join(name))?;
        }
        Ok(Self(root))
    }
    fn source(&self, name: &str, portable: bool) -> Result<crate::repo::Snap> {
        let native = self.0.join(name);
        if !portable {
            return Ok(crate::repo::snap(&native));
        }
        let store = layr_objects::Store::open(&self.0.join("objects"), layr_objects::Keying::Plain)?;
        let written = layr_objects::write_tree(
            &store,
            &native,
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        let tree = crate::object_tree::ObjectTree::new(
            store,
            layr_objects::StateRef { tree: written.tree.context("missing tree")?, times: written.times.context("missing times")? },
            &self.0.join("reads"),
        )?;
        std::fs::remove_dir_all(native)?;
        #[cfg(target_os = "macos")]
        return Ok(crate::thin::Layered::objects(tree));
        #[cfg(not(target_os = "macos"))]
        Ok(crate::source::Source::Objects(tree))
    }
    fn merge(&self, ours_portable: bool, theirs_portable: bool) -> Result<Outcome> {
        let (base, ours, theirs) =
            (self.source("base", true)?, self.source("ours", ours_portable)?, self.source("theirs", theirs_portable)?);
        let result = self.0.join("result");
        fsutil::walk(&ours, "", &mut |p, _| {
            fsutil::copy_entry(&ours, p, &result, p, None)?;
            Ok(true)
        })?;
        merge(&Input {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            result: &result,
            base_id: "01a12290-0000-7000-8000-000000000001",
            ours_id: "01a12290-0000-7000-8000-000000000002",
            theirs_id: "01a12290-0000-7000-8000-000000000003",
            ours_label: "head",
            theirs_label: "topic",
            config: &|key| match key {
                "merge.driver.sum" => Some("echo $(( $(cat %A) + $(cat %B) - $(cat %O) )) > %A".into()),
                "merge.verify.size" => Some("test $(wc -c < %A) = 65536".into()),
                _ => None,
            },
            run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
            allow_root: false,
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn object_merges_pair_moves_and_preserve_edits_without_native_states() -> Result<()> {
    for (ours_portable, theirs_portable) in [(true, true), (true, false), (false, true)] {
        let f = Fixture::new()?;
        for (dir, moved, text, q) in [
            ("base", "A.txt", "one\ntwo\nthree\n", "Q.txt"),
            ("ours", "A.txt", "one\nOURS\nthree\n", "Qo.txt"),
            ("theirs", "A2.txt", "one\ntwo\nthree\n", "Qt.txt"),
        ] {
            std::fs::write(f.0.join(dir).join(moved), text)?;
            std::fs::write(f.0.join(dir).join(q), "q\n")?;
        }
        let out = f.merge(ours_portable, theirs_portable)?;
        assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
        assert_eq!(out.conflicts[0].path, "Q.txt");
        assert!(matches!(&out.conflicts[0].details, crate::model::ConflictDetails::Known { kind: ConflictKind::RenameRename { .. }, .. }));
        assert_eq!(std::fs::read_to_string(f.0.join("result/A2.txt"))?, "one\nOURS\nthree\n");
        assert!(!f.0.join("result/A.txt").exists());
    }
    Ok(())
}

#[test]
fn object_merges_read_attributes_drivers_and_disjoint_blocks() -> Result<()> {
    for (ours_portable, theirs_portable) in [(true, true), (true, false), (false, true)] {
        let f = Fixture::new()?;
        for (dir, label, num, offset) in
            [("base", "base", "1", None), ("ours", "main", "3", Some(0)), ("theirs", "topic", "5", Some(40960))]
        {
            let root = f.0.join(dir);
            std::fs::write(
                root.join(".layrattributes"),
                "list merge=union\nkeep merge=ours\n*.img merge=blocks merge-verify=size\nnum merge=sum\n",
            )?;
            std::fs::write(root.join("list"), if dir == "base" { "a\n".into() } else { format!("a\n{label}\n") })?;
            std::fs::write(root.join("keep"), label)?;
            std::fs::write(root.join("num"), num)?;
            let mut disk = vec![0; 65536];
            if let Some(offset) = offset {
                disk[offset..offset + 4].copy_from_slice(if dir == "ours" { b"MAIN" } else { b"TOP!" });
            }
            std::fs::write(root.join("disk.img"), disk)?;
        }
        let out = f.merge(ours_portable, theirs_portable)?;
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
        assert_eq!(std::fs::read_to_string(f.0.join("result/list"))?, "a\nmain\ntopic\n");
        assert_eq!(std::fs::read_to_string(f.0.join("result/keep"))?, "main");
        assert_eq!(std::fs::read_to_string(f.0.join("result/num"))?.trim(), "7");
        let disk = std::fs::read(f.0.join("result/disk.img"))?;
        assert_eq!(&disk[..4], b"MAIN");
        assert_eq!(&disk[40960..40964], b"TOP!");
    }
    Ok(())
}

#[test]
fn object_blocks_reject_overlap_and_resize_and_keep_ours() -> Result<()> {
    for resize in [false, true] {
        let f = Fixture::new()?;
        for (dir, bytes) in
            [("base", b"00000000".as_slice()), ("ours", b"OURS0000"), ("theirs", if resize { b"short" } else { b"0THEIRS0" })]
        {
            std::fs::write(f.0.join(dir).join(".layrattributes"), "disk.img merge=blocks\n")?;
            std::fs::write(f.0.join(dir).join("disk.img"), bytes)?;
        }
        let out = f.merge(true, true)?;
        assert_eq!(out.conflicts.len(), 1);
        assert_eq!(out.conflicts[0].path, "disk.img");
        assert_eq!(std::fs::read(f.0.join("result/disk.img"))?, b"OURS0000");
    }
    Ok(())
}

#[test]
fn object_blocks_follow_a_move_when_both_sides_edit() -> Result<()> {
    let f = Fixture::new()?;
    for (dir, name, offset) in [("base", "disk.img", None), ("ours", "disk.img", Some(0)), ("theirs", "moved.img", Some(8192))] {
        std::fs::write(f.0.join(dir).join(".layrattributes"), "*.img merge=blocks\n")?;
        // Text without NULs permits content-based pairing of a move with an edit.
        let mut bytes = b"unchanged line\n".repeat(1024);
        if let Some(offset) = offset {
            bytes[offset..offset + 4].copy_from_slice(if dir == "ours" { b"OURS" } else { b"THEM" });
        }
        std::fs::write(f.0.join(dir).join(name), bytes)?;
    }
    let out = f.merge(true, true)?;
    assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
    let bytes = std::fs::read(f.0.join("result/moved.img"))?;
    assert_eq!(&bytes[..4], b"OURS");
    assert_eq!(&bytes[8192..8196], b"THEM");
    assert!(!f.0.join("result/disk.img").exists());
    Ok(())
}

#[test]
fn object_merge_does_not_resurrect_unchanged_files_inside_a_deleted_directory() -> Result<()> {
    let f = Fixture::new()?;
    for dir in ["base", "theirs"] {
        std::fs::create_dir(f.0.join(dir).join("X"))?;
        std::fs::write(f.0.join(dir).join("X/old"), "unchanged\n")?;
    }
    std::fs::write(f.0.join("theirs/X/new"), "added\n")?;
    let out = f.merge(true, true)?;
    assert!(out.conflicts.iter().any(|c| c.path == "X/new"));
    assert!(!out.conflicts.iter().any(|c| c.path == "X/old"));
    assert!(!f.0.join("result/X/old").exists());
    assert_eq!(std::fs::read_to_string(f.0.join("result/X/new"))?, "added\n");
    Ok(())
}

#[test]
fn object_directory_moves_receive_edits_at_the_new_path() -> Result<()> {
    for moved_by_ours in [true, false] {
        for (ours_portable, theirs_portable) in [(true, true), (true, false), (false, true)] {
            let f = Fixture::new()?;
            for (side, dir, text) in [
                ("base", "X", "one\ntwo\nthree\nfour\nfive\n"),
                ("ours", if moved_by_ours { "Y" } else { "X" }, "one\nOURS\nthree\nfour\nfive\n"),
                ("theirs", if moved_by_ours { "X" } else { "Y" }, "one\ntwo\nthree\nfour\nTHEIRS\n"),
            ] {
                std::fs::create_dir(f.0.join(side).join(dir))?;
                std::fs::write(f.0.join(side).join(dir).join("file"), text)?;
            }
            let out = f.merge(ours_portable, theirs_portable)?;
            assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
            assert_eq!(std::fs::read_to_string(f.0.join("result/Y/file"))?, "one\nOURS\nthree\nfour\nTHEIRS\n");
            assert!(!f.0.join("result/X").exists());
        }
    }
    Ok(())
}

#[test]
fn object_merges_preserve_symlink_changes_through_renames() -> Result<()> {
    for (ours_portable, theirs_portable) in [(true, true), (true, false), (false, true)] {
        let f = Fixture::new()?;
        std::os::unix::fs::symlink("target1", f.0.join("base/link"))?;
        std::os::unix::fs::symlink("target2", f.0.join("ours/link"))?;
        std::os::unix::fs::symlink("target1", f.0.join("theirs/moved"))?;
        let out = f.merge(ours_portable, theirs_portable)?;
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
        assert_eq!(std::fs::read_link(f.0.join("result/moved"))?, PathBuf::from("target2"));
    }
    Ok(())
}

#[test]
fn rename_detection_compares_symlink_targets_as_bytes() -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    for portable in [false, true] {
        let f = Fixture::new()?;
        std::os::unix::fs::symlink(std::ffi::OsString::from_vec(vec![0xff]), f.0.join("base/old"))?;
        std::os::unix::fs::symlink(std::ffi::OsString::from_vec(vec![0xfe]), f.0.join("theirs/new"))?;
        let base = f.source("base", portable)?;
        let child = f.source("theirs", portable)?;
        let list = vec![
            EntryChange::Remove { path: "old".into(), old: fsutil::lmeta(&base, "old").context("missing old link")? },
            EntryChange::Add { path: "new".into(), new: fsutil::lmeta(&child, "new").context("missing new link")? },
        ];
        let result = crate::changes::with_renames(&base, &child, list);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|change| change.old_path().is_none()));
    }
    Ok(())
}
