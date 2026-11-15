//! The production writer/comparison/diff/merge algorithms run against both reference drivers.
use anyhow::{Context, Result};
use layr_fs_core::*;
use layr_fs_plain::{MemoryFs, MemoryTree, PlainFs};
use std::path::PathBuf;

use crate::fsutil::Src;

struct ReaderOnly<'a>(&'a dyn TreeRead);
impl Src for ReaderOnly<'_> {
    fn at(&self, _: &str) -> Result<PathBuf> {
        anyhow::bail!("core reader rejects native paths")
    }
    fn folder(&self) -> Option<&std::path::Path> {
        None
    }
    fn core_reader(&self) -> Option<&dyn TreeRead> {
        Some(self.0)
    }
}

fn attributes(mode: u32) -> EntryAttributes {
    EntryAttributes {
        mode,
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        modified_ns: 1_234_567_890,
        stored_time: Some(1_234_567_890),
        xattrs: Vec::new(),
    }
}
fn path(name: &str) -> RelPath {
    RelPath::new(name.as_bytes()).unwrap()
}
fn write(tree: &dyn TreeWrite, name: &str, bytes: &[u8]) -> Result<()> {
    tree.put(&path(name), EntryData::File(&mut &bytes[..]), &attributes(0o644))?;
    Ok(())
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("layr-fs-contract-{}", crate::ids::new_id()));
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&root)?;
        Ok(Self(root))
    }
    fn directory(&self, name: &str) -> Result<PathBuf> {
        let path = self.0.join(name);
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn capture(
    fs: &dyn Filesystem,
    location: &dyn Fn(&str) -> layr_fs_core::Result<NewTreeLocation>,
    source: &dyn TreeRead,
    name: &str,
) -> Result<Capture> {
    let CreatedTree { tree, ownership } = fs.materialise(source, location(&format!("{name}-work"))?)?;
    let capture = fs.freeze(&tree, location(name)?)?;
    drop(tree);
    fs.remove(ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(capture)
}
fn save(store: &layr_objects::Store, tree: &dyn TreeRead) -> Result<(layr_objects::Id, layr_objects::Id)> {
    let report = layr_objects::write::write_tree_from(
        store,
        &layr_objects::write::ReaderSource(tree),
        &layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        },
    )?;
    Ok((report.tree.context("tree absent")?, report.times.context("times absent")?))
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    base: (layr_objects::Id, layr_objects::Id),
    merged: (layr_objects::Id, layr_objects::Id),
    changed: Vec<String>,
    patch: String,
    conflicts: Vec<String>,
    bytes: Vec<u8>,
}
#[derive(Clone, Copy)]
enum ExpectedCapabilities {
    Empty,
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    Accelerated,
}
fn scenarios(
    fs: &dyn Filesystem,
    location: &dyn Fn(&str) -> layr_fs_core::Result<NewTreeLocation>,
    scratch: &Scratch,
    name: &str,
    expected_capabilities: ExpectedCapabilities,
    prepare_removal: &dyn Fn(&str) -> Result<()>,
) -> Result<Observed> {
    let seed = MemoryTree::new(attributes(0o755));
    write(&seed, "text", b"one\ntwo\nthree\n")?;
    write(&seed, "old", b"delete me\n")?;
    seed.put(&path("dir"), EntryData::Directory, &attributes(0o755))?;
    write(&seed, "dir/link-a", b"linked\n")?;
    seed.put(&path("link-b"), EntryData::HardLink(&path("dir/link-a")), &attributes(0o644))?;
    seed.put(&path("sym"), EntryData::Symlink(b"text"), &attributes(0o777))?;
    let base = capture(fs, location, &seed, "base")?;
    let caps = fs.capabilities(&base.tree.scope());
    let Capabilities { change_hints, clone_new, reflink_range } = caps;
    match expected_capabilities {
        ExpectedCapabilities::Empty => assert!(change_hints.is_none() && clone_new.is_none() && reflink_range.is_none()),
        #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
        ExpectedCapabilities::Accelerated => assert!(clone_new.is_some() && reflink_range.is_some()),
    }
    assert!(fs.changes(&base.tree, &base.tree)?.iter().all(|c| c.old().is_some() && c.new().is_some()));
    let CreatedTree { tree: ours_work, ownership: ours_owner } = fs.materialise(base.tree.reader(), location("ours-work")?)?;
    write(ours_work.writer(), "text", b"one\nOURS\nthree\n")?;
    ours_work.writer().remove_entry(&path("old"))?;
    let ours = fs.freeze(&ours_work, location("ours")?)?;
    drop(ours_work);
    // Source remains independent of subsequent versions; mtime-preserving edits are visible.
    let CreatedTree { tree: theirs_work, ownership: theirs_owner } =
        fs.materialise(base.tree.reader(), location("theirs-work")?)?;
    write(theirs_work.writer(), "text", b"one\nTHEIRS\nthree\n")?;
    write(theirs_work.writer(), "added", b"new\n")?;
    let theirs = fs.freeze(&theirs_work, location("theirs")?)?;
    drop(theirs_work);
    let (b, o, t) = (ReaderOnly(base.tree.reader()), ReaderOnly(ours.tree.reader()), ReaderOnly(theirs.tree.reader()));
    assert!(crate::changes::file_changes_of(&b, &b, fs.changes(&base.tree, &base.tree)?)?.is_empty());
    let changed = crate::changes::file_changes_of(&b, &o, fs.changes(&base.tree, &ours.tree)?)?;
    assert_eq!(changed.iter().map(|c| c.path().as_str()).collect::<Vec<_>>(), ["old", "text"]);
    let diffs = crate::diff::file_diffs(&b, &o, &changed)?;
    let patch = diffs.iter().map(|diff| crate::diff::patch(diff, 3)).collect::<String>();
    let store = layr_objects::Store::open(&scratch.directory(&format!("{name}-objects"))?, layr_objects::Keying::Plain)?;
    let base_ids = save(&store, base.tree.reader())?;
    assert_eq!(store.get_tree(&base_ids.0)?.links, [vec![b"dir/link-a".to_vec(), b"link-b".to_vec()]]);
    // The same executor writes directly into the backend under test. Only external tools
    // export individual inputs to their private process workspace.
    let output = fs.materialise(ours.tree.reader(), location("merge-work")?)?;
    output.tree.writer().put(&path("merge-alias"), EntryData::HardLink(&path("text")), &attributes(0o644))?;
    let outcome = crate::merge::merge(&crate::merge::Input {
        base: &b,
        ours: &o,
        theirs: &t,
        result: &crate::write_source::WriteSrc::Core { tree: output.tree.writer(), modified_ns: attributes(0o644).modified_ns },
        base_id: "01a12290-0000-7000-8000-000000000001",
        ours_id: "01a12290-0000-7000-8000-000000000002",
        theirs_id: "01a12290-0000-7000-8000-000000000003",
        ours_label: "ours",
        theirs_label: "theirs",
        config: &|_| None,
        run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
        allow_root: false,
    })?;
    let root_attributes = attributes(0o755);
    // The native executor gives writes the current time. Normalize the explicit test clock.
    for file in ["text", "added"] {
        output.tree.writer().set_metadata(&RelDir::Path(path(file)), &attributes(0o644))?;
    }
    output.tree.writer().set_metadata(&RelDir::Root, &root_attributes)?;
    let merged = fs.freeze(&output.tree, location("merged")?)?;
    drop(output.tree);
    fs.remove(output.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    let merged_ids = save(&store, merged.tree.reader())?;
    let mut bytes = Vec::new();
    merged.tree.reader().open_file(&path("text"))?.read_to_end(&mut bytes)?;
    assert!(bytes.windows(b"<<<<<<<".len()).any(|part| part == b"<<<<<<<"));
    let mut alias = Vec::new();
    merged.tree.reader().open_file(&path("merge-alias"))?.read_to_end(&mut alias)?;
    assert_eq!(alias, bytes);
    assert!(merged.tree.reader().metadata(&RelDir::Path(path("added")))?.is_some());
    assert!(merged.tree.reader().metadata(&RelDir::Path(path("old")))?.is_none());
    // Known rollback cleanup has authority; readers must release their pins first.
    for (name, capture) in [("base", base), ("ours", ours), ("theirs", theirs), ("merged", merged)] {
        let Capture { tree, ownership, .. } = capture;
        drop(tree);
        prepare_removal(name)?;
        fs.remove(ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    }
    fs.remove(ours_owner).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    fs.remove(theirs_owner).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(Observed {
        base: base_ids,
        merged: merged_ids,
        changed: changed.iter().map(|c| c.path().clone()).collect(),
        patch,
        conflicts: outcome.conflicts.iter().map(|c| c.path.clone()).collect(),
        bytes,
    })
}

#[test]
fn plain_and_memory_run_production_save_status_diff_and_merge() -> Result<()> {
    let scratch = Scratch::new()?;
    let memory = MemoryFs::default();
    let expected =
        scenarios(&memory, &|name| memory.location(path(name)), &scratch, "memory", ExpectedCapabilities::Empty, &|_| Ok(()))?;
    let plain = PlainFs::open(&scratch.directory("plain")?)?;
    let actual =
        scenarios(&plain, &|name| plain.location(path(name)), &scratch, "plain", ExpectedCapabilities::Empty, &|_| Ok(()))?;
    assert_eq!(actual, expected);
    Ok(())
}

use std::sync::atomic::{AtomicUsize, Ordering};
#[derive(Clone, Copy, Debug)]
enum Disabled {
    None,
    Hints,
    Clone,
    Range,
    All,
    Unavailable,
}
struct Decorator<'a> {
    fs: &'a dyn Filesystem,
    disabled: Disabled,
    calls: [AtomicUsize; 3],
}
impl Filesystem for Decorator<'_> {
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
        self.fs.materialise(source, dst)
    }
    fn freeze(&self, source: &WorkingTree, dst: NewTreeLocation) -> layr_fs_core::Result<Capture> {
        self.fs.freeze(source, dst)
    }
    fn changes(&self, before: &FrozenTree, after: &FrozenTree) -> layr_fs_core::Result<Vec<evidence::EntryChange>> {
        layr_fs_common::changes(self, before, after)
    }
    fn remove(&self, owner: OwnedTree) -> std::result::Result<(), RemovalFailure> {
        self.fs.remove(owner)
    }
    fn capabilities(&self, _: &TreeScope) -> Capabilities<'_> {
        // Exhaustive construction registers conformance for every declared capability: adding
        // a capability requires extending this fixture and its independent disable cases.
        Capabilities {
            change_hints: (!matches!(self.disabled, Disabled::Hints | Disabled::All)).then_some(self as &dyn hints::ChangeHints),
            clone_new: (!matches!(self.disabled, Disabled::Clone | Disabled::All)).then_some(self as &dyn CloneToNew),
            reflink_range: (!matches!(self.disabled, Disabled::Range | Disabled::All)).then_some(self as &dyn ReflinkRange),
        }
    }
}
impl hints::ChangeHints for Decorator<'_> {
    fn between(&self, before: &FrozenTree, after: &FrozenTree) -> layr_fs_core::Result<hints::HintSet> {
        self.calls[0].fetch_add(1, Ordering::Relaxed);
        if matches!(self.disabled, Disabled::Unavailable) {
            return Ok(hints::HintSet::Partial);
        }
        Ok(hints::HintSet::Complete(layr_fs_common::candidates(before.reader(), after.reader())?))
    }
}
impl CloneToNew for Decorator<'_> {
    fn clone_new(
        &self,
        source: &FrozenTree,
        path: &RelPath,
        dst: &WorkingTree,
        target: &RelPath,
    ) -> layr_fs_core::Result<Attempt<()>> {
        self.calls[1].fetch_add(1, Ordering::Relaxed);
        if matches!(self.disabled, Disabled::Unavailable) {
            return Ok(Attempt::Unavailable(std::io::ErrorKind::Unsupported.into()));
        }
        let metadata = layr_fs_common::required(source.reader(), &RelDir::Path(path.clone()))?;
        dst.writer().put(target, EntryData::File(&mut source.reader().open_file(path)?), &metadata.attributes)?;
        Ok(Attempt::Applied(()))
    }
}
impl ReflinkRange for Decorator<'_> {
    fn clone_range(
        &self,
        source: &std::fs::File,
        range: ByteRange,
        dst: &std::fs::File,
        offset: u64,
    ) -> layr_fs_core::Result<Attempt<()>> {
        use std::io::{Read, Seek, SeekFrom, Write};
        self.calls[2].fetch_add(1, Ordering::Relaxed);
        if matches!(self.disabled, Disabled::Unavailable) {
            return Ok(Attempt::Unavailable(std::io::ErrorKind::Unsupported.into()));
        }
        let mut source = source;
        let mut dst = dst;
        source.seek(SeekFrom::Start(range.offset()))?;
        let mut bytes = vec![0; usize::try_from(range.length()).map_err(|_| FsError::Invalid("fixture range too large"))?];
        source.read_exact(&mut bytes)?;
        dst.seek(SeekFrom::Start(offset))?;
        dst.write_all(&bytes)?;
        Ok(Attempt::Applied(()))
    }
}
#[test]
fn capabilities_disabled_independently_match_full_fallback() -> Result<()> {
    let scratch = Scratch::new()?;
    let memory = MemoryFs::default();
    let source = MemoryTree::new(attributes(0o755));
    write(&source, "file", b"0123456789")?;
    let mut file_attributes = attributes(0o755);
    file_attributes.xattrs = vec![(b"user.contract".to_vec(), b"kept".to_vec())];
    source.set_metadata(&RelDir::Path(path("file")), &file_attributes)?;
    source.put(&path("alias"), EntryData::HardLink(&path("file")), &file_attributes)?;
    let base = capture(&memory, &|name| memory.location(path(name)), &source, "base")?;
    let store = layr_objects::Store::open(&scratch.directory("objects")?, layr_objects::Keying::Plain)?;
    let mut outputs = Vec::new();
    for (index, disabled) in
        [Disabled::All, Disabled::None, Disabled::Hints, Disabled::Clone, Disabled::Range, Disabled::Unavailable]
            .into_iter()
            .enumerate()
    {
        let fs = Decorator { fs: &memory, disabled, calls: std::array::from_fn(|_| AtomicUsize::new(0)) };
        let target = fs.materialise(base.tree.reader(), memory.location(path(&format!("work-{index}")))?)?;
        layr_fs_common::copy_file(&fs, &base.tree, &path("file"), &target.tree, &path("clone"))?;
        target.tree.writer().set_metadata(&RelDir::Root, &attributes(0o755))?;
        let after = fs.freeze(&target.tree, memory.location(path(&format!("after-{index}")))?)?;
        let changes = fs.changes(&base.tree, &after.tree)?;
        let a = ReaderOnly(base.tree.reader());
        let b = ReaderOnly(after.tree.reader());
        let changes = crate::changes::file_changes_of(&a, &b, changes)?;
        let source = scratch.0.join(format!("range-source-{index}"));
        let destination = scratch.0.join(format!("range-target-{index}"));
        std::fs::write(&source, b"0123456789")?;
        std::fs::write(&destination, b"abcdefghij")?;
        let src = std::fs::File::open(&source)?;
        let dst = std::fs::OpenOptions::new().write(true).open(&destination)?;
        layr_fs_common::copy_range(&fs, &after.tree.scope(), &src, ByteRange::new(2, 4)?, &dst, 1)?;
        outputs.push((
            save(&store, after.tree.reader())?,
            changes.iter().map(|c| c.path().clone()).collect::<Vec<_>>(),
            std::fs::read(destination)?,
        ));
        assert_eq!(fs.calls[0].load(Ordering::Relaxed), usize::from(!matches!(disabled, Disabled::Hints | Disabled::All)));
        assert_eq!(fs.calls[1].load(Ordering::Relaxed), usize::from(!matches!(disabled, Disabled::Clone | Disabled::All)));
        assert_eq!(fs.calls[2].load(Ordering::Relaxed), usize::from(!matches!(disabled, Disabled::Range | Disabled::All)));
    }
    assert!(outputs.iter().all(|output| output == &outputs[0]));
    assert_eq!(outputs[0].1, ["clone"]);
    assert_eq!(outputs[0].2, b"a2345fghij");
    Ok(())
}

