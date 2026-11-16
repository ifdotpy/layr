//! Portable state objects are the durable source for new and migrated projects. Generic readers use
//! ObjectTree directly. Native-only consumers can request a disposable tree through
//! build_state; retire_native removes that copy after the command releases its readers.

use crate::repo::Repo;
use crate::store::Project;
use anyhow::{Context, Result};
use layr_objects::{Id, Keying, Node, StateRef, Store};
use std::path::PathBuf;

mod maintenance;
pub(crate) use maintenance::maintenance_lock;
pub use maintenance::{hot_states, hot_trees, maintain, periodic, MaintenanceInterval, MaintenanceMode};

/// The folder of the object store of a project.
pub fn dir(p: &Project) -> PathBuf {
    p.meta_dir().join("objects")
}

/// Whether the project has an object store (without making one).
pub fn exists(p: &Project) -> bool {
    dir(p).join("config.json").is_file()
}

/// Open the object store of a project, made when absent.
pub fn open(p: &Project) -> Result<Store> {
    // Plain ids for every project in this slice; Secret projects get keyed ids when the project
    // key is available here (docs/objects.md, "Secret overlays").
    let mut store = Store::open(&dir(p), Keying::Plain)?;
    // Native immutable flags belong to the cloned inode's executor.
    store.clone_hook = crate::fs::host().file_copies.unseal_cloned;
    Ok(store)
}

/// Inspect an existing object store without changing its format marker or files.
pub fn open_existing(p: &Project) -> Result<Store> {
    Store::open_existing(&dir(p), Keying::Plain)
}

/// The tree and times table that the record of `state` names, when the local object store holds
/// both. None for a state that `layr migrate` did not record, or whose objects are elsewhere (a
/// record from another machine).
pub fn verified(p: &Project, state: &str) -> Result<Option<(Store, StateRef)>> {
    if !exists(p) {
        return Ok(None);
    }
    let Some(rec) = p.db.state(state)? else {
        return Ok(None);
    };
    let (Some(tree), Some(times)) = (rec.tree.as_deref(), rec.times.as_deref()) else {
        return Ok(None);
    };
    let r = StateRef { tree: Id::parse(tree)?, times: Id::parse(times)? };
    let store = open(p)?;
    if !store.has(&r.tree) || !store.has(&r.times) {
        return Ok(None);
    }
    Ok(Some((store, r)))
}

/// Make the snapshot of `state` again from its tree and times table: a checkout into a new tree of
/// the work folder, then a read-only snapshot at `states/<id>`, as a state built from Git
/// (`gitbridge::build_tree`). Returns false when the object store has no tree for the state.
pub fn build_state(repo: &Repo, state: &str) -> Result<bool> {
    let _lock = repo.p.lock_or_borrow()?;
    if repo.p.has_state(state) {
        return Ok(true);
    }
    let Some((store, r)) = verified(&repo.p, state)? else {
        return Ok(false);
    };
    let _t = crate::trace::span(format!("build {} from objects", crate::ids::short(state)));
    let source =
        crate::source::Source::Objects(crate::object_tree::ObjectTree::new(store, r, &repo.p.meta_dir().join("object-reads"))?);
    #[cfg(test)]
    crate::repo::MATERIALIZATIONS.with(|n| n.set(n.get() + 1));
    let captured = crate::fs::freeze_source(&source, &repo.p.state_path(state))
        .with_context(|| format!("cannot build state {} from the object store", crate::ids::short(state)))?;
    if let Err(error) = queue_native(&repo.p, state) {
        return Err(captured.abort(error));
    }
    match repo.p.lock_or_borrow()?.write(|tx| tx.set_present(state, true)) {
        Ok(()) => captured.release(),
        Err(crate::db::CommitResolution::NotCommitted(error)) => return Err(captured.abort(error)),
        Err(error) => {
            captured.release();
            return Err(error.into());
        }
    }
    Ok(true)
}

