//! Structural comparison budgets, with I/O counted by a fake source adapter.
use super::*;
use crate::fsutil::ComparisonIo;
use crate::object_tree::ObjectTree;
use layr_objects::{Node, StateRef, Store, Temperature, WriteOptions};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Default, Debug)]
struct Counts {
    metadata: BTreeMap<String, usize>,
    opens: usize,
    bytes_hashed: u64,
}
struct FakeSource<'a> {
    source: &'a dyn Src,
    counts: Mutex<Counts>,
}
impl<'a> FakeSource<'a> {
    fn new(source: &'a dyn Src) -> Self {
        Self { source, counts: Mutex::default() }
    }
}
impl Src for FakeSource<'_> {
    fn at(&self, rel: &str) -> Result<PathBuf> {
        anyhow::ensure!(self.source.objects().is_none(), "an object comparison must not materialize {rel}");
        self.source.at(rel)
    }
    fn folder(&self) -> Option<&Path> {
        self.source.folder()
    }
    fn objects(&self) -> Option<&ObjectTree> {
        self.source.objects()
    }
    #[cfg(target_os = "macos")]
    fn layered(&self) -> Option<&crate::thin::Layered> {
        self.source.layered()
    }
    fn comparison_io(&self, path: &str, io: ComparisonIo) {
        let mut counts = self.counts.lock().unwrap();
        match io {
            ComparisonIo::Metadata => *counts.metadata.entry(path.to_owned()).or_default() += 1,
            ComparisonIo::Open => counts.opens += 1,
            ComparisonIo::Hashed(bytes) => counts.bytes_hashed += bytes,
        }
    }
}
struct Fixture {
    root: PathBuf,
    native: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("layr-compare-budget-{}", crate::ids::new_id()));
        let native = root.join("native");
        for dir in ["keep", "edit"] {
            std::fs::create_dir_all(native.join(dir))?;
            for n in 0..8 {
                std::fs::write(native.join(format!("{dir}/{n}")), b"data")?;
            }
        }
        Ok(Self { root, native })
    }
    fn snapshot(&self) -> Result<Arc<ObjectTree>> {
        let store = Store::open(&self.root.join("objects"), layr_objects::Keying::Plain)?;
        let report = layr_objects::write_tree(
            &store,
            &self.native,
            &WriteOptions { temperature: Temperature::Hot, previous: None, exclude: Vec::new(), similar: false },
        )?;
        ObjectTree::new(store, StateRef { tree: report.tree.unwrap(), times: report.times.unwrap() }, &self.root.join("reads"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn object_source(tree: Arc<ObjectTree>) -> crate::repo::Snap {
    #[cfg(target_os = "macos")]
    {
        crate::thin::Layered::objects(tree)
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::source::Source::Objects(tree)
    }
}

#[test]
fn unchanged_native_entries_use_one_source_metadata_lookup() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = object_source(fixture.snapshot()?);
    let (parent, child) = (FakeSource::new(&source), FakeSource::new(&fixture.native));
    assert!(file_changes(&parent, &child)?.is_empty());
    for source in [&parent, &child] {
        let counts = source.counts.lock().unwrap();
        assert_eq!(counts.metadata.len(), 18);
        assert!(counts.metadata.values().all(|count| *count == 1), "{counts:?}");
    }
    let counts = child.counts.lock().unwrap();
    assert_eq!(counts.opens, 16, "mutable timestamps alone cannot prove equality");
    assert_eq!(counts.bytes_hashed, 16 * 4);
    Ok(())
}

#[test]
fn equal_immutable_roots_need_no_entries_or_blobs() -> Result<()> {
    let fixture = Fixture::new()?;
    let tree = fixture.snapshot()?;
    let source = object_source(tree.clone());
    let (parent, child) = (FakeSource::new(&source), FakeSource::new(&source));
    // A compare that accidentally opens a blob must fail, even when both sides use one store.
    let Node::File { blob, .. } = tree.entry("keep/0")?.unwrap().entry.node else { unreachable!() };
    std::fs::write(tree.store.hot_path(&blob).unwrap(), b"corrupt")?;
    assert!(file_changes(&parent, &child)?.is_empty());
    for source in [&parent, &child] {
        let counts = source.counts.lock().unwrap();
        assert!(counts.metadata.is_empty(), "{counts:?}");
        assert_eq!(counts.opens, 0);
        assert_eq!(counts.bytes_hashed, 0);
    }
    Ok(())
}

#[test]
fn equal_subtrees_prune_but_changed_times_and_attributes_remain_candidates() -> Result<()> {
    let fixture = Fixture::new()?;
    let old = object_source(fixture.snapshot()?);
    layr_objects::times::set(&fixture.native.join("edit/0"), 1_700_000_000_000_000_000)?;
    layr_objects::fsx::set_xattrs(&fixture.native.join("edit/1"), &[(b"user.test".to_vec(), b"changed".to_vec())])?;
    let new = object_source(fixture.snapshot()?);
    let (parent, child) = (FakeSource::new(&old), FakeSource::new(&new));
    let (changed, touched) = file_changes_and_touched(&parent, &child)?;
    assert_eq!(changed.iter().map(|c| c.path().as_str()).collect::<Vec<_>>(), ["edit/1"]);
    assert!(touched.iter().any(|path| path == "edit/0"));
    for source in [&parent, &child] {
        let counts = source.counts.lock().unwrap();
        assert!(!counts.metadata.keys().any(|path| path.starts_with("keep/")), "{counts:?}");
        assert!(counts.metadata.values().all(|count| *count == 1), "{counts:?}");
        assert_eq!(counts.opens, 0);
        assert_eq!(counts.bytes_hashed, 0);
    }
    Ok(())
}

#[test]
fn matching_link_maps_are_checked_once_across_many_directories() -> Result<()> {
    let fixture = Fixture::new()?;
    for n in 0..64 {
        let dir = fixture.native.join(format!("linked-{n}"));
        std::fs::create_dir(&dir)?;
        std::fs::write(dir.join("file"), b"linked")?;
        std::fs::hard_link(dir.join("file"), dir.join("alias"))?;
    }
    let old_tree = fixture.snapshot()?;
    let old = object_source(old_tree.clone());
    // Different times force the directory walk while preserving all 64 hard-link groups.
    layr_objects::times::set(&fixture.native.join("edit/0"), 1_700_000_000_000_000_000)?;
    let new = object_source(fixture.snapshot()?);
    let (parent, child) = (FakeSource::new(&old), FakeSource::new(&new));
    assert!(file_changes(&parent, &child)?.is_empty());
    assert_eq!(old_tree.link_comparisons.load(std::sync::atomic::Ordering::Relaxed), 1);
    for source in [&parent, &child] {
        let counts = source.counts.lock().unwrap();
        assert_eq!(counts.metadata.keys().filter(|path| path.starts_with("linked-")).count(), 64);
        assert!(!counts.metadata.keys().any(|path| path.starts_with("linked-") && path.contains('/')));
        assert_eq!(counts.opens, 0);
        assert_eq!(counts.bytes_hashed, 0);
    }
    Ok(())
}

#[test]
fn changed_hard_link_groups_disable_subtree_pruning() -> Result<()> {
    let fixture = Fixture::new()?;
    let old = object_source(fixture.snapshot()?);
    std::fs::hard_link(fixture.native.join("keep/0"), fixture.native.join("edit/link"))?;
    let new = object_source(fixture.snapshot()?);
    let (parent, child) = (FakeSource::new(&old), FakeSource::new(&new));
    let list = object_change_candidates(&parent, &child)?;
    assert!(list.iter().any(|c| c.path() == "keep/0"), "cross-directory link source remains a candidate");
    assert!(list.iter().any(|c| c.path() == "edit/link"));
    Ok(())
}

#[test]
fn observed_comparison_rejects_replacement_resize_and_still_hashes_restored_times() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = object_source(fixture.snapshot()?);
    let old = fsutil::lmeta(&source, "edit/0").unwrap();
    let observed = fsutil::lmeta(&fixture.native, "edit/0").unwrap();
    let compare = || fsutil::same_entry_observed((&source, "edit/0", &old), (&fixture.native, "edit/0", &observed), None);
    let path = fixture.native.join("edit/0");
    let metadata = std::fs::symlink_metadata(&path)?;
    let stamp = metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec();
    std::fs::write(&path, b"diff")?;
    layr_objects::times::set(&path, stamp)?;
    assert!(!compare()?, "a restored timestamp cannot hide changed bytes");
    std::fs::write(&path, b"longer")?;
    assert!(!compare()?, "descriptor length must agree with the observation");
    std::fs::remove_file(&path)?;
    std::os::unix::fs::symlink(fixture.native.join("keep/0"), &path)?;
    assert!(compare().is_err(), "a replacement link must never be followed");
    Ok(())
}