#[test]
fn memory_publication_fault_keeps_source_and_destination_reusable() -> Result<()> {
    let memory = MemoryFs::default();
    let source = MemoryTree::new(attributes(0o755));
    write(&source, "file", b"old")?;
    let work = memory.materialise(&source, memory.location(path("work"))?)?;
    memory.fail_next_publication();
    assert!(memory.freeze(&work.tree, memory.location(path("saved"))?).is_err());
    let saved = memory.freeze(&work.tree, memory.location(path("saved"))?)?;
    write(work.tree.writer(), "file", b"new")?;
    let mut bytes = Vec::new();
    saved.tree.reader().open_file(&path("file"))?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"old");
    assert_eq!(saved.consistency, CaptureConsistency::AtomicPoint);
    assert!(!saved.consistency.running());
    Ok(())
}

#[test]
fn memory_capture_commit_faults_follow_catalog_resolution() -> Result<()> {
    use crate::repo::PublicationCleanup;
    #[derive(Clone, Copy)]
    enum CommitFault {
        Before,
        ReportedAfter,
        UnknownAfter,
    }
    for fault in [CommitFault::Before, CommitFault::ReportedAfter, CommitFault::UnknownAfter] {
        let scratch = Scratch::new()?;
        let store = crate::store::Store::open(&scratch.0.join("catalog-store"))?;
        let project_dir = store.project_dir("p")?;
        std::fs::create_dir_all(project_dir.join("meta/db"))?;
        std::fs::create_dir(project_dir.join("states"))?;
        let id = crate::ids::new_id();
        let project = crate::store::Project {
            lease: Default::default(),
            store: &store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite"))?,
        };
        let actor = crate::model::Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None };
        let mut record = crate::model::DraftRecord::new("project");
        record.data = serde_json::json!({"id":id,"name":"p","config":{"objects.enabled":true}});
        project.lock_maintenance()?.append(record, &actor)?;
        let memory = MemoryFs::default();
        let source = MemoryTree::new(attributes(0o755));
        write(&source, "file", b"prepared content")?;
        let capture = capture(&memory, &|name| memory.location(path(name)), &source, "prepared")?;
        let (tree, times) = save(&crate::objstore::open(&project)?, capture.tree.reader())?;
        let state = crate::ids::new_id();
        let mut record = crate::model::DraftRecord::new("state");
        record.states.push(serde_json::from_value(serde_json::json!({
            "id":state,"kind":"commit","subvol":"","time":1,
            "author":{"name":"test","email":"test@example.com"},
            "tree":tree.to_string(),"times":times.to_string(),"running":capture.consistency.running()
        }))?);
        // Commit injection belongs at the catalog boundary, never in MemoryFs's storage model.
        crate::db::FAIL_BEFORE_COMMIT.with(|flag| flag.set(matches!(fault, CommitFault::Before)));
        crate::db::FAIL_AFTER_COMMIT.with(|flag| flag.set(!matches!(fault, CommitFault::Before)));
        crate::db::FAIL_RECONCILE.with(|flag| flag.set(matches!(fault, CommitFault::UnknownAfter)));
        let result = project.lock_maintenance()?.append(record, &actor);
        let error = result.unwrap_err();
        let cleanup = PublicationCleanup::from_failure(&error);
        match fault {
            CommitFault::Before => assert!(matches!(cleanup, PublicationCleanup::Rollback)),
            CommitFault::ReportedAfter => assert!(matches!(cleanup, PublicationCleanup::Keep)),
            CommitFault::UnknownAfter => assert!(matches!(cleanup, PublicationCleanup::Recover)),
        }
        if matches!(cleanup, PublicationCleanup::Rollback) {
            drop(capture.tree);
            memory.remove(capture.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
            assert!(project.db.state(&state)?.is_none());
        } else {
            assert!(project.db.state(&state)?.is_some());
            let mut bytes = Vec::new();
            capture.tree.reader().open_file(&path("file"))?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"prepared content");
        }
    }
    Ok(())
}