/// The entry at `path` ("" is the root) of a tree, read from the objects (`layr show`).
pub fn entry(store: &Store, root: &Id, path: &str) -> Result<crate::gitbridge::Entry> {
    use crate::gitbridge::Entry;
    let mut tree = store.get_tree(root)?;
    let parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    for (i, name) in parts.iter().enumerate() {
        let Some(e) = tree.entries.iter().find(|e| e.name == name.as_bytes()) else {
            return Ok(Entry::Missing);
        };
        let last = i + 1 == parts.len();
        match (&e.node, last) {
            (Node::Dir { tree: t }, _) => tree = store.get_tree(t)?,
            (Node::File { blob, .. }, true) => return Ok(Entry::File(store.read_blob(blob).context("read a blob")?)),
            (Node::Symlink { target }, true) => return Ok(Entry::Link(target.clone())),
            (_, false) => return Ok(Entry::Missing),
        }
    }
    Ok(Entry::Dir(
        tree.entries
            .iter()
            .map(|e| (String::from_utf8_lossy(&e.name).into_owned(), matches!(e.node, Node::Dir { .. })))
            .collect(),
    ))
}

/// Project setting shared by new projects and successful migrations.
pub const ENABLED: &str = "objects.enabled";

/// Enable portable source-state writes for a new project or a completed migration.
/// The caller holds the project lock; existing projects keep their recorded setting.
pub fn enable(locked: &crate::store::LockedProject<'_>, ctx: &crate::ctx::Ctx, repo: &Repo) -> Result<()> {
    crate::project_config::set_config(locked, ctx, repo, ENABLED, serde_json::json!(true))
}

pub use layr_objects::write::ReaderSource as SnapshotSource;

/// Whether new states of `p` go to its object store. An overlay (an overlay) follows its project: a part of an overlay
/// is stored as objects like a state of the base (docs/overlays.md section 1a, slice 7), in the overlay's own store.
pub fn enabled(p: &Project) -> Result<bool> {
    let on = |v: Option<serde_json::Value>| v.and_then(|v| v.as_bool()) == Some(true);
    let Some((project, _)) = p.name.split_once('%') else {
        return Ok(on(p.config_value(ENABLED)?));
    };
    if on(p.config_value(ENABLED)?) {
        return Ok(true);
    }
    let path = crate::store::db_file(&p.store.project_dir(project)?);
    if !path.is_file() {
        return Ok(false);
    }
    Ok(on(Project::open_partial(p.store, project)?.config_value(ENABLED)?))
}

/// Comparison of an immutable save snapshot. Its delta stays bound to that snapshot
/// and the object refs it was checked against until publication.
pub struct Compared {
    pub files: Vec<crate::compare::EntryChange>,
    source: crate::repo::Snap,
    delta: Option<CheckedDelta>,
}

struct CheckedDelta {
    refs: StateRef,
    changed: Vec<Vec<u8>>,
    dirty: Vec<Vec<u8>>,
}

pub fn compare(parent: &crate::repo::Snap, source: crate::repo::Snap) -> Result<Compared> {
    use crate::fsutil::Src;
    if let (Some(objects), Some(folder)) = (parent.objects(), source.folder()) {
        let comparison = crate::changes::compare_for_write(parent, folder)?;
        let delta = CheckedDelta { refs: objects.refs, changed: comparison.changed, dirty: comparison.dirty };
        return Ok(Compared { files: comparison.files, source, delta: Some(delta) });
    }
    let files = crate::changes::file_changes(parent, &source)?;
    Ok(Compared { files, source, delta: None })
}

impl Compared {
    /// Store checked object deltas before the record is signed. Other sources keep the
    /// existing publication fallback. Call only after the save's checks succeed.
    pub fn record(self, p: &Project, state: &mut crate::model::StateRec) -> Result<()> {
        let Some(CheckedDelta { refs, changed, dirty }) = self.delta else { return Ok(()) };
        if !enabled(p)? || state.tree.is_some() {
            return Ok(());
        }
        let store = open(p)?;
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        let _trace = crate::trace::span("write checked objects");
        let written = layr_objects::write::write_changed_with_dirty_from(
            &store,
            &SnapshotSource(&self.source),
            &layr_objects::write::Base { tree: refs.tree, times: refs.times },
            &changed,
            &dirty,
            &options,
        )?;
        attach(p, &store, state, written)
    }
}

