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
        crate::source::Source::Objects(tree)
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

#[derive(Clone, Copy, Debug)]
enum HintMode {
    Complete,
    Partial,
    Failed,
}

struct FakeHints<'a> {
    base: &'a FakeSource<'a>,
    tree: &'a FakeSource<'a>,
    changed: &'a [String],
    mode: HintMode,
}
impl FakeHints<'_> {
    fn between(&self, base: &Path, tree: &Path) -> std::result::Result<layr_fs_core::hints::HintSet, layr_fs_core::FsError> {
        assert_eq!(self.base.folder(), Some(base));
        assert_eq!(self.tree.folder(), Some(tree));
        match self.mode {
            HintMode::Partial => Ok(layr_fs_core::hints::HintSet::Partial),
            HintMode::Failed => Err(layr_fs_core::FsError::Invalid("candidate enumeration failed")),
            HintMode::Complete => Ok(layr_fs_core::hints::HintSet::Complete(
                self.changed
                    .iter()
                    .filter_map(|path| {
                        EntryChange::observed(
                            path.clone(),
                            fsutil::lmeta(self.base, path),
                            fsutil::lmeta(self.tree, path),
                            DataEvidence::Unknown,
                        )
                    })
                    .collect(),
            )),
        }
    }
}

fn hint_budget(files: usize) -> Result<()> {
    let root = std::env::temp_dir().join(format!("layr-hints-{}", crate::ids::new_id()));
    let (base, tree) = (root.join("base"), root.join("tree"));
    std::fs::create_dir_all(&base)?;
    std::fs::create_dir_all(&tree)?;
    for n in 0..files {
        std::fs::write(base.join(n.to_string()), b"before")?;
        std::fs::write(tree.join(n.to_string()), b"before")?;
    }
    let (base, tree) = (FakeSource::new(&base), FakeSource::new(&tree));
    for dirty in [0, 10] {
        let paths: Vec<_> = (0..dirty).map(|n| n.to_string()).collect();
        for path in &paths {
            std::fs::write(tree.at(path)?, b"after!")?;
        }
        for mode in [HintMode::Complete, HintMode::Partial, HintMode::Failed] {
            *base.counts.lock().unwrap() = Counts::default();
            *tree.counts.lock().unwrap() = Counts::default();
            let hints = FakeHints { base: &base, tree: &tree, changed: &paths, mode };
            let candidates = candidates_with_hints(&base, &tree, Default::default(), None, |a, b| hints.between(a, b))?;
            let compared = base.counts.lock().unwrap().metadata.values().sum::<usize>()
                + tree.counts.lock().unwrap().metadata.values().sum::<usize>();
            let expected = match mode {
                HintMode::Complete => 2 * dirty,
                HintMode::Partial | HintMode::Failed => 2 * files,
            };
            // APFS's existing scanner bypasses Src instrumentation for full fallback.
            // Keep its correctness assertions below, and measure the instrumented paths.
            if matches!(mode, HintMode::Complete) || !cfg!(target_os = "macos") {
                assert_eq!(compared, expected, "{mode:?}, {dirty} dirty files");
            }
            let result = file_changes_of(&base, &tree, candidates)?;
            assert_eq!(result.len(), dirty, "{mode:?}");
            assert!(result.iter().all(|change| paths.contains(change.path())));
            eprintln!("hint budget: files={files} dirty={dirty} mode={mode:?} metadata={compared}");
        }
    }
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn complete_hints_stat_only_candidates_partial_and_failed_hints_scan_the_rest() -> Result<()> {
    hint_budget(64)
}

#[test]
#[ignore = "50k structural measurement; run explicitly on a native scratch volume"]
fn change_hints_50k_metadata_measurement() -> Result<()> {
    hint_budget(50_000)
}

#[cfg(not(target_os = "macos"))]
#[test]
fn fallback_preserves_ranges_renames_and_rejects_incomplete_observations() -> Result<()> {
    if crate::test_runs_as_root() {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new()?;
    let (base, tree) = (fixture.root.join("base"), fixture.root.join("tree"));
    for root in [&base, &tree] {
        std::fs::create_dir_all(root.join("private"))?;
        std::fs::write(root.join("private/file"), b"private")?;
        std::fs::write(root.join("file"), [0; 8192])?;
    }
    std::fs::write(base.join("old"), b"renamed")?;
    std::fs::hard_link(base.join("old"), tree.join("new"))?;
    let mut bytes = [0; 8192];
    bytes[4096] = 1;
    std::fs::write(tree.join("file"), bytes)?;
    let (base, tree) = (FakeSource::new(&base), FakeSource::new(&tree));
    for mode in [HintMode::Partial, HintMode::Failed] {
        let hints = FakeHints { base: &base, tree: &tree, changed: &[], mode };
        let list = candidates_with_hints(&base, &tree, Default::default(), None, |a, b| hints.between(a, b))?;
        assert!(list.iter().any(|change| matches!(change, EntryChange::Add { path, .. } if path == "new")));
        assert!(list.iter().any(|change| matches!(change, EntryChange::Remove { path, .. } if path == "old")));
        let inferred = with_renames(&base, &tree, list.clone());
        let renamed = inferred.iter().find(|change| change.path() == "new").unwrap();
        assert_eq!(renamed.old_path().map(String::as_str), Some("old"));
        let file = list.iter().find(|change| change.path() == "file").unwrap();
        assert_eq!(file.data(), &DataEvidence::from_native(&[(4096, 4096)], 8192, 8192).unwrap());
        // Neither one unreadable subtree nor two unreadable subtrees are a complete scan.
        for unreadable in [&[tree.folder().unwrap()][..], &[base.folder().unwrap(), tree.folder().unwrap()][..]] {
            for root in unreadable {
                std::fs::set_permissions(root.join("private"), std::fs::Permissions::from_mode(0o0))?;
            }
            let result = candidates_with_hints(&base, &tree, Default::default(), None, |a, b| hints.between(a, b));
            for root in unreadable {
                std::fs::set_permissions(root.join("private"), std::fs::Permissions::from_mode(0o755))?;
            }
            assert!(result.is_err(), "{mode:?}: failed observation must not become a successful delta");
        }
    }
    Ok(())
}

#[test]
fn selected_comparisons_do_not_visit_unrelated_paths() -> Result<()> {
    let fixture = Fixture::new()?;
    let old = object_source(fixture.snapshot()?);
    std::fs::write(fixture.native.join("edit/0"), b"edit")?;
    let new = object_source(fixture.snapshot()?);
    for source in [&fixture.native as &dyn Src, &new as &dyn Src] {
        let (parent, child) = (FakeSource::new(&old), FakeSource::new(source));
        let paths = ["edit/0".to_string()].into();
        assert!(!file_changes_of(&parent, &child, candidates_selected(&parent, &child, &paths)?)?.is_empty());
        for source in [&parent, &child] {
            let counts = source.counts.lock().map_err(|_| anyhow::anyhow!("comparison counts poisoned"))?;
            assert_eq!(counts.metadata.keys().map(String::as_str).collect::<Vec<_>>(), ["edit/0"]);
            assert_eq!(counts.metadata["edit/0"], 1);
            assert!(counts.opens <= 1, "{counts:?}");
        }
        assert!(
            file_changes_of(&parent, &child, candidates_selected(&parent, &child, &["keep/0".to_string()].into())?)?.is_empty()
        );
    }
    Ok(())
}

#[test]
fn selected_roots_cover_deletes_types_links_and_overlapping_directories() -> Result<()> {
    let fixture = Fixture::new()?;
    std::os::unix::fs::symlink("before", fixture.native.join("link"))?;
    let old = object_source(fixture.snapshot()?);
    std::fs::remove_file(fixture.native.join("edit/0"))?;
    std::fs::create_dir(fixture.native.join("edit/0"))?;
    std::fs::write(fixture.native.join("edit/0/new"), b"new")?;
    std::fs::remove_file(fixture.native.join("edit/1"))?;
    std::fs::rename(fixture.native.join("edit/2"), fixture.native.join("renamed"))?;
    std::fs::remove_file(fixture.native.join("link"))?;
    std::os::unix::fs::symlink("after", fixture.native.join("link"))?;
    let child = &fixture.native;
    for path in ["edit/0", "edit/1", "edit/2", "renamed", "link"] {
        assert!(
            !file_changes_of(&old, child, candidates_selected(&old, child, &[path.to_string()].into())?)?.is_empty(),
            "{path}"
        );
    }
    assert!(!any_file_change_selected(&old, child, &["link/child".to_string()].into(), |_| false)?);
    let paths = ["edit", "edit-elsewhere", "edit/0", "edit/0/new"].map(str::to_string).into();
    let source = FakeSource::new(child);
    let candidates = candidates_selected(&old, &source, &paths)?;
    assert!(file_changes_of(&old, &source, candidates)?.iter().any(|c| c.path() == "edit/1"));
    let counts = source.counts.lock().map_err(|_| anyhow::anyhow!("comparison counts poisoned"))?;
    assert!(counts.metadata.values().all(|count| *count == 1), "{counts:?}");
    assert!(!counts.metadata.contains_key("keep"), "{counts:?}");
    Ok(())
}

/// Reader adapters may wrap namespace failures with platform-specific diagnostic context.
struct FailingReader<F>(F);
impl<F: Fn() -> layr_fs_core::FsError + Send + Sync> layr_fs_core::TreeRead for FailingReader<F> {
    fn metadata(&self, _: &layr_fs_core::RelDir) -> layr_fs_core::Result<Option<layr_fs_core::EntryMetadata>> {
        Err((self.0)())
    }
    fn entries(
        &self,
        _: &layr_fs_core::RelDir,
        _: &mut dyn FnMut(&layr_fs_core::RelPath) -> layr_fs_core::Result<()>,
    ) -> layr_fs_core::Result<()> {
        Err(layr_fs_core::FsError::Invalid("unselected enumeration"))
    }
    fn open_file(&self, _: &layr_fs_core::RelPath) -> layr_fs_core::Result<Box<dyn layr_fs_core::ReadSeek + '_>> {
        Err(layr_fs_core::FsError::Invalid("unselected content read"))
    }
    fn read_link(&self, _: &layr_fs_core::RelPath) -> layr_fs_core::Result<Vec<u8>> {
        Err(layr_fs_core::FsError::Invalid("unselected link read"))
    }
}
impl<F: Fn() -> layr_fs_core::FsError + Send + Sync> Src for FailingReader<F> {
    fn core_reader(&self) -> Option<&dyn layr_fs_core::TreeRead> {
        Some(self)
    }
    fn at(&self, _: &str) -> Result<PathBuf> {
        anyhow::bail!("no materialization")
    }
    fn folder(&self) -> Option<&Path> {
        None
    }
}

#[test]
fn selected_comparisons_propagate_metadata_errors() -> Result<()> {
    use layr_fs_core::FsError;
    for errno in [libc::EACCES, libc::EIO] {
        for wrapped in [false, true] {
            let reader = FailingReader(|| {
                let error = FsError::Io(std::io::Error::from_raw_os_error(errno));
                if wrapped {
                    FsError::backend(anyhow::Error::new(error).context("metadata adapter"))
                } else {
                    error
                }
            });
            let paths = ["file".to_string()].into();
            let error = candidates_selected(&reader, &reader, &paths)
                .err()
                .ok_or_else(|| anyhow::anyhow!("metadata error was swallowed"))?;
            assert!(
                error
                    .chain()
                    .any(|cause| cause.downcast_ref::<std::io::Error>().is_some_and(|e| e.raw_os_error() == Some(errno))),
                "{error:#}"
            );
        }
    }
    Ok(())
}

#[test]
fn selected_comparisons_skip_wrapped_symlink_ancestors() -> Result<()> {
    assert_eq!(layr_fs_core::FsError::SymlinkAncestor.to_string(), "path goes through a symbolic link");
    let fixture = Fixture::new()?;
    let outside = fixture.root.join("outside");
    std::fs::create_dir(&outside)?;
    std::fs::write(outside.join("child"), b"outside the source namespace")?;
    std::os::unix::fs::symlink(&outside, fixture.native.join("link"))?;
    // APFS visibility performs safe_join before calling the native metadata reader.
    let reader = FailingReader(|| match crate::fsutil::safe_join(&fixture.native, "link/child") {
        Err(error) => layr_fs_core::FsError::backend(error.context("metadata adapter")),
        Ok(_) => layr_fs_core::FsError::Invalid("symlink ancestor was followed"),
    });
    assert!(crate::fsutil::lmeta_checked(&reader, "link/child")?.is_none());
    let paths = ["link/child".to_string()].into();
    assert!(!any_file_change_selected(&reader, &reader, &paths, |_| false)?);
    // The native source must behave identically, while a final symlink is still an entry.
    assert!(crate::fsutil::lmeta_checked(&fixture.native, "link/child")?.is_none());
    assert!(
        crate::fsutil::lmeta_checked(&fixture.native, "link")?.is_some_and(|meta| meta.ftype == crate::fsutil::FType::Symlink)
    );
    Ok(())
}

#[test]
fn a_selection_visits_each_entry_once_and_ends_at_symlinks() -> Result<()> {
    let fixture = Fixture::new()?;
    let (parent, child) = (fixture.root.join("parent"), fixture.root.join("child"));
    std::fs::create_dir_all(parent.join("dir"))?;
    std::fs::write(parent.join("dir/file"), b"old")?;
    std::fs::create_dir_all(child.join("a/b/c"))?;
    std::fs::write(child.join("a/b/c/file"), b"new")?;
    std::os::unix::fs::symlink("elsewhere", child.join("dir"))?;
    // `add` of a/b/c/file left its new folders racy: entries alone that exist on one side only.
    let selection = Selection {
        subtrees: ["a/b/c/file", "dir/file"].map(str::to_string).into(),
        entries: ["", "a", "a/b", "a/b/c"].map(str::to_string).into(),
    };
    let source = FakeSource::new(&child);
    let changes = file_changes_selected(&parent, &source, &selection)?;
    let paths: Vec<&str> = changes.iter().map(|c| c.path().as_str()).collect();
    assert_eq!(paths, ["a/b/c/file", "dir/file"], "a file below a symlink is removed from the stored namespace");
    assert!(matches!(changes[1], EntryChange::Remove { .. }));
    let counts = source.counts.lock().map_err(|_| anyhow::anyhow!("comparison counts poisoned"))?;
    // A folder found alone is read once more by its walk; nothing below it is visited twice.
    assert_eq!(counts.metadata.get("a/b/c/file"), Some(&1), "{counts:?}");
    assert!(counts.metadata.values().all(|count| *count <= 2), "{counts:?}");
    Ok(())
}