#[test]
fn plain_copy_preserves_byte_names_attributes_sparse_data_and_safe_paths() -> Result<()> {
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
    let scratch = Scratch::new()?;
    let source = scratch.directory("source")?;
    let unusual = std::ffi::OsStr::from_bytes(b"name-\xff");
    std::fs::write(source.join(unusual), b"bytes")?;
    let sparse = std::fs::File::create(source.join("sparse"))?;
    sparse.set_len(1 << 20)?;
    std::fs::create_dir(source.join("readonly"))?;
    std::fs::write(source.join("readonly/file"), b"data")?;
    std::fs::set_permissions(source.join("readonly"), std::fs::Permissions::from_mode(0o500))?;
    rustix::fs::setxattr(source.join(unusual), "user.layr-test", b"attribute", rustix::fs::XattrFlags::empty())?;
    std::os::unix::fs::symlink(&scratch.0, source.join("escape"))?;
    let reader = layr_fs_plain::DirectoryTree::open(&source)?;
    assert!(reader.metadata(&RelDir::Path(path("escape/source"))).is_err());
    let plain = PlainFs::open(&scratch.directory("plain")?)?;
    let work = plain.materialise(&reader, plain.location(path("work"))?)?;
    let copy = plain.freeze(&work.tree, plain.location(path("copy"))?)?;
    assert_eq!(copy.consistency, CaptureConsistency::UnsettledCopy);
    assert!(copy.consistency.running());
    let memory = MemoryFs::default();
    let virtual_copy = capture(&memory, &|name| memory.location(path(name)), copy.tree.reader(), "virtual")?;
    let objects = layr_objects::Store::open(&scratch.directory("objects")?, layr_objects::Keying::Plain)?;
    assert_eq!(save(&objects, &reader)?, save(&objects, virtual_copy.tree.reader())?);
    assert_eq!(
        copy.tree.reader().metadata(&RelDir::Path(RelPath::new(b"name-\xff")?))?.context("entry absent")?.attributes.xattrs,
        [(b"user.layr-test".to_vec(), b"attribute".to_vec())]
    );
    // Legacy string-only comparison rejects bytes explicitly; object writing stays byte preserving.
    assert!(plain.changes(&copy.tree, &copy.tree).is_err());
    drop(copy.tree);
    plain.remove(copy.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    drop(work.tree);
    plain.remove(work.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(())
}

#[test]
fn failed_plain_copy_returns_retry_authority_when_cleanup_cannot_finish() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    struct FailingRead(MemoryTree);
    impl TreeRead for FailingRead {
        fn metadata(&self, path: &RelDir) -> layr_fs_core::Result<Option<EntryMetadata>> {
            self.0.metadata(path)
        }
        fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> layr_fs_core::Result<()>) -> layr_fs_core::Result<()> {
            self.0.entries(dir, visit)
        }
        fn open_file(&self, _: &RelPath) -> layr_fs_core::Result<Box<dyn ReadSeek + '_>> {
            Err(FsError::Invalid("injected read failure"))
        }
        fn read_link(&self, path: &RelPath) -> layr_fs_core::Result<Vec<u8>> {
            self.0.read_link(path)
        }
    }
    let scratch = Scratch::new()?;
    let root = scratch.directory("plain")?;
    let plain = PlainFs::open(&root)?;
    let seed = MemoryTree::new(attributes(0o755));
    seed.put(&path("a"), EntryData::Directory, &attributes(0))?;
    seed.set_metadata(&RelDir::Path(path("a")), &attributes(0))?;
    write(&seed, "z", b"unreadable")?;
    let error = plain.materialise(&FailingRead(seed), plain.location(path("failed"))?).err().context("copy should fail")?;
    let FsError::Cleanup { cause, failure } = error else { anyhow::bail!("cleanup failure must retain its authority") };
    assert!(matches!(*cause, FsError::Invalid("injected read failure")));
    std::fs::set_permissions(root.join("failed/a"), std::fs::Permissions::from_mode(0o700))?;
    plain.remove(failure.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    assert!(!root.join("failed").exists());
    Ok(())
}