fn attach(p: &Project, store: &Store, state: &mut crate::model::StateRec, written: layr_objects::WriteReport) -> Result<()> {
    let _trace = crate::trace::span(format!("objects write {:?}: {} files", written.mode, written.files));
    let refs = StateRef {
        tree: written.tree.context("object write gave no tree")?,
        times: written.times.context("object write gave no times")?,
    };
    store.set_state(&state.id, &refs)?;
    state.tree = Some(refs.tree.hex());
    state.times = Some(refs.times.hex());
    if !p.is_overlay() || state.cache_of.is_some() {
        queue_native(p, &state.id)?;
    }
    Ok(())
}

/// Attach durable objects before signing each new source state. Native copies remain until
/// command cleanup, so callers can finish with the paths they already hold.
pub fn record_states(p: &Project, records: &mut [crate::model::DraftRecord]) -> Result<()> {
    if !enabled(p)? {
        return Ok(());
    }
    let store = open(p)?;
    for state in records.iter_mut().flat_map(|r| &mut r.states) {
        if state.kind.auxiliary() || state.tree.is_some() || !p.has_state(&state.id) {
            continue;
        }
        let source = crate::source::Source::of_state(p, &state.id)?;
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        let incremental = state_write_base(p, &store, &state.id, state.parents.first().map(String::as_str), &source)?;
        // LAYR_TRACE names the kind of write (tests/t257-btrfs-incremental-write.sh reads it).
        let _t = crate::trace::named(|| {
            let kind = if incremental.is_some() { "incremental" } else { "full" };
            format!("objects: {kind} write of {}", crate::ids::short(&state.id))
        });
        let source = SnapshotSource(&source);
        let written = match incremental {
            Some((refs, paths)) => layr_objects::write::write_changed_from(
                &store,
                &source,
                &layr_objects::write::Base { tree: refs.tree, times: refs.times },
                &paths,
                &options,
            )?,
            None => {
                let mut report = layr_objects::write::write_tree_from(&store, &source, &options)?;
                report.mode = layr_objects::write::WriteMode::Full { reason: layr_objects::write::FullReason::NoCheckedDelta };
                report
            }
        };
        attach(p, &store, state, written)?;
    }
    Ok(())
}

/// Select an incremental base without losing time-only or attribute-only changes.
pub fn write_base(
    repo: &Repo,
    store: &Store,
    state: &str,
    parent: Option<&str>,
    source: &crate::repo::Snap,
) -> Result<Option<(StateRef, Vec<Vec<u8>>)>> {
    let _pin = parent.map(|id| repo.pin(&[id])).transpose()?;
    state_write_base(&repo.p, store, state, parent, source)
}

fn state_write_base(
    p: &Project,
    store: &Store,
    state: &str,
    parent: Option<&str>,
    source: &crate::repo::Snap,
) -> Result<Option<(StateRef, Vec<Vec<u8>>)>> {
    if let Some((base, paths)) = crate::source::storage::stored_delta(p, state)? {
        if let Some(refs) = store.state_ref(&base)? {
            return Ok(Some((refs, paths)));
        }
    }
    let Some(parent_state) = parent else { return Ok(None) };
    let Some(refs) = store.state_ref(parent_state)? else { return Ok(None) };
    let parent = if p.has_state(parent_state) {
        let source = crate::source::Source::of_state(p, parent_state)?;
        source
    } else {
        let tree = crate::object_tree::ObjectTree::new(open(p)?, refs, &p.meta_dir().join("object-reads"))?;
        let source = crate::source::Source::Objects(tree);
        source
    };
    changed_write_base(refs, &parent, source)
}

