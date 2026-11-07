//! The content-addressed object store of layr (experimental, docs/objects.md): Merkle trees with
//! POSIX metadata, blobs kept whole for clones or compressed as zstd frames, deltas and
//! content-defined chunks.
#![forbid(unsafe_code)]

pub mod checkout;
pub mod compare;
pub mod fsx;
pub mod id;
pub mod pack;
pub mod store;
pub mod times;
pub mod transfer;
pub mod tree;
pub mod write;
pub mod zstdx;

pub use checkout::{checkout, checkout_as, checkout_with, CheckoutReport, Writer};
pub use compare::{compare, compare_with, Comparison};
pub use id::{Id, Keying, Kind};
pub use store::{Form, Stat, StateRef, Store, Temperature};
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

    /// A new empty folder under the temporary folder, removed when the test ends.
    pub struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            make_writable(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn scratch(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("layr-objects-test-{}-{name}", std::process::id()));
        if dir.exists() {
            make_writable(&dir);
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
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
        times::set(&root.join("notes-old"), 0).ok();
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
        let opts = WriteOptions { temperature, previous: None, exclude: vec![], similar: false };
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

        // Times: a checkout with the times table restores every modification time (folders and
        // links included); one without it does not. The times are not part of the tree id.
        let report_times = report.times.unwrap();
        let timed = dir.join("timed");
        let co = checkout_with(&store, &tree, Some(&report_times), &timed).unwrap();
        assert!(co.times > 10);
        let cmp = compare_with(&src, &timed, true).unwrap();
        assert!(cmp.differences.is_empty(), "{:#?}", cmp.differences);
        assert!(!compare_with(&src, &out, true).unwrap().differences.is_empty(), "the plain checkout has new times");
        times::set(&src.join("src/main.rs"), 1_000_000_000_123_456_789).unwrap();
        let moved = write_tree(&store, &src, &opts).unwrap();
        assert_eq!(moved.tree.unwrap(), tree, "a new time keeps the tree id");
        assert_ne!(moved.times.unwrap(), report_times, "and changes the times table");
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
        let tree = write_tree(
            &store,
            &a,
            &WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![], similar: false },
        )
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
            let opts = WriteOptions { temperature: Temperature::Cold, previous, exclude: vec![], similar: false };
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
    fn an_incremental_write_equals_a_full_write() {
        let dir = scratch("incremental");
        let src = dir.join("src");
        sample(&src);
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let opts = WriteOptions { temperature: Temperature::Cold, previous: None, exclude: vec![], similar: false };
        let first = write_tree(&store, &src, &opts).unwrap();
        let base = write::Base { tree: first.tree.unwrap(), times: first.times.unwrap() };
        // Changes: new content, a new folder with a file, a removed file, a mode, a new time.
        fs::write(src.join("src/deep/er/same.txt"), "changed\n").unwrap();
        fs::create_dir_all(src.join("new/inner")).unwrap();
        fs::write(src.join("new/inner/file"), "new\n").unwrap();
        fs::remove_file(src.join("empty-file")).unwrap();
        chmod(&src.join("run-copy.sh"), 0o700);
        times::set(&src.join("same-too.txt"), 5_000_000_000).unwrap();
        let changed: Vec<Vec<u8>> = ["src/deep/er/same.txt", "new", "empty-file", "run-copy.sh", "same-too.txt"]
            .iter()
            .map(|p| p.as_bytes().to_vec())
            .collect();
        let full = write_tree(&store, &src, &opts).unwrap();
        let inc = write::write_changed(&store, &src, &base, &changed, &opts).unwrap();
        assert_eq!(inc.tree, full.tree, "the incremental tree equals the full tree");
        assert_eq!(inc.times, full.times, "and so do the times");
        assert!(inc.files < full.files, "it read only the changed files ({} of {})", inc.files, full.files);
    }

    #[test]
    fn incremental_hard_links_match_a_full_write() {
        let dir = scratch("incremental-links");
        let src = dir.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a"), "first").unwrap();
        fs::hard_link(src.join("a"), src.join("sub/b")).unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let opts = WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![], similar: false };
        let first = write_tree(&store, &src, &opts).unwrap();
        let base = write::Base { tree: first.tree.unwrap(), times: first.times.unwrap() };
        fs::write(src.join("a"), "changed through one name").unwrap();
        let inc = write::write_changed(&store, &src, &base, &[b"a".to_vec()], &opts).unwrap();
        let full = write_tree(&store, &src, &opts).unwrap();
        assert_eq!(inc.tree, full.tree);
        assert_eq!(inc.times, full.times);
        fs::hard_link(src.join("a"), src.join("new")).unwrap();
        let inc = write::write_changed(&store, &src, &base, &[b"new".to_vec()], &opts).unwrap();
        let full = write_tree(&store, &src, &opts).unwrap();
        assert_eq!(inc.tree, full.tree);
    }

    #[test]
    fn a_save_writes_only_the_times_on_the_way_to_a_change() {
        let dir = scratch("times-path");
        let src = dir.join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::create_dir_all(src.join("c/d")).unwrap();
        fs::write(src.join("a/b/f"), "one\n").unwrap();
        fs::write(src.join("c/d/g"), "two\n").unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let opts = WriteOptions { temperature: Temperature::Cold, previous: None, exclude: vec![], similar: false };
        let first = write_tree(&store, &src, &opts).unwrap();
        let base = write::Base { tree: first.tree.unwrap(), times: first.times.unwrap() };
        fs::write(src.join("a/b/f"), "changed\n").unwrap();
        times::set(&src.join("a/b/f"), 5_000_000_000).unwrap();
        let next = write::write_changed(&store, &src, &base, &[b"a/b/f".to_vec()], &opts).unwrap();
        let before: std::collections::HashSet<Id> = store.times_ids(&base.times).unwrap().into_iter().collect();
        let after = store.times_ids(&next.times.unwrap()).unwrap();
        assert_eq!(after.len(), before.len(), "the same folders");
        let new = after.iter().filter(|id| !before.contains(id)).count();
        assert_eq!(new, 3, "the root, a and a/b are new; c and c/d are shared");
        // The new state checks out with its times, the old one with its own.
        let out = dir.join("out");
        checkout_with(&store, &next.tree.unwrap(), Some(&next.times.unwrap()), &out).unwrap();
        let cmp = compare_with(&src, &out, true).unwrap();
        assert!(cmp.differences.is_empty(), "{:#?}", cmp.differences);
        make_writable(&dir);
    }

    #[test]
    fn every_checkout_write_goes_through_the_writer() {
        let dir = scratch("writer");
        let src = dir.join("src");
        sample(&src);
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let opts = WriteOptions { temperature: Temperature::Cold, previous: None, exclude: vec![], similar: false };
        let r = write_tree(&store, &src, &opts).unwrap();
        let (tree, times) = (r.tree.unwrap(), r.times.unwrap());
        let steps = std::sync::atomic::AtomicU64::new(0);
        // The writer counts its steps: every entry, mode and time is one or more of them.
        let counting = |f: &mut dyn FnMut() -> std::io::Result<()>| -> std::io::Result<()> {
            steps.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            f()
        };
        let out = dir.join("out");
        let co = checkout_as(&store, &tree, Some(&times), &out, &counting).unwrap();
        let cmp = compare_with(&src, &out, true).unwrap();
        assert!(cmp.differences.is_empty(), "{:#?}", cmp.differences);
        let n = steps.load(std::sync::atomic::Ordering::Relaxed);
        assert!(n >= co.files + co.dirs + co.symlinks + co.times, "{n} steps for {co:?}");
        // A writer that refuses (the caller may not write there) leaves nothing.
        let refusing = |_: &mut dyn FnMut() -> std::io::Result<()>| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        };
        assert!(checkout_as(&store, &tree, Some(&times), &dir.join("refused"), &refusing).is_err());
        assert!(!dir.join("refused").exists());
    }

    #[test]
    fn a_packed_store_round_trips() {
        let dir = scratch("packed");
        let src = dir.join("src");
        sample(&src);
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let opts = WriteOptions { temperature: Temperature::Cold, previous: None, exclude: vec![], similar: false };
        let tree = write_tree(&store, &src, &opts).unwrap().tree.unwrap();
        let times = write_tree(&store, &src, &opts).unwrap().times.unwrap();
        store.set_state("s1", &store::StateRef { tree, times }).unwrap();
        let before = store.stat().unwrap();
        assert!(before.loose_bytes() > 0 && before.packs == 0);
        // A second store object (another process) reads the packs the first one writes.
        let other = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let report = store.pack().unwrap();
        assert_eq!(report.packs, 1);
        assert_eq!(
            report.objects,
            before.trees + before.whole_blobs + before.delta_blobs + before.chunked_blobs + before.chunks + before.times
        );
        let after = store.stat().unwrap();
        assert_eq!(after.loose_bytes(), 0, "no loose object is left");
        assert_eq!(after.packed_objects, report.objects);
        let out = dir.join("out");
        checkout(&other, &tree, &out).unwrap();
        let cmp = compare(&src, &out).unwrap();
        assert!(cmp.differences.is_empty(), "{:#?}", cmp.differences);
        // Writing the same folder again adds nothing: packed objects count as present.
        let added = store.added.objects.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(write_tree(&store, &src, &opts).unwrap().tree.unwrap(), tree);
        assert_eq!(store.added.objects.load(std::sync::atomic::Ordering::Relaxed), added);
        // A second pack run changes nothing.
        let again = store.pack().unwrap();
        assert_eq!((again.packs, again.objects, again.removed), (0, 0, 0));
        // A loose copy of a packed object goes at the next run.
        let (blob, _) = store.put_cold_bytes(b"fn main() {}\n", None).unwrap();
        assert!(store.has(&blob));
        let copy = Store::open(&dir.join("store2"), Keying::Plain).unwrap();
        copy.put_cold_bytes(b"fn main() {}\n", None).unwrap();
        let hex = blob.hex();
        let loose = dir.join("store/objects").join(&hex[..2]).join(&hex[2..]);
        fs::create_dir_all(loose.parent().unwrap()).unwrap();
        fs::copy(dir.join("store2/objects").join(&hex[..2]).join(&hex[2..]), &loose).unwrap();
        assert_eq!(store.pack().unwrap().removed, 1);
        assert!(!loose.exists());
        // A changed byte in a pack block is found on read.
        let pack = fs::read_dir(dir.join("store/packs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|x| x == "pack"))
            .unwrap();
        let mut bytes = fs::read(&pack).unwrap();
        let at = bytes.len() / 2;
        bytes[at] ^= 0x55;
        chmod(&pack, 0o644);
        fs::write(&pack, bytes).unwrap();
        let fresh = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let broken = dir.join("broken");
        let read = checkout(&fresh, &tree, &broken).map(|_| compare(&src, &broken).unwrap().differences);
        assert!(!matches!(read, Ok(ref d) if d.is_empty()), "a corrupted pack must not check out cleanly");
    }

    #[test]
    fn a_repack_trains_a_dictionary_for_small_loose_blobs() {
        let dir = scratch("dict");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let file = |i: usize| {
            format!("// module {i}\nuse std::collections::HashMap;\n\npub fn handler_{i}(map: &HashMap<String, u64>) -> u64 {{\n    map.values().sum::<u64>() * {i}\n}}\n")
        };
        for i in 0..200 {
            store.put_cold_bytes(file(i).as_bytes(), None).unwrap();
        }
        assert!(store.stat().unwrap().whole_blobs == 200);
        store.pack().unwrap();
        assert_eq!(fs::read_dir(dir.join("store/dicts")).unwrap().count(), 1, "the repack trained one dictionary");
        let plain = layr_store::zst::compress(file(1000).as_bytes(), None).unwrap().len();
        let (id, _) = store.put_cold_bytes(file(1000).as_bytes(), None).unwrap();
        let hex = id.hex();
        let loose = fs::read(dir.join("store/objects").join(&hex[..2]).join(&hex[2..])).unwrap();
        assert_eq!(&loose[..6], b"LOB1by", "a small blob uses the dictionary");
        assert!(loose.len() - 38 < plain, "and is smaller: {} against {plain}", loose.len() - 38);
        // Another store object reads it (it loads the dictionary), and the next repack packs it raw.
        let other = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        assert_eq!(other.read_blob(&id).unwrap(), file(1000).as_bytes());
        store.pack().unwrap();
        assert_eq!(Store::open(&dir.join("store"), Keying::Plain).unwrap().read_blob(&id).unwrap(), file(1000).as_bytes());
        assert_eq!(fs::read_dir(dir.join("store/dicts")).unwrap().count(), 1, "the dictionary stays");
    }

    #[test]
    fn hot_blobs_outside_the_kept_trees_turn_cold() {
        let dir = scratch("evict");
        let src = dir.join("src");
        sample(&src);
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let opts = WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![], similar: false };
        let old = write_tree(&store, &src, &opts).unwrap().tree.unwrap();
        fs::write(src.join("notes-new.txt"), "only in the new tree\n").unwrap();
        fs::remove_file(src.join("big.bin")).unwrap();
        let new = write_tree(&store, &src, &opts).unwrap().tree.unwrap();
        let hot_before = store.stat().unwrap().hot_blobs;
        // Keep the new tree hot: big.bin (only in the old tree) turns cold, as chunks.
        let r = store.evict_hot(&[new]).unwrap();
        assert_eq!(r.blobs, 1);
        let s = store.stat().unwrap();
        assert_eq!(s.hot_blobs, hot_before - 1);
        assert!(s.chunked_blobs == 1 && s.chunks > 0);
        // Both trees still check out; the old one decodes the evicted blob.
        let out = dir.join("old");
        let co = checkout(&store, &old, &out).unwrap();
        assert_eq!(co.decoded, 1);
        assert_eq!(fs::read(out.join("big.bin")).unwrap(), noise(3 << 20, 5));
        // Nothing is kept: every hot blob turns cold, and the store still checks out after a pack.
        store.evict_hot(&[]).unwrap();
        assert_eq!(store.stat().unwrap().hot_blobs, 0);
        store.pack().unwrap();
        let again = dir.join("again");
        checkout(&store, &new, &again).unwrap();
        assert!(compare(&src, &again).unwrap().differences.is_empty());
    }

    /// The test vectors of docs/objects.md, section "Test vectors". A change here changes the format.
    #[test]
    fn format_test_vectors() {
        let k = Keying::Plain;
        let hello = k.id(Kind::Blob, b"hello\n");
        let empty_tree = Tree::default();
        let mut tree = Tree {
            entries: vec![
                Entry {
                    name: b"a.txt".to_vec(),
                    mode: 0o644,
                    node: Node::File { size: 6, blob: hello },
                    xattrs: vec![(b"user.k".to_vec(), b"v".to_vec())],
                },
                Entry {
                    name: b"bin".to_vec(),
                    mode: 0o755,
                    node: Node::Dir { tree: k.id(Kind::Tree, &empty_tree.encode()) },
                    xattrs: vec![],
                },
                Entry { name: b"link".to_vec(), mode: 0, node: Node::Symlink { target: b"a.txt".to_vec() }, xattrs: vec![] },
            ],
            links: vec![],
        };
        tree.normalize();
        let t = 1_700_000_000_000_000_000i64;
        let bin_times = times::Times { own: t + 1, records: vec![] };
        let times = times::Times {
            own: t,
            records: vec![
                times::Record::Time(t),
                times::Record::Folder(k.id(Kind::Times, &bin_times.encode())),
                times::Record::Time(t),
            ],
        };
        let key = id::epoch_key("AGE-SECRET-KEY-1EXAMPLE", "epoch-1");
        let lines = [
            format!("blob \"\" {}", k.id(Kind::Blob, b"")),
            format!("blob \"hello\\n\" {hello}"),
            format!("chunk \"abc\" {}", k.id(Kind::Chunk, b"abc")),
            format!("opaque \"\" {}", k.id(Kind::Opaque, b"")),
            format!("empty tree {} {}", hex::encode(empty_tree.encode()), k.id(Kind::Tree, &empty_tree.encode())),
            format!("tree {} {}", hex::encode(tree.encode()), k.id(Kind::Tree, &tree.encode())),
            format!("times of bin {} {}", hex::encode(bin_times.encode()), k.id(Kind::Times, &bin_times.encode())),
            format!("times {} {}", hex::encode(times.encode()), k.id(Kind::Times, &times.encode())),
            format!("epoch key {}", hex::encode(key)),
            format!("keyed blob \"hello\\n\" {}", Keying::Keyed(key).id(Kind::Blob, b"hello\n")),
        ];
        let got = lines.join("\n");
        println!("{got}");
        assert_eq!(got, VECTORS.trim());
    }

    const VECTORS: &str = "\
blob \"\" 473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813
blob \"hello\\n\" 2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4
chunk \"abc\" ada203fc3bdbffd43c38aae4e8af47e642b35cf6d7985e81a29483c746db3309
opaque \"\" 2b528cd00f2cb0427c1a0f8b5cc0c511032d28d1383fa0639ce3b13225c78132
empty tree 010000 463e15cd99aa5c788c292f8d1c5e510ac9c2b9f79817c06c2457f0dcf3931242
tree 010305612e74787401a403062cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb40106757365722e6b01760362696e02ed03463e15cd99aa5c788c292f8d1c5e510ac9c2b9f79817c06c2457f0dcf393124200046c696e6b030005612e7478740000 9e1e4e5228345fd99d1edcd9c3c6f946dcc323909dcdfbee7c6166869e1e41ae
times of bin 018280d0e2c6bfce972f00 2f2937d3648fcaca72b8935e42260a72ba52ad3c9c599114b98b5f663fd387f6
times 018080d0e2c6bfce972f030000012f2937d3648fcaca72b8935e42260a72ba52ad3c9c599114b98b5f663fd387f60000 87c7087f0a07a0665fa76ddf82b89fbdca680c02ee26dc0b42d6fa878940257d
epoch key 975c2d8e41c44ccf4aea50a2294bdd7b95ad5fab019182e8d24193e2951c398b
keyed blob \"hello\\n\" ed428789bac0bafb394f8234a3fce8cc1069cf68a7dede6d0338ef3033971447";

    #[test]
    fn a_hard_link_group_cannot_leave_the_folder() {
        let dir = scratch("hostile-links");
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x"), "victim").unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (blob, size) = store.put_cold_bytes(b"x", None).unwrap();
        let target = outside.as_os_str().as_encoded_bytes().to_vec();
        let mut tree = Tree {
            entries: vec![
                Entry { name: b"a".to_vec(), mode: 0o644, node: Node::File { size, blob }, xattrs: vec![] },
                Entry { name: b"l".to_vec(), mode: 0, node: Node::Symlink { target }, xattrs: vec![] },
            ],
            links: vec![vec![b"a".to_vec(), b"l/x".to_vec()]],
        };
        tree.normalize();
        let id = store.put_tree(&tree).unwrap();
        assert!(checkout(&store, &id, &dir.join("out")).is_err());
        assert_eq!(fs::read_to_string(outside.join("x")).unwrap(), "victim", "nothing was written through the link");
    }

    #[test]
    fn secret_ids_differ_from_plain_ids() {
        let dir = scratch("secret");
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), "x").unwrap();
        let opts = WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![], similar: false };
        let plain = Store::open(&dir.join("plain"), Keying::Plain).unwrap();
        let secret = Store::open(&dir.join("secret"), Keying::Keyed(id::epoch_key("AGE-SECRET-KEY-1X", "p"))).unwrap();
        let a = write_tree(&plain, &src, &opts).unwrap().tree.unwrap();
        let b = write_tree(&secret, &src, &opts).unwrap().tree.unwrap();
        assert_ne!(a, b);
        assert!(!secret.has(&Keying::Plain.id(Kind::Blob, b"x")), "a keyed store names no plain blob");
        checkout(&secret, &b, &dir.join("out")).unwrap();
        assert_eq!(fs::read(dir.join("out/f")).unwrap(), b"x");
    }
}