#[test]
fn range_fallback_handles_shared_descriptors_and_overlap_without_moving_offsets() -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let scratch = Scratch::new()?;
    let memory = MemoryFs::default();
    let seed = MemoryTree::new(attributes(0o755));
    let frozen = capture(&memory, &|name| memory.location(path(name)), &seed, "scope")?;
    let name = scratch.0.join("ranges");
    for duplicated in [false, true] {
        for (start, length, destination) in [(0, 4, 6), (0, 8, 2), (2, 8, 0), (0, 131_072, 1)] {
            let original: Vec<u8> = (0..=255).cycle().take(150_000).collect();
            let mut expected = original.clone();
            expected.copy_within(start..start + length, destination);
            std::fs::write(&name, &original)?;
            let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&name)?;
            file.seek(SeekFrom::Start(3))?;
            let duplicate = file.try_clone()?;
            let target = if duplicated { &duplicate } else { &file };
            layr_fs_common::copy_range(
                &memory,
                &frozen.tree.scope(),
                &file,
                ByteRange::new(start as u64, length as u64)?,
                target,
                destination as u64,
            )?;
            assert_eq!(std::fs::read(&name)?, expected);
            assert_eq!(file.stream_position()?, 3);
        }
    }
    Ok(())
}

#[test]
fn plain_and_memory_directory_and_symlink_mutations_have_identical_attributes() -> Result<()> {
    fn mutate(tree: &dyn TreeWrite) -> Result<(EntryAttributes, EntryAttributes, Vec<u8>)> {
        let mut directory = attributes(0o555);
        directory.xattrs = vec![(b"user.old".to_vec(), b"value".to_vec())];
        tree.put(&path("dir"), EntryData::Directory, &directory)?;
        assert_eq!(tree.metadata(&RelDir::Path(path("dir")))?.context("directory absent")?.attributes, directory);
        let mut readonly = attributes(0o444);
        readonly.xattrs = vec![(b"user.old".to_vec(), b"value".to_vec())];
        tree.put(&path("readonly"), EntryData::File(&mut b"data".as_slice()), &readonly)?;
        readonly.xattrs = vec![(b"user.new".to_vec(), b"replacement".to_vec())];
        for name in ["readonly", "dir"] {
            let mut update = readonly.clone();
            if name == "dir" {
                update.mode = 0o555;
            }
            tree.set_metadata(&RelDir::Path(path(name)), &update)?;
            assert_eq!(tree.metadata(&RelDir::Path(path(name)))?.context("entry absent")?.attributes, update);
            update.xattrs.clear();
            tree.set_metadata(&RelDir::Path(path(name)), &update)?;
            assert_eq!(tree.metadata(&RelDir::Path(path(name)))?.context("entry absent")?.attributes, update);
        }
        assert!(tree.put(&path("readonly"), EntryData::Directory, &directory).is_err());
        let mut link = attributes(0o777);
        tree.put(&path("link"), EntryData::Symlink(b"first"), &link)?;
        tree.put(&path("link"), EntryData::Symlink(b"replacement"), &link)?;
        link.modified_ns += 123;
        link.stored_time = Some(link.modified_ns);
        tree.set_metadata(&RelDir::Path(path("link")), &link)?;
        assert!(tree.put(&path("link"), EntryData::Directory, &directory).is_err());
        Ok((
            tree.metadata(&RelDir::Path(path("dir")))?.context("directory absent")?.attributes,
            tree.metadata(&RelDir::Path(path("link")))?.context("link absent")?.attributes,
            tree.read_link(&path("link"))?,
        ))
    }
    let scratch = Scratch::new()?;
    let memory = MemoryTree::new(attributes(0o755));
    let expected = mutate(&memory)?;
    let plain = PlainFs::open(&scratch.directory("plain")?)?;
    let empty = MemoryTree::new(attributes(0o755));
    let work = plain.materialise(&empty, plain.location(path("work"))?)?;
    assert_eq!(mutate(work.tree.writer())?, expected);
    drop(work.tree);
    plain.remove(work.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(())
}

#[test]
fn binary_merge_decisions_read_core_streams_without_materialising_inputs() -> Result<()> {
    let memory = MemoryFs::default();
    let mut captures = Vec::new();
    for (name, edit) in [("base", None), ("ours", Some((0, b"OURS"))), ("theirs", Some((8192, b"THEM")))] {
        let seed = MemoryTree::new(attributes(0o755));
        write(&seed, ".layrattributes", b"disk merge=blocks\n")?;
        let mut bytes = vec![0; 16384];
        if let Some((offset, value)) = edit {
            bytes[offset..offset + 4].copy_from_slice(value);
        }
        write(&seed, "disk", &bytes)?;
        captures.push(capture(&memory, &|name| memory.location(path(name)), &seed, name)?);
    }
    let output = memory.materialise(captures[1].tree.reader(), memory.location(path("result"))?)?;
    output.tree.writer().put(&path("alias"), EntryData::HardLink(&path("disk")), &attributes(0o644))?;
    let merged = crate::merge::merge(&crate::merge::Input {
        base: &ReaderOnly(captures[0].tree.reader()),
        ours: &ReaderOnly(captures[1].tree.reader()),
        theirs: &ReaderOnly(captures[2].tree.reader()),
        result: &crate::write_source::WriteSrc::Core { tree: output.tree.writer(), modified_ns: 42 },
        base_id: "01a12290-0000-7000-8000-000000000001",
        ours_id: "01a12290-0000-7000-8000-000000000002",
        theirs_id: "01a12290-0000-7000-8000-000000000003",
        ours_label: "ours",
        theirs_label: "theirs",
        config: &|_| None,
        run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
        allow_root: false,
    })?;
    assert!(merged.conflicts.is_empty(), "{:?}", merged.conflicts);
    let mut bytes = Vec::new();
    output.tree.writer().open_file(&path("disk"))?.read_to_end(&mut bytes)?;
    let mut alias = Vec::new();
    output.tree.writer().open_file(&path("alias"))?.read_to_end(&mut alias)?;
    assert_eq!(alias, bytes);
    let disk = output.tree.writer().metadata(&RelDir::Path(path("disk")))?.unwrap();
    let alias = output.tree.writer().metadata(&RelDir::Path(path("alias")))?.unwrap();
    assert_eq!(disk.link, alias.link);
    assert_eq!(disk.attributes.modified_ns, 42);
    assert_eq!(&bytes[..4], b"OURS");
    assert_eq!(&bytes[8192..8196], b"THEM");
    Ok(())
}

#[test]
fn reader_only_binary_add_add_reports_conflict_and_copies_new_entries() -> Result<()> {
    let scratch = Scratch::new()?;
    let memory = MemoryFs::default();
    let mut captures = Vec::new();
    for (name, bytes) in [("base", None), ("ours", Some(b"ours\0".as_slice())), ("theirs", Some(b"them\0".as_slice()))] {
        let source = MemoryTree::new(attributes(0o755));
        if let Some(bytes) = bytes {
            write(&source, "binary", bytes)?;
        }
        if name == "theirs" {
            write(&source, "added", b"streamed addition")?;
            source.put(&path("symlink"), EntryData::Symlink(b"target-\xff"), &attributes(0o777))?;
        }
        captures.push(capture(&memory, &|name| memory.location(path(name)), &source, name)?);
    }
    let plain = PlainFs::open(&scratch.directory("workspace")?)?;
    let output = plain.materialise(captures[1].tree.reader(), plain.location(path("result"))?)?;
    let result = plain.working_path(&output.tree)?;
    let outcome = crate::merge::merge(&crate::merge::Input {
        base: &ReaderOnly(captures[0].tree.reader()),
        ours: &ReaderOnly(captures[1].tree.reader()),
        theirs: &ReaderOnly(captures[2].tree.reader()),
        result: &crate::write_source::WriteSrc::Native(&result),
        base_id: "01a12290-0000-7000-8000-000000000001",
        ours_id: "01a12290-0000-7000-8000-000000000002",
        theirs_id: "01a12290-0000-7000-8000-000000000003",
        ours_label: "ours",
        theirs_label: "theirs",
        config: &|_| None,
        run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
        allow_root: false,
    })?;
    assert_eq!(outcome.conflicts.len(), 1);
    assert!(matches!(
        &outcome.conflicts[0].details,
        crate::model::ConflictDetails::Known { kind: crate::model::ConflictKind::AddAdd, .. }
    ));
    assert_eq!(std::fs::read(result.join("binary"))?, b"ours\0");
    assert_eq!(std::fs::read(result.join("added"))?, b"streamed addition");
    use std::os::unix::ffi::OsStrExt;
    assert_eq!(std::fs::read_link(result.join("symlink"))?.as_os_str().as_bytes(), b"target-\xff");
    Ok(())
}

#[test]
fn mixed_core_native_and_object_comparisons_do_not_require_core_paths() -> Result<()> {
    struct Objects(std::sync::Arc<crate::object_tree::ObjectTree>);
    impl Src for Objects {
        fn at(&self, _: &str) -> Result<PathBuf> {
            anyhow::bail!("object comparison rejects native paths")
        }
        fn folder(&self) -> Option<&std::path::Path> {
            None
        }
        fn objects(&self) -> Option<&crate::object_tree::ObjectTree> {
            Some(&self.0)
        }
    }
    let scratch = Scratch::new()?;
    let memory = MemoryFs::default();
    let source = MemoryTree::new(attributes(0o755));
    write(&source, "file", b"same")?;
    let base = capture(&memory, &|name| memory.location(path(name)), &source, "base")?;
    write(&source, "file", b"edit")?;
    let edit = capture(&memory, &|name| memory.location(path(name)), &source, "edit")?;
    let plain = PlainFs::open(&scratch.directory("plain")?)?;
    let native = plain.materialise(base.tree.reader(), plain.location(path("native"))?)?;
    let native_path = plain.working_path(&native.tree)?;
    let object_dir = scratch.directory("objects")?;
    let store = layr_objects::Store::open(&object_dir, layr_objects::Keying::Plain)?;
    let (tree, times) = save(&store, base.tree.reader())?;
    let objects = Objects(crate::object_tree::ObjectTree::new(
        store,
        layr_objects::StateRef { tree, times },
        &scratch.directory("reads")?,
    )?);
    for other in [&native_path as &dyn Src, &objects as &dyn Src] {
        assert!(crate::changes::file_changes(&ReaderOnly(base.tree.reader()), other)?.is_empty());
        assert!(crate::changes::file_changes(other, &ReaderOnly(base.tree.reader()))?.is_empty());
        for changes in [
            crate::changes::file_changes(&ReaderOnly(edit.tree.reader()), other)?,
            crate::changes::file_changes(other, &ReaderOnly(edit.tree.reader()))?,
        ] {
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].path(), "file");
        }
    }
    Ok(())
}