fn changed_write_base(
    refs: StateRef,
    parent: &crate::repo::Snap,
    source: &crate::repo::Snap,
) -> Result<Option<(StateRef, Vec<Vec<u8>>)>> {
    use crate::fsutil::Src;
    if parent.objects().is_some() {
        let Some(folder) = source.folder() else { return Ok(None) };
        let comparison = crate::changes::compare_for_write(parent, folder)?;
        // This older import API has no dirty-folder argument. Including those folders
        // conservatively imports their subtree and preserves all metadata.
        let paths = comparison.changed.into_iter().chain(comparison.dirty).collect();
        return Ok(Some((refs, paths)));
    }
    let (changed, dirty) = crate::changes::object_candidates(parent, source)?;
    let paths: std::collections::BTreeSet<Vec<u8>> = changed.into_iter().chain(dirty).collect();
    Ok(Some((refs, paths.into_iter().collect())))
}

/// The native copy is disposable only after the signed record names durable objects.
/// Queue before publication so a crash leaves the retry marker beside the objects.
fn queue_native(p: &Project, state: &str) -> Result<()> {
    let dir = p.meta_dir().join("object-native");
    std::fs::create_dir_all(&dir)?;
    crate::fsutil::atomic_write(&dir.join(state), b"", 0o600)
}

/// Retire native copies after command readers release their pins. The existing reservation
/// protocol prevents another reader from taking a path while it is removed.
pub fn retire_native(repo: &Repo) -> Result<()> {
    let _lock = repo.p.lock_or_borrow()?;
    if !repo.p.meta_dir().join("object-native").is_dir() {
        return Ok(());
    }
    retire_native_locked(repo, crate::retention::overlay_named_states(repo)?)
}

/// A command's free step leaves busy native copies for the next pass.
pub fn retire_native_maintenance(repo: &Repo) -> Result<()> {
    if !repo.p.meta_dir().join("object-native").is_dir() {
        return Ok(());
    }
    let crate::store::LockAttempt::Acquired(_lock) = repo.p.try_lock()? else { return Ok(()) };
    let Some(keep) = crate::retention::overlay_named_states_maintenance(repo)? else { return Ok(()) };
    retire_native_locked(repo, keep)
}

fn retire_native_locked(repo: &Repo, keep: std::collections::BTreeSet<String>) -> Result<()> {
    let dir = repo.p.meta_dir().join("object-native");
    if !dir.is_dir() {
        return Ok(());
    }
    #[allow(unused_mut)]
    let mut keep = keep;
    #[cfg(target_os = "macos")]
    keep.extend(crate::views::protected_states(&repo.p));
    keep.extend(native_bases(repo)?);
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let id = entry?.file_name().to_string_lossy().into_owned();
        let Some(state) = repo.p.db.state(&id)? else { continue };
        if keep.contains(&id) || state.kind.auxiliary() {
            continue;
        }
        if verified(&repo.p, &id)?.is_none() {
            continue;
        }
        ids.push(id);
    }
    let gone = crate::retention::drop_trees(repo, &ids)?;
    for id in gone {
        std::fs::remove_file(dir.join(id))?;
    }
    Ok(())
}

