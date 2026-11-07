//! The content-addressed object store of layr (experimental, docs/objects.md): Merkle trees with
//! POSIX metadata, blobs kept whole for clones or compressed as zstd frames, deltas and
//! content-defined chunks.
#![forbid(unsafe_code)]

pub mod checkout;
pub mod compare;
pub mod fsx;
pub mod id;
pub mod store;
pub mod tree;
pub mod write;

pub use checkout::{checkout, CheckoutReport};
pub use compare::{compare, Comparison};
pub use id::{Id, Keying, Kind};
pub use store::{Form, Stat, Store, Temperature};
pub use tree::{Entry, Node, Tree};
pub use write::{write_tree, WriteOptions, WriteReport};

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;

    pub fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    }

    /// A new empty folder under the temporary folder, removed by the next run of the same test.
    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("layr-objects-test-{}-{name}", std::process::id()));
        if dir.exists() {
            make_writable(&dir);
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn make_writable(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    make_writable(&e.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{make_writable, noise, scratch};
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    fn chmod(p: &Path, mode: u32) {
        fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A folder with every kind of entry a tree keeps.
    fn sample(root: &Path) -> bool {
        fs::create_dir_all(root.join("src/deep/er")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/deep/er/same.txt"), "same\n").unwrap();
        fs::write(root.join("same-too.txt"), "same\n").unwrap();
        fs::write(root.join("empty-file"), "").unwrap();
        fs::write(root.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        // Same bytes as run.sh without its attribute: one blob, two attribute sets.
        fs::write(root.join("run-copy.sh"), "#!/bin/sh\necho hi\n").unwrap();
        chmod(&root.join("run.sh"), 0o755);
        fs::write(root.join("locked/readonly"), "r\n").unwrap();
        chmod(&root.join("locked/readonly"), 0o444);
        fs::write(root.join("big.bin"), noise(3 << 20, 5)).unwrap();
        std::os::unix::fs::symlink("src/main.rs", root.join("link")).unwrap();
        std::os::unix::fs::symlink("missing/target", root.join("dangling")).unwrap();
        fs::hard_link(root.join("src/main.rs"), root.join("hard")).unwrap();
        fs::hard_link(root.join("src/main.rs"), root.join("src/deep/hard2")).unwrap();
        chmod(&root.join("locked"), 0o555);
        // A user attribute: on a file system without them the test still checks the rest.
        let xattr = fsx::set_xattrs(&root.join("run.sh"), &[(b"user.layr-test".to_vec(), b"v\0\xff".to_vec())]).is_ok();
        #[cfg(target_os = "linux")]
        fs::write(root.join(fsx::os(b"latin1-caf\xe9")), "not UTF-8\n").unwrap();
        xattr
    }

    fn round_trip(temperature: Temperature) {
        let dir = scratch(&format!("round-trip-{temperature:?}"));
        let src = dir.join("src");
        let xattr = sample(&src);
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let opts = WriteOptions { temperature, previous: None, exclude: vec![] };
        let report = write_tree(&store, &src, &opts).unwrap();
        let tree = report.tree.unwrap();
        assert_eq!(report.hard_link_groups, 1);
        // The same folder gives the same tree and adds nothing.
        let added = store.added.objects.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(write_tree(&store, &src, &opts).unwrap().tree.unwrap(), tree);
        assert_eq!(store.added.objects.load(std::sync::atomic::Ordering::Relaxed), added);

        let out = dir.join("out");
        let co = checkout(&store, &tree, &out).unwrap();
        let cmp = compare(&src, &out).unwrap();
        assert!(cmp.differences.is_empty(), "{:#?}", cmp.differences);
        assert!(cmp.entries > 10);
        if temperature == Temperature::Hot {
            assert_eq!(co.decoded, 0);
        } else {
            assert_eq!(co.clones + co.copies, 0);
        }
        assert_eq!(co.hard_links, 2);
        let m = fs::symlink_metadata(out.join("hard")).unwrap();
        assert_eq!(m.nlink(), 3);
        assert_eq!(m.ino(), fs::symlink_metadata(out.join("src/main.rs")).unwrap().ino());
        assert!(out.join("empty").is_dir());
        assert_eq!(fs::read_link(out.join("dangling")).unwrap(), Path::new("missing/target"));
        if xattr {
            assert_eq!(fsx::xattrs(&out.join("run.sh")).unwrap(), vec![(b"user.layr-test".to_vec(), b"v\0\xff".to_vec())]);
        }
        // The tree from the copy is the tree of the original.
        assert_eq!(write_tree(&store, &out, &opts).unwrap().tree.unwrap(), tree);
        // A checkout into a folder with files is refused.
        assert!(checkout(&store, &tree, &src).is_err());
        make_writable(&dir);
    }

    #[test]
    fn a_folder_round_trips_hot() {
        round_trip(Temperature::Hot);
    }

    #[test]
    fn a_folder_round_trips_cold() {
        round_trip(Temperature::Cold);
    }

    #[test]
    fn verify_finds_each_kind_of_difference() {
        let dir = scratch("differences");
        let a = dir.join("a");
        sample(&a);
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let tree = write_tree(&store, &a, &WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![] })
            .unwrap()
            .tree
            .unwrap();
        type Change = (&'static str, Box<dyn Fn(&Path)>);
        let change: Vec<Change> = vec![
            (
                "content differs: src/main.rs",
                Box::new(|b: &Path| {
                    fs::remove_file(b.join("src/main.rs")).unwrap();
                    fs::write(b.join("src/main.rs"), "fn main() { }\n").unwrap();
                }),
            ),
            ("mode differs: run.sh", Box::new(|b: &Path| chmod(&b.join("run.sh"), 0o644))),
            ("only in the copy: extra", Box::new(|b: &Path| fs::write(b.join("extra"), "").unwrap())),
            ("only in the original: empty", Box::new(|b: &Path| fs::remove_dir(b.join("empty")).unwrap())),
            (
                "link target differs: link",
                Box::new(|b: &Path| {
                    fs::remove_file(b.join("link")).unwrap();
                    std::os::unix::fs::symlink("elsewhere", b.join("link")).unwrap();
                }),
            ),
            (
                "hard link group differs",
                Box::new(|b: &Path| {
                    fs::remove_file(b.join("hard")).unwrap();
                    fs::write(b.join("hard"), "fn main() {}\n").unwrap();
                }),
            ),
        ];
        for (n, (expected, f)) in change.iter().enumerate() {
            let b = dir.join(format!("b{n}"));
            checkout(&store, &tree, &b).unwrap();
            make_writable(&b);
            f(&b);
            let diffs = compare(&a, &b).unwrap().differences;
            assert!(diffs.iter().any(|d| d.starts_with(expected)), "{expected}: {diffs:#?}");
        }
        make_writable(&dir);
    }

    #[test]
    fn a_cold_history_stores_changes_as_deltas() {
        let dir = scratch("history");
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let mut text = String::new();
        for i in 0..3000 {
            text.push_str(&format!("line {i} of a source file that changes a little in each version\n"));
        }
        let mut previous = None;
        let mut trees = Vec::new();
        for version in 0..5 {
            text.push_str(&format!("version {version}\n"));
            fs::write(src.join("file.txt"), &text).unwrap();
            let opts = WriteOptions { temperature: Temperature::Cold, previous, exclude: vec![] };
            let tree = write_tree(&store, &src, &opts).unwrap().tree.unwrap();
            trees.push((tree, text.clone()));
            previous = Some(tree);
        }
        let s = store.stat().unwrap();
        assert_eq!(s.delta_blobs, 4);
        assert!(s.delta_bytes < 4 * 200, "{s:?}");
        for (n, (tree, text)) in trees.iter().enumerate() {
            let out = dir.join(format!("out{n}"));
            checkout(&store, tree, &out).unwrap();
            assert_eq!(fs::read_to_string(out.join("file.txt")).unwrap(), *text);
        }
    }

    #[test]
    fn secret_ids_differ_from_plain_ids() {
        let dir = scratch("secret");
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), "x").unwrap();
        let opts = WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![] };
        let plain = Store::open(&dir.join("plain"), Keying::Plain).unwrap();
        let secret = Store::open(&dir.join("secret"), Keying::Keyed(id::project_key("AGE-SECRET-KEY-1X", "p"))).unwrap();
        let a = write_tree(&plain, &src, &opts).unwrap().tree.unwrap();
        let b = write_tree(&secret, &src, &opts).unwrap().tree.unwrap();
        assert_ne!(a, b);
        assert!(!secret.has(&Keying::Plain.id(Kind::Blob, b"x")), "a keyed store names no plain blob");
        checkout(&secret, &b, &dir.join("out")).unwrap();
        assert_eq!(fs::read(dir.join("out/f")).unwrap(), b"x");
    }
}