#[test]
fn byte_distinct_symlink_targets_are_never_equal_after_lossy_decoding() -> Result<()> {
    let memory = MemoryFs::default();
    let source = MemoryTree::new(attributes(0o755));
    source.put(&path("link"), EntryData::Symlink(b"\xfe"), &attributes(0o777))?;
    let before = capture(&memory, &|name| memory.location(path(name)), &source, "before")?;
    source.put(&path("link"), EntryData::Symlink(b"\xff"), &attributes(0o777))?;
    let after = capture(&memory, &|name| memory.location(path(name)), &source, "after")?;
    let changes = crate::changes::file_changes(&ReaderOnly(before.tree.reader()), &ReaderOnly(after.tree.reader()))?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path(), "link");
    Ok(())
}

#[test]
fn writable_trees_preserve_link_groups_moves_and_empty_directory_checks() -> Result<()> {
    fn check(tree: &dyn TreeWrite) -> Result<()> {
        tree.put(&path("dir"), EntryData::Directory, &attributes(0o755))?;
        write(tree, "dir/file", b"old")?;
        tree.put(&path("alias"), EntryData::HardLink(&path("dir/file")), &attributes(0o644))?;
        assert!(!tree.remove_empty_dir(&path("dir"))?);
        tree.rename_new(&path("dir"), &path("moved"))?;
        assert!(tree.metadata(&RelDir::Path(path("dir")))?.is_none());
        assert!(matches!(tree.overwrite_file(&path("moved/file"), &mut &b"updated"[..], 123)?, Attempt::Applied(())));
        tree.write_range(&path("alias"), 2, b"XY", 456)?;
        assert!(tree.write_range(&path("alias"), 8, b"bad", 789).is_err());
        for name in ["moved/file", "alias"] {
            let mut bytes = Vec::new();
            tree.open_file(&path(name))?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"upXYted");
            let metadata = tree.metadata(&RelDir::Path(path(name)))?.unwrap();
            assert_eq!(metadata.attributes.modified_ns, 456);
            assert_eq!(metadata.link.unwrap().count, 2);
        }
        let a = tree.metadata(&RelDir::Path(path("moved/file")))?.unwrap().link;
        assert_eq!(a, tree.metadata(&RelDir::Path(path("alias")))?.unwrap().link);
        tree.remove_entry(&path("moved/file"))?;
        assert!(tree.remove_empty_dir(&path("moved"))?);
        Ok(())
    }
    let scratch = Scratch::new()?;
    check(&MemoryTree::new(attributes(0o755)))?;
    check(&layr_fs_plain::DirectoryTree::open(&scratch.directory("native")?)?)?;
    Ok(())
}

#[test]
fn direct_outputs_share_derive_verification_mode_and_rename_decisions() -> Result<()> {
    fn run(
        fs: &dyn Filesystem,
        location: &dyn Fn(&str) -> layr_fs_core::Result<NewTreeLocation>,
        scratch: &Scratch,
        tag: &str,
    ) -> Result<(layr_objects::Id, Vec<String>)> {
        let mut captures = Vec::new();
        for name in ["base", "ours", "theirs"] {
            let seed = MemoryTree::new(attributes(0o755));
            write(
                &seed,
                ".layrattributes",
                b"disk merge=blocks merge-verify=reject\ngenerated derive=first\nsecond derive=second\n",
            )?;
            write(&seed, "source", if name == "ours" { b"OURS\n" } else { b"base\n" })?;
            write(&seed, if name == "theirs" { "renamed" } else { "move" }, b"rename me")?;
            write(&seed, "mode", if name == "theirs" { b"changed" } else { b"original" })?;
            if name == "ours" {
                seed.set_metadata(&RelDir::Path(path("mode")), &attributes(0o755))?;
            }
            let mut bytes = vec![0; 16384];
            if name == "ours" {
                bytes[..4].copy_from_slice(b"OURS");
            }
            if name == "theirs" {
                bytes[8192..8196].copy_from_slice(b"THEM");
            }
            write(&seed, "disk", &bytes)?;
            captures.push(capture(fs, location, &seed, name)?);
        }
        let output = fs.materialise(captures[1].tree.reader(), location("output")?)?;
        for (from, to) in [("disk", "disk-alias"), ("move", "move-alias")] {
            output.tree.writer().put(&path(to), EntryData::HardLink(&path(from)), &attributes(0o644))?;
        }
        let sources: Vec<_> = captures.iter().map(|c| ReaderOnly(c.tree.reader())).collect();
        let config = |key: &str| match key {
            "merge.verify.reject" => Some("exit 1".into()),
            "derive.inputs.first" => Some("source".into()),
            "derive.command.first" => Some("cat source > generated; echo untrusted > source".into()),
            "derive.inputs.second" => Some("generated".into()),
            "derive.command.second" => Some("cat generated > second".into()),
            _ => None,
        };
        let result = crate::write_source::WriteSrc::Core { tree: output.tree.writer(), modified_ns: 42 };
        let outcome = crate::merge::merge(&crate::merge::Input {
            base: &sources[0],
            ours: &sources[1],
            theirs: &sources[2],
            result: &result,
            base_id: "01a12290-0000-7000-8000-000000000001",
            ours_id: "01a12290-0000-7000-8000-000000000002",
            theirs_id: "01a12290-0000-7000-8000-000000000003",
            ours_label: "ours",
            theirs_label: "theirs",
            config: &config,
            run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
            allow_root: false,
        })?;
        assert_eq!(
            outcome.conflicts.iter().map(|c| (c.path.as_str(), c.kind())).collect::<Vec<_>>(),
            [("disk", std::borrow::Cow::Borrowed("verify reject failed"))]
        );
        for file in ["source", "generated", "second"] {
            assert_eq!(crate::fsutil::read_file(&result, file)?, b"OURS\n");
        }
        assert_eq!(crate::fsutil::read_file(&result, "mode")?, b"changed");
        assert_eq!(output.tree.writer().metadata(&RelDir::Path(path("mode")))?.unwrap().attributes.mode, 0o755);
        assert!(output.tree.writer().metadata(&RelDir::Path(path("move")))?.is_none());
        for (a, b) in [("renamed", "move-alias")] {
            assert_eq!(crate::fsutil::read_file(&result, a)?, crate::fsutil::read_file(&result, b)?);
            assert_eq!(
                output.tree.writer().metadata(&RelDir::Path(path(a)))?.unwrap().link,
                output.tree.writer().metadata(&RelDir::Path(path(b)))?.unwrap().link
            );
        }
        assert_eq!(&crate::fsutil::read_file(&result, "disk")?[8192..8196], &[0; 4]);
        assert_eq!(&crate::fsutil::read_file(&result, "disk-alias")?[8192..8196], b"THEM");
        assert_ne!(
            output.tree.writer().metadata(&RelDir::Path(path("disk")))?.unwrap().link,
            output.tree.writer().metadata(&RelDir::Path(path("disk-alias")))?.unwrap().link
        );
        layr_fs_common::walk(output.tree.writer(), &RelDir::Root, &mut |path, meta| {
            let mut attr = meta.attributes.clone();
            attr.modified_ns = 1234;
            attr.stored_time = Some(1234);
            output.tree.writer().set_metadata(&RelDir::Path(path.clone()), &attr)?;
            Ok(true)
        })?;
        output.tree.writer().set_metadata(&RelDir::Root, &attributes(0o755))?;
        let store = layr_objects::Store::open(&scratch.directory(tag)?, layr_objects::Keying::Plain)?;
        let id = save(&store, output.tree.writer())?.0;
        Ok((id, outcome.conflicts.iter().map(|c| c.path.clone()).collect()))
    }
    let scratch = Scratch::new()?;
    let memory = MemoryFs::default();
    let expected = run(&memory, &|name| memory.location(path(name)), &scratch, "memory-objects")?;
    let plain = PlainFs::open(&scratch.directory("plain")?)?;
    assert_eq!(run(&plain, &|name| plain.location(path(name)), &scratch, "plain-objects")?, expected);
    Ok(())
}