/// The native copies that the next save needs off macOS: the head of each line with a working folder here.
/// The next state of the line is written incrementally from it, and without it the write reads and hashes
/// the whole tree. The marker stays, so a later pass retires the copy once a newer state replaced the head
/// (or the line went). An overlay reads its parts through objects and keeps none. macOS writes
/// incrementally from the thin layer and keeps none either.
fn native_bases(repo: &Repo) -> Result<Vec<String>> {
    if cfg!(target_os = "macos") || repo.p.is_overlay() {
        return Ok(Vec::new());
    }
    Ok(repo
        .p
        .db
        .lines()?
        .into_iter()
        .filter(|line| repo.p.line_folder(&line.id, &line.name).exists())
        .map(|line| line.head)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(feature = "overlayfs", ignore = "OverlayFS registry requires root")]
    fn enable_records_the_project_setting_without_changing_existing_projects() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-enable-{}", crate::ids::new_id()));
        let ctx = crate::ctx::Ctx {
            store: crate::store::Store::open(&root)?,
            caller: crate::ctx::Caller::from_uid(rustix::process::getuid().as_raw()),
            cwd: root.clone(),
            env: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
            author: Default::default(),
            captures: Default::default(),
        };
        let repo = Repo { p: Project::create(&ctx.store, "p", &ctx.actor(), serde_json::json!({}))?, ctx: None };
        assert!(!enabled(&repo.p)?);
        {
            let lock = repo.p.lock_maintenance()?;
            enable(&lock, &ctx, &repo)?;
        }
        assert!(enabled(&repo.p)?);
        let records = repo.p.db.records_with_op("config")?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].data["config"][ENABLED], true);
        repo.p.verify_records()?;
        drop(repo);
        let p = Project::open(&ctx.store, "p")?;
        assert!(enabled(&p)?, "the setting survives reopening");
        drop(p);
        drop(ctx);
        crate::fs::remove(&root)?;
        Ok(())
    }

    fn options() -> layr_objects::WriteOptions {
        layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        }
    }

    #[test]
    fn object_only_parent_writes_five_files_without_resolving_unchanged_files() -> Result<()> {
        use layr_objects::write::{Source, WriteMode};
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("layr-object-base-{}", crate::ids::new_id()));
        let folder = root.join("source");
        std::fs::create_dir_all(folder.join("sub"))?;
        for i in 0..100 {
            std::fs::write(folder.join(format!("sub/f{i}")), format!("original {i}"))?;
        }
        let store = Store::open(&root.join("objects"), Keying::Plain)?;
        let original = layr_objects::write_tree(&store, &folder, &options())?;
        let refs = StateRef { tree: original.tree.unwrap(), times: original.times.unwrap() };
        // Only objects retain the parent; the folder now becomes the new snapshot.
        let tree = crate::object_tree::ObjectTree::new(store, refs, &root.join("reads"))?;
        #[cfg(target_os = "macos")]
        let parent = crate::source::Source::Objects(tree);
        #[cfg(not(target_os = "macos"))]
        let parent = crate::repo::Snap::Objects(tree);
        for i in 0..5 {
            std::fs::write(folder.join(format!("sub/f{i}")), format!("changed {i}"))?;
        }
        let compared = compare(&parent, crate::repo::snap(&folder))?;
        assert_eq!(compared.files.len(), 5);
        let CheckedDelta { refs, changed, dirty } = compared.delta.unwrap();
        assert_eq!(changed.len(), 5);
        let snapshot = SnapshotSource(&compared.source);
        // A readable directory without search permission exposes names but forbids
        // metadata resolution of its children. Exercise the production enumeration.
        let sub = folder.join("sub");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o444))?;
        let blocked = std::fs::symlink_metadata(sub.join("f99")).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        let names = snapshot.names(b"sub");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755))?;
        assert!(blocked, "the fixture must forbid resolving an unchanged sibling");
        assert_eq!(names?.len(), 100);
        struct ChangedOnly<'a>(SnapshotSource<'a>, &'a [Vec<u8>]);
        impl Source for ChangedOnly<'_> {
            fn path(&self, relative: &[u8]) -> Result<PathBuf> {
                anyhow::ensure!(
                    relative.is_empty() || relative == b"sub" || self.1.iter().any(|p| p == relative),
                    "unchanged file was resolved"
                );
                self.0.path(relative)
            }
            fn names(&self, relative: &[u8]) -> Result<Vec<Vec<u8>>> {
                self.0.names(relative)
            }
            fn metadata(&self, relative: &[u8]) -> Result<layr_fs_core::EntryMetadata> {
                anyhow::ensure!(
                    relative.is_empty() || relative == b"sub" || self.1.iter().any(|path| path == relative),
                    "unchanged file was resolved"
                );
                self.0.metadata(relative)
            }
            fn read_link(&self, relative: &[u8]) -> Result<Vec<u8>> {
                self.0.read_link(relative)
            }
            fn store_file(
                &self,
                store: &Store,
                relative: &[u8],
                temperature: layr_objects::Temperature,
                bases: &[Id],
                judge: bool,
            ) -> Result<(Id, u64)> {
                anyhow::ensure!(self.1.iter().any(|path| path == relative), "unchanged file was read");
                self.0.store_file(store, relative, temperature, bases, judge)
            }
        }
        let store = Store::open(&root.join("objects"), Keying::Plain)?;
        let report = layr_objects::write::write_changed_with_dirty_from(
            &store,
            &ChangedOnly(snapshot, &changed),
            &layr_objects::write::Base { tree: refs.tree, times: refs.times },
            &changed,
            &dirty,
            &options(),
        )?;
        assert_eq!(report.mode, WriteMode::Incremental { changed_paths: 5 });
        assert_eq!(report.files, 5);
        let full = layr_objects::write_tree(&store, &folder, &options())?;
        assert_eq!((report.tree, report.times), (full.tree, full.times));
        drop(compared.source);
        drop(parent);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn object_comparison_preserves_metadata_and_link_topology() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("layr-object-metadata-{}", crate::ids::new_id()));
        let folder = root.join("source");
        std::fs::create_dir_all(folder.join("sub"))?;
        for name in ["time", "mode", "attr", "split", "join", "sub/file"] {
            std::fs::write(folder.join(name), name)?;
        }
        std::fs::hard_link(folder.join("split"), folder.join("split-alias"))?;
        let store = Store::open(&root.join("objects"), Keying::Plain)?;
        let original = layr_objects::write_tree(&store, &folder, &options())?;
        let refs = StateRef { tree: original.tree.unwrap(), times: original.times.unwrap() };
        let tree = crate::object_tree::ObjectTree::new(store, refs, &root.join("reads"))?;
        #[cfg(target_os = "macos")]
        let parent = crate::source::Source::Objects(tree);
        #[cfg(not(target_os = "macos"))]
        let parent = crate::repo::Snap::Objects(tree);
        // Nanosecond-only changes, non-executable mode bits, attributes, empty folders,
        // replacement types, and groups that split or join despite equal file bytes.
        layr_objects::times::set(&folder.join("time"), 1_700_000_000_123_456_789)?;
        layr_objects::times::set(&folder.join("sub"), 1_700_000_000_123_456_789)?;
        std::fs::set_permissions(folder.join("mode"), std::fs::Permissions::from_mode(0o600))?;
        layr_objects::fsx::set_xattrs(&folder.join("attr"), &[(b"user.test".to_vec(), b"new".to_vec())])?;
        std::fs::remove_file(folder.join("split-alias"))?;
        std::fs::write(folder.join("split-alias"), b"split")?;
        std::fs::hard_link(folder.join("join"), folder.join("joined"))?;
        std::fs::create_dir(folder.join("empty"))?;
        std::fs::remove_file(folder.join("sub/file"))?;
        std::fs::create_dir(folder.join("sub/file"))?;
        let compared = compare(&parent, crate::repo::snap(&folder))?;
        let CheckedDelta { refs, changed, dirty } = compared.delta.unwrap();
        let store = Store::open(&root.join("objects"), Keying::Plain)?;
        let report = layr_objects::write::write_changed_with_dirty_from(
            &store,
            &SnapshotSource(&compared.source),
            &layr_objects::write::Base { tree: refs.tree, times: refs.times },
            &changed,
            &dirty,
            &options(),
        )?;
        let full = layr_objects::write_tree(&store, &folder, &options())?;
        assert_eq!((report.tree, report.times), (full.tree, full.times));
        drop(compared.source);
        drop(parent);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn incomplete_hard_link_groups_report_full_write() -> Result<()> {
        use layr_objects::write::{Base, FullReason, WriteMode};
        let root = std::env::temp_dir().join(format!("layr-object-links-{}", crate::ids::new_id()));
        let folder = root.join("source");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("a"), b"old")?;
        std::fs::hard_link(folder.join("a"), folder.join("b"))?;
        let store = Store::open(&root.join("objects"), Keying::Plain)?;
        let original = layr_objects::write_tree(&store, &folder, &options())?;
        let base = Base { tree: original.tree.unwrap(), times: original.times.unwrap() };
        std::fs::write(folder.join("a"), b"new")?;
        let report = layr_objects::write::write_changed(&store, &folder, &base, &[b"a".to_vec()], &options())?;
        assert_eq!(report.mode, WriteMode::Full { reason: FullReason::IncompleteHardLinks });
        assert_eq!(report.files, 2);
        let full = layr_objects::write_tree(&store, &folder, &options())?;
        assert_eq!((report.tree, report.times), (full.tree, full.times));
        // Also cover an alias absent from the base group, discovered during the walk.
        std::fs::remove_file(folder.join("b"))?;
        std::fs::write(folder.join("b"), b"new")?;
        let single = layr_objects::write_tree(&store, &folder, &options())?;
        let base = Base { tree: single.tree.unwrap(), times: single.times.unwrap() };
        std::fs::remove_file(folder.join("b"))?;
        std::fs::hard_link(folder.join("a"), folder.join("b"))?;
        let report = layr_objects::write::write_changed(&store, &folder, &base, &[b"a".to_vec()], &options())?;
        assert_eq!(report.mode, WriteMode::Full { reason: FullReason::IncompleteHardLinks });
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    #[cfg_attr(feature = "overlayfs", ignore = "OverlayFS registry requires root")]
    #[cfg(target_os = "linux")]
    fn native_base_keeps_metadata_changes_through_complete_reader_evidence() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-native-base-{}", crate::ids::new_id()));
        let store = crate::store::Store::open(&root)?;
        let dir = store.project_dir("p")?;
        std::fs::create_dir_all(dir.join("meta/db"))?;
        let p = Project {
            lease: Default::default(),
            store: &store,
            name: "p".into(),
            dir: dir.clone(),
            id: crate::ids::new_id(),
            db: crate::db::Db::open(&crate::store::db_file(&dir))?,
        };
        let id = crate::ids::new_id();
        let parent = p.state_path(&id);
        let child = root.join("child");
        for folder in [&parent, &child] {
            std::fs::create_dir_all(folder)?;
            for path in ["same", "time", "attr"] {
                std::fs::write(folder.join(path), b"data")?;
                layr_objects::times::set(&folder.join(path), 1_700_000_000_000_000_000)?;
            }
        }
        let objects = open(&p)?;
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        let full = layr_objects::write_tree(&objects, &parent, &options)?;
        let refs = StateRef { tree: full.tree.unwrap(), times: full.times.unwrap() };
        objects.set_state(&id, &refs)?;
        layr_objects::times::set(&child.join("time"), 1_700_000_001_000_000_000)?;
        layr_objects::fsx::set_xattrs(&child.join("attr"), &[(b"user.test".to_vec(), b"new".to_vec())])?;
        let source = crate::repo::snap(&child);
        let base = state_write_base(&p, &objects, "child", Some(&id), &source)?;
        {
            let (base, paths) = base.expect("native parent");
            assert!(paths.contains(&b"time".to_vec()));
            assert!(paths.contains(&b"attr".to_vec()));
            assert!(!paths.contains(&b"same".to_vec()));
            let incremental = layr_objects::write::write_changed_from(
                &objects,
                &SnapshotSource(&source),
                &layr_objects::write::Base { tree: base.tree, times: base.times },
                &paths,
                &options,
            )?;
            let full = layr_objects::write_tree(&objects, &child, &options)?;
            assert_eq!(incremental.tree, full.tree);
            assert_eq!(incremental.times, full.times);
        }
        drop(p);
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