#[test]
fn copy_replacement_and_inplace_policies_match_native_output() -> Result<()> {
    use crate::write_source::WriteSrc;
    fn seed(tree: &dyn TreeWrite) -> Result<()> {
        for (name, alias) in [("a", "b"), ("out", "out-alias"), ("git", "git-alias")] {
            let mut attrs = attributes(0o644);
            attrs.xattrs = vec![(b"user.old".to_vec(), b"old".to_vec())];
            tree.put(&path(name), EntryData::File(&mut b"ours".as_slice()), &attrs)?;
            tree.put(&path(alias), EntryData::HardLink(&path(name)), &attrs)?;
        }
        tree.put(&path("readonly"), EntryData::File(&mut b"old".as_slice()), &attributes(0o444))?;
        tree.put(&path("directory"), EntryData::Directory, &attributes(0o755))?;
        write(tree, "directory/child", b"old")?;
        Ok(())
    }
    fn apply(
        output: &WriteSrc,
        source: &dyn Src,
        reader: &dyn TreeWrite,
    ) -> Result<Vec<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>, u32, u64)>> {
        output.copy_from(source, "file", "a")?;
        output.copy_from(source, "file", "directory")?;
        output.copy_from(source, "file", "readonly")?;
        output.write_from("out", &mut b"replacement".as_slice(), 0o444)?;
        output.overwrite("git", &mut b"inplace".as_slice())?;
        let mut result = Vec::new();
        for name in ["a", "b", "directory", "out", "out-alias", "git", "git-alias", "readonly"] {
            let bytes = crate::fsutil::read_file(output, name)?;
            let metadata = reader.metadata(&RelDir::Path(path(name)))?.unwrap();
            result.push((bytes, metadata.attributes.xattrs, metadata.attributes.mode, metadata.link.unwrap().count));
        }
        assert_eq!(result[0].0, b"theirs");
        assert_eq!(result[1].0, b"ours");
        assert_eq!(result[3].0, b"replacement");
        assert!(result[3].1.is_empty());
        assert_eq!(result[4].0, b"ours");
        assert_eq!(result[5].0, b"inplace");
        assert_eq!(result[6].0, b"inplace");
        assert_eq!(result[5].3, 2);
        assert_eq!(result[7].0, b"theirs");
        Ok(result)
    }
    let scratch = Scratch::new()?;
    let source_path = scratch.directory("source")?;
    let source = layr_fs_plain::DirectoryTree::open(&source_path)?;
    let mut attrs = attributes(0o644);
    attrs.xattrs = vec![(b"user.source".to_vec(), b"new".to_vec())];
    source.put(&path("file"), EntryData::File(&mut b"theirs".as_slice()), &attrs)?;
    let native_path = scratch.directory("native")?;
    let native = layr_fs_plain::DirectoryTree::open(&native_path)?;
    seed(&native)?;
    let expected = apply(&WriteSrc::Native(&native_path), &source_path, &native)?;
    let memory = MemoryTree::new(attributes(0o755));
    seed(&memory)?;
    assert_eq!(apply(&WriteSrc::Core { tree: &memory, modified_ns: 42 }, &source_path, &memory)?, expected);
    let plain_path = scratch.directory("plain")?;
    let plain = layr_fs_plain::DirectoryTree::open(&plain_path)?;
    seed(&plain)?;
    assert_eq!(apply(&WriteSrc::Core { tree: &plain, modified_ns: 42 }, &source_path, &plain)?, expected);
    let core_input_path = scratch.directory("core-input")?;
    let core_input = layr_fs_plain::DirectoryTree::open(&core_input_path)?;
    seed(&core_input)?;
    assert_eq!(apply(&WriteSrc::Native(&core_input_path), &ReaderOnly(&source), &core_input)?, expected);
    Ok(())
}

#[test]
fn directory_type_replacements_match_native_merge_conflicts_and_results() -> Result<()> {
    use crate::write_source::WriteSrc;
    let scratch = Scratch::new()?;
    for ours_changed in [false, true] {
        let base = MemoryTree::new(attributes(0o755));
        base.put(&path("d"), EntryData::Directory, &attributes(0o755))?;
        write(&base, "d/child", b"unchanged")?;
        let ours = MemoryTree::new(attributes(0o755));
        let theirs = MemoryTree::new(attributes(0o755));
        if ours_changed {
            write(&ours, "d", b"ours")?;
        } else {
            layr_fs_common::copy_tree(&base, &ours)?;
            write(&theirs, "d", b"theirs")?;
        }
        let native_path = scratch.directory(if ours_changed { "changed" } else { "unchanged" })?;
        let native = layr_fs_plain::DirectoryTree::open(&native_path)?;
        layr_fs_common::copy_tree(&ours, &native)?;
        let memory = MemoryTree::new(attributes(0o755));
        layr_fs_common::copy_tree(&ours, &memory)?;
        let mut observed = Vec::new();
        for output in [WriteSrc::Native(&native_path), WriteSrc::Core { tree: &memory, modified_ns: 42 }] {
            let outcome = crate::merge::merge(&crate::merge::Input {
                base: &ReaderOnly(&base),
                ours: &ReaderOnly(&ours),
                theirs: &ReaderOnly(&theirs),
                result: &output,
                base_id: "01a12290-0000-7000-8000-000000000001",
                ours_id: "01a12290-0000-7000-8000-000000000002",
                theirs_id: "01a12290-0000-7000-8000-000000000003",
                ours_label: "ours",
                theirs_label: "theirs",
                config: &|_| None,
                run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
                allow_root: false,
            })?;
            observed.push((
                outcome.conflicts.iter().map(|c| (c.path.clone(), c.kind().into_owned())).collect::<Vec<_>>(),
                crate::fsutil::read_file(&output, "d")?,
            ));
        }
        assert_eq!(observed[0], observed[1]);
        assert_eq!(observed[0].0.len(), usize::from(ours_changed));
    }
    Ok(())
}

#[test]
fn readonly_custom_driver_inputs_export_writable_scratch_and_replace_xattrs() -> Result<()> {
    use crate::write_source::WriteSrc;
    let scratch = Scratch::new()?;
    let mut sources = Vec::new();
    for bytes in [b"base\n".as_slice(), b"ours\n", b"theirs\n"] {
        let tree = MemoryTree::new(attributes(0o755));
        write(&tree, ".layrattributes", b"file merge=replace\n")?;
        let mut attr = attributes(0o444);
        attr.xattrs = vec![(b"user.old".to_vec(), b"discard".to_vec())];
        tree.put(&path("file"), EntryData::File(&mut &bytes[..]), &attr)?;
        sources.push(tree);
    }
    let native_path = scratch.directory("native")?;
    let native = layr_fs_plain::DirectoryTree::open(&native_path)?;
    layr_fs_common::copy_tree(&sources[1], &native)?;
    let memory = MemoryTree::new(attributes(0o755));
    layr_fs_common::copy_tree(&sources[1], &memory)?;
    for (output, reader) in [
        (WriteSrc::Native(&native_path), &native as &dyn TreeRead),
        (WriteSrc::Core { tree: &memory, modified_ns: 42 }, &memory as &dyn TreeRead),
    ] {
        let outcome = crate::merge::merge(&crate::merge::Input {
            base: &ReaderOnly(&sources[0]),
            ours: &ReaderOnly(&sources[1]),
            theirs: &ReaderOnly(&sources[2]),
            result: &output,
            base_id: "01a12290-0000-7000-8000-000000000001",
            ours_id: "01a12290-0000-7000-8000-000000000002",
            theirs_id: "01a12290-0000-7000-8000-000000000003",
            ours_label: "ours",
            theirs_label: "theirs",
            config: &|key| (key == "merge.driver.replace").then(|| "cat %B > %A".into()),
            run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
            allow_root: false,
        })?;
        assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
        assert_eq!(crate::fsutil::read_file(&output, "file")?, b"theirs\n");
        let metadata = reader.metadata(&RelDir::Path(path("file")))?.unwrap();
        assert_eq!(metadata.attributes.mode, 0o444);
        assert!(metadata.attributes.xattrs.is_empty());
    }
    Ok(())
}

#[test]
fn plain_backend_requires_a_private_owned_namespace() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new()?;
    let root = scratch.directory("frame")?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777))?;
    assert!(matches!(PlainFs::open(&root), Err(FsError::Invalid(_))));
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    PlainFs::open(&root)?;
    Ok(())
}

#[test]
fn overwrite_availability_never_masks_errors_after_mutation() -> Result<()> {
    struct FailingInput {
        reads: usize,
    }
    impl std::io::Read for FailingInput {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.reads == 0 && !bytes.is_empty() {
                bytes[0] = b'n';
                self.reads += 1;
                return Ok(1);
            }
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "source fails after first byte"))
        }
    }
    let scratch = Scratch::new()?;
    let tree = layr_fs_plain::DirectoryTree::open(&scratch.directory("native")?)?;
    tree.put(&path("file"), EntryData::File(&mut b"old".as_slice()), &attributes(0o444))?;
    let mut source = FailingInput { reads: 0 };
    assert!(
        matches!(tree.overwrite_file(&path("file"), &mut source, 123)?, Attempt::Unavailable(error) if error.kind() == std::io::ErrorKind::PermissionDenied)
    );
    assert_eq!(source.reads, 0);
    let mut bytes = Vec::new();
    tree.open_file(&path("file"))?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"old");
    tree.set_metadata(&RelDir::Path(path("file")), &attributes(0o644))?;
    assert!(
        matches!(tree.overwrite_file(&path("file"), &mut source, 123), Err(FsError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
    );
    let mut bytes = Vec::new();
    tree.open_file(&path("file"))?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"n");
    Ok(())
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[test]
fn btrfs_core_operations_match_reference_results_and_capabilities() -> Result<()> {
    let scratch = Scratch::new()?;
    if !crate::storage::is_supported(&scratch.0) {
        eprintln!("btrfs conformance unavailable: TMPDIR is not btrfs");
        return Ok(());
    }
    let memory = MemoryFs::default();
    let expected =
        scenarios(&memory, &|name| memory.location(path(name)), &scratch, "reference", ExpectedCapabilities::Empty, &|_| Ok(()))?;
    let native = crate::btrfs_driver::BtrfsFs::new(std::fs::File::open(&scratch.0)?)?;
    let root = scratch.directory("native")?;
    let actual = scenarios(
        &native,
        &|name| native.location(&root.join(name)),
        &scratch,
        "btrfs",
        ExpectedCapabilities::Accelerated,
        &|name| crate::storage::set_writable(&root.join(name)),
    )?;
    assert_eq!(actual, expected);

    let seed = MemoryTree::new(attributes(0o755));
    write(&seed, "file", b"cloned")?;
    let base = capture(&native, &|name| native.location(&root.join(name)), &seed, "clone-base")?;
    let work = native.materialise(base.tree.reader(), native.location(&root.join("clone-work"))?)?;
    layr_fs_common::copy_file(&native, &base.tree, &path("file"), &work.tree, &path("copy"))?;
    let mut cloned = Vec::new();
    work.tree.reader().open_file(&path("copy"))?.read_to_end(&mut cloned)?;
    assert_eq!(cloned, b"cloned");
    assert_eq!(
        work.tree.reader().metadata(&RelDir::Path(path("file")))?.context("source absent")?.attributes,
        work.tree.reader().metadata(&RelDir::Path(path("copy")))?.context("clone absent")?.attributes
    );
    let source_caps = native.capabilities(&base.tree.scope());
    assert_eq!(
        source_caps.change_hints.is_some(),
        rustix::thread::capabilities(None)?.effective.contains(rustix::thread::CapabilitySet::SYS_ADMIN)
    );
    assert!(matches!(
        source_caps.clone_new.context("clone capability absent")?.clone_new(
            &base.tree,
            &path("file"),
            &work.tree,
            &path("direct")
        )?,
        Attempt::Applied(())
    ));
    let memory_source = capture(&memory, &|name| memory.location(path(name)), &seed, "foreign")?;
    layr_fs_common::copy_file(&native, &memory_source.tree, &path("file"), &work.tree, &path("foreign"))?;
    let mut copied = Vec::new();
    work.tree.reader().open_file(&path("foreign"))?.read_to_end(&mut copied)?;
    assert_eq!(copied, b"cloned");
    let caps = native.capabilities(&work.tree.scope());
    assert!(caps.change_hints.is_none(), "live trees cannot advertise immutable change evidence");
    let scope = base.tree.scope();
    let failed = native.remove(base.ownership).unwrap_err();
    assert!(matches!(failed.error, FsError::Busy));
    drop(base.tree);
    let failed = native.remove(failed.ownership).unwrap_err();
    assert!(matches!(failed.error, FsError::Busy), "scope retains its tree pin");
    drop(scope);
    // Normal-user deletion requires a writable subvolume; prepare only this owned fixture.
    crate::storage::set_writable(&root.join("clone-base"))?;
    native.remove(failed.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    let source_path = scratch.0.join("range-source");
    let target_path = scratch.0.join("range-target");
    std::fs::write(&source_path, [vec![b'a'; 4096], vec![b'b'; 4096]].concat())?;
    std::fs::write(&target_path, vec![b'y'; 8192])?;
    let source = std::fs::File::open(source_path)?;
    let target = std::fs::OpenOptions::new().read(true).write(true).open(&target_path)?;
    assert!(matches!(
        caps.reflink_range.context("range capability absent")?.clone_range(&source, ByteRange::new(4096, 4096)?, &target, 0)?,
        Attempt::Applied(())
    ));
    assert_eq!(&std::fs::read(target_path)?[..4096], vec![b'b'; 4096]);
    drop(work.tree);
    native.remove(work.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(())
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[test]
fn btrfs_destinations_keep_parent_identity_after_namespace_replacement() -> Result<()> {
    let scratch = Scratch::new()?;
    if !crate::storage::is_supported(&scratch.0) {
        return Ok(());
    }
    let driver = crate::btrfs_driver::BtrfsFs::new(std::fs::File::open(&scratch.0)?)?;
    let seed = MemoryTree::new(attributes(0o755));
    write(&seed, "file", b"captured")?;
    let work = driver.materialise(&seed, driver.location(&scratch.0.join("work"))?)?;
    let parent = scratch.directory("parent")?;
    let destination = driver.location(&parent.join("saved"))?;
    let moved = scratch.0.join("moved-parent");
    std::fs::rename(&parent, &moved)?;
    std::fs::create_dir(&parent)?;
    crate::storage::create_subvolume(&parent.join("saved"))?;
    std::fs::write(parent.join("saved/file"), b"replacement")?;
    let saved = driver.freeze(&work.tree, destination)?;
    let mut bytes = Vec::new();
    saved.tree.reader().open_file(&path("file"))?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"captured");
    // Removal addresses the original subvolume ID through the retained parent, even though
    // the display pathname now names an unrelated subvolume.
    crate::storage::set_writable(&moved.join("saved"))?;
    drop(saved.tree);
    driver.remove(saved.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    assert!(!moved.join("saved").exists());
    assert_eq!(std::fs::read(parent.join("saved/file"))?, b"replacement");
    crate::storage::delete_tree(&parent.join("saved"))?;
    drop(work.tree);
    driver.remove(work.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(())
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[test]
fn btrfs_nested_boundaries_keep_state_ids_through_reference_copies() -> Result<()> {
    let scratch = Scratch::new()?;
    if !crate::storage::is_supported(&scratch.0) {
        return Ok(());
    }
    let native = crate::btrfs_driver::BtrfsFs::new(std::fs::File::open(&scratch.0)?)?;
    let seed = MemoryTree::new(attributes(0o755));
    write(&seed, "file", b"kept")?;
    let work_path = scratch.0.join("work");
    let work = native.materialise(&seed, native.location(&work_path)?)?;
    crate::storage::create_subvolume(&work_path.join("nested"))?;
    std::fs::write(work_path.join("nested/excluded"), b"nested subvolume content")?;
    let snapshot_path = scratch.0.join("snapshot");
    let snapshot = native.freeze(&work.tree, native.location(&snapshot_path)?)?;
    let metadata = snapshot.tree.reader().metadata(&RelDir::Path(path("nested")))?.context("boundary absent")?;
    assert!(metadata.attributes.stored_time.is_none());
    let store = layr_objects::Store::open(&scratch.directory("objects")?, layr_objects::Keying::Plain)?;
    let expected = save(&store, snapshot.tree.reader())?;
    let memory = MemoryFs::default();
    let memory_copy = capture(&memory, &|name| memory.location(path(name)), snapshot.tree.reader(), "copy")?;
    assert_eq!(save(&store, memory_copy.tree.reader())?, expected);
    let plain = PlainFs::open(&scratch.directory("plain")?)?;
    let plain_copy = capture(&plain, &|name| plain.location(path(name)), snapshot.tree.reader(), "copy")?;
    assert_eq!(save(&store, plain_copy.tree.reader())?, expected);
    drop(snapshot.tree);
    crate::storage::set_writable(&snapshot_path)?;
    native.remove(snapshot.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    drop(work.tree);
    native.remove(work.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(())
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[test]
fn btrfs_pinned_hints_and_removal_survive_root_replacement() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let scratch = Scratch::new()?;
    if !crate::storage::is_supported(&scratch.0) {
        return Ok(());
    }
    let driver = crate::btrfs_driver::BtrfsFs::new(std::fs::File::open(&scratch.0)?)?;
    let seed = MemoryTree::new(attributes(0o755));
    write(&seed, "file", b"old")?;
    let work = driver.materialise(&seed, driver.location(&scratch.0.join("work"))?)?;
    let before = driver.freeze(&work.tree, driver.location(&scratch.0.join("before"))?)?;
    write(work.tree.writer(), "file", b"new contents")?;
    let after = driver.freeze(&work.tree, driver.location(&scratch.0.join("after"))?)?;
    let hints = driver.hints_with(&before.tree, &after.tree, |base, tree, p, c| {
        // This callback runs after registry lookup and descriptor selection, immediately
        // where the real kernel send runs. Replace both namespace entries at that point.
        for (name, file) in [(base, p), (tree, c)] {
            let moved = name.with_extension("moved");
            std::fs::rename(name, &moved)?;
            crate::storage::create_subvolume(name)?;
            std::fs::write(name.join("file"), b"unrelated replacement")?;
            assert_eq!(file.metadata()?.dev(), std::fs::metadata(&moved)?.dev());
            assert_ne!(file.metadata()?.dev(), std::fs::metadata(name)?.dev());
        }
        Ok(vec![crate::stream::Entry { origin: Some("file".into()), path: Some("file".into()), ..Default::default() }])
    })?;
    let layr_fs_core::hints::HintSet::Complete(candidates) = hints else {
        anyhow::bail!("pinned pair lost completeness");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].old().unwrap().size, 3);
    assert_eq!(candidates[0].new().unwrap().size, 12);
    for (name, saved) in [("before", before), ("after", after)] {
        crate::storage::set_writable(&scratch.0.join(name).with_extension("moved"))?;
        drop(saved.tree);
        let failure = driver.remove(saved.ownership).unwrap_err();
        assert!(matches!(failure.error, FsError::LocationChanged));
        assert!(scratch.0.join(name).with_extension("moved").exists());
        assert_eq!(std::fs::read(scratch.0.join(name).join("file"))?, b"unrelated replacement");
        crate::storage::delete_tree(&scratch.0.join(name))?;
        std::fs::rename(scratch.0.join(name).with_extension("moved"), scratch.0.join(name))?;
        driver.remove(failure.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    }
    drop(work.tree);
    driver.remove(work.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    Ok(())
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[test]
fn btrfs_deletion_rejects_nonroots_and_ignores_unreadable_files() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new()?;
    if !crate::storage::is_supported(&scratch.0) {
        return Ok(());
    }
    let driver = crate::btrfs_driver::BtrfsFs::new(std::fs::File::open(&scratch.0)?)?;
    let seed = MemoryTree::new(attributes(0o755));
    let root = scratch.0.join("owned");
    let work = driver.materialise(&seed, driver.location(&root)?)?;
    std::fs::create_dir(root.join("ordinary"))?;
    assert!(matches!(driver.owned(&root.join("ordinary")), Err(FsError::Invalid("tree is not a subvolume root"))));
    assert!(root.exists());
    std::fs::write(root.join("a"), b"cannot read, can delete")?;
    std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0))?;
    std::fs::create_dir(root.join("b"))?;
    std::fs::set_permissions(root.join("b"), std::fs::Permissions::from_mode(0))?;
    crate::storage::create_subvolume(&root.join("z"))?;
    std::fs::write(root.join("z/kept"), b"nested")?;
    drop(work.tree);
    driver.remove(work.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    assert!(!root.exists());
    Ok(())
}

#[test]
fn stream_normalization_treats_non_directory_ancestors_as_absent() -> Result<()> {
    let scratch = Scratch::new()?;
    let before = scratch.directory("before")?;
    let after = scratch.directory("after")?;
    std::fs::create_dir(before.join("d"))?;
    std::fs::write(before.join("d/x"), b"child")?;
    std::fs::write(after.join("d"), b"replacement")?;
    let before = layr_fs_unix::DirectoryTree::open(&before)?;
    let after = layr_fs_unix::DirectoryTree::open(&after)?;
    for (base, tree, created) in [(&before, &after, false), (&after, &before, true)] {
        let entries = vec![
            crate::stream::Entry { origin: Some("d".into()), path: None, ..Default::default() },
            crate::stream::Entry { origin: None, path: Some("d".into()), ..Default::default() },
            crate::stream::Entry {
                origin: (!created).then(|| "d/x".into()),
                path: created.then(|| "d/x".into()),
                ..Default::default()
            },
        ];
        let candidates = crate::fs_candidates::build(base, tree, entries)?;
        let child = candidates.iter().find(|c| c.path() == "d/x").context("child candidate absent")?;
        assert_eq!(child.old().is_some(), !created);
        assert_eq!(child.new().is_some(), created);
        let directory = candidates.iter().find(|c| c.path() == "d").context("directory candidate absent")?;
        assert_eq!(directory.old().unwrap().is_dir(), !created);
        assert_eq!(directory.new().unwrap().is_dir(), created);
    }
    Ok(())
}
