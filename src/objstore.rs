//! Portable state objects are the durable source for new and migrated projects. Generic readers use
//! ObjectTree directly. Native-only consumers can request a disposable tree through
//! build_state; retire_native removes that copy after the command releases its readers.

use crate::linepaths::LineFolders;
use crate::repo::Repo;
use crate::store::Project;
use anyhow::{Context, Result};
use layr_objects::{Id, Keying, Node, StateRef, Store};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

#[cfg(test)]
thread_local! { pub(crate) static OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// Open the object store of a project, made when absent.
pub fn open(p: &Project) -> Result<Store> {
    #[cfg(test)]
    OPENS.with(|count| count.set(count.get() + 1));
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

/// The object store handle that the reads of a project handle share within `share` (`ProjectLease::objects`).
#[derive(Default)]
pub struct Shared {
    scopes: std::cell::Cell<usize>,
    store: std::cell::RefCell<Option<Arc<Store>>>,
}

/// The object store of `p`: within `share`, one handle that the first call opens and the later ones and the trees
/// that they read share; else a new one. An open lists and opens every pack and reads the dictionary.
pub fn shared(p: &Project) -> Result<Arc<Store>> {
    let objects = &p.lease.objects;
    if let Some(store) = objects.store.borrow().as_ref() {
        return Ok(store.clone());
    }
    let store = Arc::new(open(p)?);
    if objects.scopes.get() > 0 {
        *objects.store.borrow_mut() = Some(store.clone());
    }
    Ok(store)
}

/// Reads of `p` share one object store handle (`shared`) until the guard goes: a history reads hundreds of states.
/// The handle holds a shared prune lease, so a command that lives on (a worker, a watcher) or prunes the store
/// itself reads without one.
pub fn share<'p, 'a>(p: &'p Project<'a>) -> SharedReads<'p, 'a> {
    let objects = &p.lease.objects;
    objects.scopes.set(objects.scopes.get() + 1);
    SharedReads(p)
}

/// The guard of `share`: the last one lets the handle go.
pub struct SharedReads<'p, 'a>(&'p Project<'a>);

impl SharedReads<'_, '_> {
    /// The handle that this scope shares (`shared`).
    pub fn objects(&self) -> Result<Arc<Store>> {
        shared(self.0)
    }
}

impl Drop for SharedReads<'_, '_> {
    fn drop(&mut self) {
        let objects = &self.0.lease.objects;
        objects.scopes.set(objects.scopes.get() - 1);
        if objects.scopes.get() == 0 {
            objects.store.borrow_mut().take();
        }
    }
}

/// The tree and times table that the record of `state` names, when the local object store holds
/// both. None for a state that `layr migrate` did not record, or whose objects are elsewhere (a
/// record from another machine).
pub fn verified(p: &Project, state: &str) -> Result<Option<(Arc<Store>, StateRef)>> {
    if !exists(p) {
        return Ok(None);
    }
    let Some(r) = recorded_refs(p, state)? else {
        return Ok(None);
    };
    let store = shared(p)?;
    // A shared handle listed the packs when it opened: a miss lists them again, as a new open did,
    // for a pack that a repack or a fetch published since.
    let held = holds(&store, &r) || {
        store.refresh_packs()?;
        holds(&store, &r)
    };
    Ok(held.then_some((store, r)))
}

/// The tree and times table that the record of `state` names, when `objects` holds both.
pub fn recorded(objects: &Store, state: &crate::model::StateRec) -> Result<Option<StateRef>> {
    Ok(named(state)?.filter(|found| holds(objects, found)))
}

fn holds(objects: &Store, found: &StateRef) -> bool {
    objects.has(&found.tree) && objects.has(&found.times)
}

/// The tree and times table that the record of `state` names, whether or not this machine has them.
pub fn named(state: &crate::model::StateRec) -> Result<Option<StateRef>> {
    let (Some(tree), Some(times)) = (state.tree.as_deref(), state.times.as_deref()) else { return Ok(None) };
    Ok(Some(StateRef { tree: Id::parse(tree)?, times: Id::parse(times)? }))
}

/// The tree and times table that the signed record of `state` names, if it names them.
pub fn recorded_refs(p: &Project, state: &str) -> Result<Option<StateRef>> {
    let Some(rec) = p.db.state(state)? else {
        return Ok(None);
    };
    let (Some(tree), Some(times)) = (rec.tree.as_deref(), rec.times.as_deref()) else {
        return Ok(None);
    };
    Ok(Some(StateRef { tree: Id::parse(tree)?, times: Id::parse(times)? }))
}

/// Make the snapshot of `state` again from its tree and times table: a checkout into a new tree of
/// the work folder, then a read-only snapshot at `states/<id>`, as a state built from Git
/// (`gitbridge::build_tree`). Returns false when the object store has no tree for the state.
pub fn build_state(repo: &Repo, state: &str) -> Result<bool> {
    if repo.p.has_state(state) {
        return Ok(true);
    }
    let Some((store, r)) = verified(&repo.p, state)? else {
        // A state whose objects this store recorded and cannot read is damage (#452): no other
        // source has them. This check takes no lock, so a save preparation can meet it.
        if exists(&repo.p) {
            open_existing(&repo.p)?.checked_state(state)?;
        }
        return Ok(false);
    };
    let _lock = repo.p.lock_or_borrow()?;
    if repo.p.has_state(state) {
        return Ok(true);
    }
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

/// `build_state` from the snapshot of the state `near`: a copy of it with the paths that differ
/// between the objects of the two states. For a state that has objects and no snapshot when a
/// state near it has one (a commit that git made in a fast line, next to the old head of the
/// line): the next save in the line needs the snapshot of the head, and `build_state` writes
/// every file of the tree for it. False when `near` has no snapshot or no objects.
#[cfg(not(target_os = "macos"))]
pub fn build_state_near(repo: &Repo, state: &str, near: &str) -> Result<bool> {
    let _lock = repo.p.lock_or_borrow()?;
    if repo.p.has_state(state) {
        return Ok(true);
    }
    let parent = near.to_string();
    if !repo.p.has_state(&parent) {
        return Ok(false);
    }
    let (Some((_, new)), Some((_, old))) = (verified(&repo.p, state)?, verified(&repo.p, &parent)?) else {
        return Ok(false);
    };
    let _t = crate::trace::span(format!("build {} from {}", crate::ids::short(state), crate::ids::short(near)));
    let reads = repo.p.meta_dir().join("object-reads");
    let a = crate::repo::Snap::Objects(crate::object_tree::ObjectTree::new(open(&repo.p)?, old, &reads)?);
    let b = crate::repo::Snap::Objects(crate::object_tree::ObjectTree::new(open(&repo.p)?, new, &reads)?);
    // A file with the same content and another time gets its time too.
    let (list, touched) = crate::changes::file_changes_and_touched(&a, &b)?;
    std::fs::create_dir_all(repo.p.work_dir())?;
    let work = repo.p.work_dir().join(crate::store::temp_name());
    let built = (|| -> Result<crate::fs::OwnedCapture> {
        crate::fs::snapshot(&repo.p.state_path(&parent), &work, false)?;
        for c in &list {
            if let Some(old_path) = c.old_path() {
                crate::fsutil::remove_entry(&work, old_path)?;
            }
            if c.new().is_none() {
                crate::fsutil::remove_entry(&work, c.path())?;
            }
        }
        for path in list.iter().filter(|c| c.new().is_some()).map(|c| c.path()).chain(&touched) {
            crate::fsutil::copy_entry(&b, path, &work, path, None)?;
        }
        crate::fs::freeze_path_owned(&work, &repo.p.state_path(state), crate::fs::CapturePurpose::State)
    })();
    let _ = crate::fs::remove(&work);
    let captured = built.with_context(|| {
        format!("cannot build state {} from state {}", crate::ids::short(state), crate::ids::short(near))
    })?;
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

/// Compare `source` with `parent`, everywhere or within a `Selection` outside which they are
/// equal (an object base still compares everything: its write needs every folder's times).
pub fn compare(
    parent: &crate::repo::Snap,
    source: crate::repo::Snap,
    selection: Option<&crate::changes::Selection>,
) -> Result<Compared> {
    use crate::fsutil::Src;
    if let (Some(objects), Some(folder)) = (parent.objects(), source.folder()) {
        let comparison = crate::changes::compare_for_write(parent, folder)?;
        let delta = CheckedDelta { refs: objects.refs, changed: comparison.changed, dirty: comparison.dirty };
        return Ok(Compared { files: comparison.files, source, delta: Some(delta) });
    }
    let files = match selection {
        Some(selection) => crate::changes::file_changes_selected(parent, &source, selection)?,
        None => crate::changes::file_changes(parent, &source)?,
    };
    Ok(Compared { files, source, delta: None })
}

impl Compared {
    /// Store checked object deltas before the record is signed. Other sources keep the
    /// existing publication fallback. Call only after the save's checks succeed.
    pub fn record(self, p: &Project, state: &mut crate::model::StateRec) -> Result<()> {
        if self.prepare(p, state)? {
            install_prepared(p, state)?;
        }
        Ok(())
    }

    /// Encode this immutable comparison without installing a state ref. Automatic saves
    /// keep publication under their existing validation and installation protocol.
    pub(crate) fn prepare(self, p: &Project, state: &mut crate::model::StateRec) -> Result<bool> {
        let Some(CheckedDelta { refs, changed, dirty }) = self.delta else { return Ok(false) };
        if !enabled(p)? || state.tree.is_some() {
            return Ok(false);
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
        describe_write(state, &written)?;
        Ok(true)
    }
}

fn attach(p: &Project, store: &Store, state: &mut crate::model::StateRec, written: layr_objects::WriteReport) -> Result<()> {
    let refs = describe_write(state, &written)?;
    store.set_state(&state.id, &refs)?;
    if !p.is_overlay() || state.cache_of.is_some() {
        queue_native(p, &state.id)?;
    }
    Ok(())
}

fn describe_write(state: &mut crate::model::StateRec, written: &layr_objects::WriteReport) -> Result<StateRef> {
    trace_write(state, written);
    let refs = StateRef {
        tree: written.tree.context("object write gave no tree")?,
        times: written.times.context("object write gave no times")?,
    };
    state.tree = Some(refs.tree.hex());
    state.times = Some(refs.times.hex());
    Ok(refs)
}

fn trace_write(state: &crate::model::StateRec, written: &layr_objects::WriteReport) {
    // Both forms are read by tests: t257 the kind, t262 the mode and file count.
    let kind = match written.mode {
        layr_objects::write::WriteMode::Incremental { .. } => "incremental",
        layr_objects::write::WriteMode::Full { .. } => "full",
    };
    let _trace = crate::trace::span(format!(
        "objects: {kind} write of {}; objects write {:?}: {} files",
        crate::ids::short(&state.id),
        written.mode,
        written.files
    ));
}

const HOT_STATE_WRITE: layr_objects::WriteOptions = layr_objects::WriteOptions {
    temperature: layr_objects::Temperature::Hot,
    previous: None,
    exclude: Vec::new(),
    similar: false,
};

/// Encode the first snapshot of a new project before the initial commit can name it.
/// Use an unpublished store so a failed flush cannot leave objects for other writers to reuse.
#[cfg(target_os = "linux")]
pub(crate) fn record_initial_state(p: &crate::store::LockedProject<'_>, state: &mut crate::model::StateRec) -> Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    use std::fs::File;

    let root = dir(p);
    let meta = p.meta_dir();
    // Older kernels do not report filesystem writeback errors through syncfs.
    let checked_syncfs = sysinfo::System::kernel_version().is_some_and(|release| {
        let mut parts = release.split('.');
        let major = parts.next().and_then(|n| n.parse::<u32>().ok());
        let minor = parts.next().and_then(|n| n.parse::<u32>().ok());
        major.zip(minor).is_some_and(|version| version >= (5, 8))
    });
    if !checked_syncfs || root.exists() || crate::sys::fs_type_name(&meta)? != "btrfs" {
        return Ok(());
    }
    let source = crate::source::Source::of_state(p, &state.id)?;
    let temp = crate::repo::Temp::directory(p.work_dir().join(crate::store::temp_name()), 0o700)?;
    let private = temp.path.join("objects");
    std::fs::create_dir(&private)?;
    let (_leased, mut written) =
        Store::write_private_tree(&private, Keying::Plain, &SnapshotSource(&source), &HOT_STATE_WRITE)?;
    written.mode = layr_objects::write::WriteMode::Full { reason: layr_objects::write::FullReason::NoCheckedDelta };

    // The filesystem flush covered all files and internal directory entries. Persist the new
    // public directory name before installing the state ref or appending the signed record.
    match renameat_with(CWD, &private, CWD, &root, RenameFlags::NOREPLACE) {
        // Another opener created the public store. The normal record writer will use it.
        Err(rustix::io::Errno::EXIST) => return Ok(()),
        result => result?,
    }
    File::open(&temp.path)?.sync_all()?;
    File::open(&meta)?.sync_all()?;
    describe_write(state, &written)?;
    install_prepared(p, state)
}

/// A shared lease protects objects encoded before a record can name their roots.
/// Acquire before preparation; pruning always tries its exclusive side without waiting.
pub(crate) struct PreparationLease {
    _file: std::fs::File,
}

fn lease_file(p: &Project) -> Result<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(p.meta_dir().join("object-preparation.lock"))?)
}

pub(crate) fn preparation_lease(p: &Project) -> Result<PreparationLease> {
    let file = lease_file(p)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)?;
    Ok(PreparationLease { _file: file })
}

/// Maintenance never waits for an in-flight preparation while holding a project lock.
pub(crate) fn try_preparation_exclusive(p: &Project) -> Result<Option<std::fs::File>> {
    let file = lease_file(p)?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Encode a caller-owned immutable capture without installing a state ref or retirement marker.
pub(crate) fn prepare_state(repo: &Repo, state: &mut crate::model::StateRec, source: &crate::repo::Snap) -> Result<()> {
    if !enabled(&repo.p)? {
        return Ok(());
    }
    let store = open(&repo.p)?;
    let options = layr_objects::WriteOptions {
        temperature: layr_objects::Temperature::Hot,
        previous: None,
        exclude: Vec::new(),
        similar: false,
    };
    // Input pins prevent native retirement while the incremental writer reads the parent.
    let incremental = state_write_base(&repo.p, &store, &state.id, state.parents.first().map(String::as_str), source)?;
    let source = SnapshotSource(source);
    let written = match incremental {
        Some((refs, paths)) => layr_objects::write::write_changed_from(
            &store,
            &source,
            &layr_objects::write::Base { tree: refs.tree, times: refs.times },
            &paths,
            &options,
        )?,
        None => layr_objects::write::write_tree_from(&store, &source, &options)?,
    };
    describe_write(state, &written)?;
    Ok(())
}

/// The short publication half; record_states skips these already encoded states.
pub(crate) fn install_prepared(p: &Project, state: &crate::model::StateRec) -> Result<()> {
    if let (Some(tree), Some(times)) = (&state.tree, &state.times) {
        open(p)?.set_state(&state.id, &StateRef { tree: Id::parse(tree)?, times: Id::parse(times)? })?;
        if !p.is_overlay() || state.cache_of.is_some() {
            queue_native(p, &state.id)?;
        }
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
        // A snapshot of a fast line already has its objects (`object_tree::register`).
        #[cfg(target_os = "linux")]
        if let Some(t) = crate::object_tree::registered(&p.state_path(&state.id)) {
            store.set_state(&state.id, &t.refs)?;
            state.tree = Some(t.refs.tree.hex());
            state.times = Some(t.refs.times.hex());
            if !p.is_overlay() || state.cache_of.is_some() {
                queue_native(p, &state.id)?;
            }
            continue;
        }
        let source = crate::source::Source::of_state(p, &state.id)?;
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
                &HOT_STATE_WRITE,
            )?,
            None => {
                let mut report = layr_objects::write::write_tree_from(&store, &source, &HOT_STATE_WRITE)?;
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
    let delta = match crate::source::storage::stored_delta(p, state)? {
        Some(delta) => Some(delta),
        None => crate::source::storage::capture_delta(source)?,
    };
    // A base that cannot be read gives a full write (#452); an I/O error stops the write.
    let readable = |state: &str| match store.checked_state(state) {
        Err(error) if layr_objects::store::is_unreadable(error.as_ref()) => Ok(None),
        result => result,
    };
    if let Some((base, paths)) = delta {
        if let Some(refs) = readable(&base)? {
            return Ok(Some((refs, paths)));
        }
    }
    let Some(parent_state) = parent else { return Ok(None) };
    let Some(refs) = readable(parent_state)? else { return Ok(None) };
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

/// The objects of the native tree `dst`, written for the paths that differ from `parent` (a
/// read-only subvolume whose content has the objects `refs`).
pub fn changed_objects(store: &Store, refs: StateRef, parent: &std::path::Path, dst: &std::path::Path) -> Result<StateRef> {
    let parent = crate::repo::snap(parent);
    let source = crate::repo::snap(dst);
    let (base, paths) = changed_write_base(refs, &parent, &source)?.context("no incremental base")?;
    let options = layr_objects::WriteOptions {
        temperature: layr_objects::Temperature::Hot,
        previous: None,
        exclude: Vec::new(),
        similar: false,
    };
    let written = layr_objects::write::write_changed_from(
        store,
        &SnapshotSource(&source),
        &layr_objects::write::Base { tree: base.tree, times: base.times },
        &paths,
        &options,
    )?;
    Ok(StateRef {
        tree: written.tree.context("object write gave no tree")?,
        times: written.times.context("object write gave no times")?,
    })
}

fn changed_write_base(
    refs: StateRef,
    parent: &crate::repo::Snap,
    source: &crate::repo::Snap,
) -> Result<Option<(StateRef, Vec<Vec<u8>>)>> {
    use crate::fsutil::Src;
    let _comparison = crate::trace::span("objects full comparison");
    if let (Some(_), Some(folder)) = (parent.objects(), source.folder()) {
        let comparison = crate::changes::compare_for_write(parent, folder)?;
        // This path-only result includes dirty folders so write_changed_from preserves
        // their metadata by conservatively importing their subtree.
        let paths = comparison.changed.into_iter().chain(comparison.dirty).collect();
        return Ok(Some((refs, paths)));
    }
    // Without sparse evidence for this base, compare the complete logical readers.
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
    let dir = repo.p.meta_dir().join("object-native");
    if !dir.is_dir() {
        return Ok(());
    }
    retire_native_locked(repo, crate::retention::overlay_named_states(repo)?, None)
}

/// Reading these markers is also safe before the project lock: the durable work list
/// remains for the next pass, and removal still checks protection under the lock.
fn native_markers(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut markers =
        entries.map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned())).collect::<std::io::Result<Vec<_>>>()?;
    markers.sort_unstable();
    Ok(markers)
}

pub(crate) fn finish_native_retirement(
    ctx: &crate::ctx::Ctx,
    name: &str,
    project_dir: &Path,
    conn: Option<&rusqlite::Connection>,
) -> Result<()> {
    use crate::db::Db;
    if conn.is_none() && !project_dir.try_exists()? {
        return Ok(());
    }
    let mut markers = native_markers(&project_dir.join("meta/object-native")).ok();
    if let Some((conn, markers)) = conn.zip(markers.as_mut()) {
        if Db::has_drop_rows(conn).is_ok_and(|has_drops| !has_drops) {
            if !markers.is_empty() {
                if let Ok(keep) = native_bases(project_dir, name, markers, || Db::read_lines(conn)) {
                    // This observation can keep a copy. Any candidate left must be checked under the lock.
                    markers.retain(|id| !keep.contains(id));
                }
            }
            if markers.is_empty() {
                return Ok(());
            }
        }
    }
    let Some(p) = Project::open_maintenance(&ctx.store, name)? else { return Ok(()) };
    let repo = Repo { restore_from_remote: true, p, ctx: Some(ctx) };
    retire_native_maintenance(&repo, markers)
}

/// A command's free step leaves busy native copies for the next pass.
pub(crate) fn retire_native_maintenance(repo: &Repo, markers: Option<Vec<String>>) -> Result<()> {
    let crate::store::LockAttempt::Acquired(_lock) = repo.p.try_lock()? else { return Ok(()) };
    let Some(keep) = crate::retention::overlay_named_states_maintenance(repo)? else { return Ok(()) };
    retire_native_locked(repo, keep, markers)
}

fn retire_native_locked(repo: &Repo, mut keep: BTreeSet<String>, markers: Option<Vec<String>>) -> Result<()> {
    let Some(_lease) = try_preparation_exclusive(&repo.p)? else { return Ok(()) };
    let dir = repo.p.meta_dir().join("object-native");
    let from_preflight = markers.is_some();
    let markers = match markers {
        Some(markers) => markers,
        None => native_markers(&dir)?,
    };
    if !markers.is_empty() {
        #[cfg(target_os = "macos")]
        keep.extend(crate::views::protected_states(&repo.p));
        #[cfg(target_os = "macos")]
        keep.extend(crate::views::kept_seeds(&repo.p));
        // A concurrent writer could change which heads are protected between preflight and lock.
        keep.extend(native_bases(&repo.p.dir, &repo.p.name, &markers, || repo.p.db.lines())?);
    }
    let mut ids = Vec::new();
    for id in markers {
        let Some(state) = repo.p.db.state(&id)? else { continue };
        if keep.contains(&id) || state.kind.auxiliary() || (from_preflight && !dir.join(&id).try_exists()?) {
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

/// Off macOS, a base project's next incremental object write needs the native head of
/// each line with a working folder. The marker stays so a later pass can retire the
/// copy once a newer state replaces the head or the working folder goes.
fn native_bases(
    project_dir: &Path,
    name: &str,
    markers: &[String],
    lines: impl FnOnce() -> Result<Vec<crate::model::Line>>,
) -> Result<BTreeSet<String>> {
    let mut keep = BTreeSet::new();
    if cfg!(target_os = "macos") || name.contains('%') {
        return Ok(keep);
    }
    let folders = LineFolders::read(project_dir)?;
    for line in lines()? {
        if markers.binary_search(&line.head).is_ok() && folders.path(project_dir, &line.id, &line.name).try_exists()? {
            keep.insert(line.head);
        }
    }
    Ok(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_damage_stays_typed_through_the_file_system_adapter() {
        use layr_objects::store::{is_unreadable, Unreadable};
        // The object tree hands store errors to the file system layer (`FsError::backend`).
        let damage = anyhow::Error::new(Unreadable("object x is empty".into())).context("read d/a");
        let through = anyhow::Error::from(layr_fs_core::FsError::backend(damage)).context("final save");
        assert!(is_unreadable(through.as_ref()), "{through:#}");
        let busy = anyhow::Error::from(layr_fs_core::FsError::Busy).context("final save");
        assert!(!is_unreadable(busy.as_ref()));
    }

    #[test]
    #[cfg_attr(feature = "overlayfs", ignore = "OverlayFS registry requires root")]
    fn enable_records_the_project_setting_without_changing_existing_projects() -> Result<()> {
        // The OverlayFS registry of a temporary store needs the root service (overlayfs.sh runs ignored tests too).
        if cfg!(feature = "overlayfs") {
            return Ok(());
        }
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
        let repo = Repo {
            restore_from_remote: true,
            p: Project::create(&ctx.store, "p", &ctx.actor(), serde_json::json!({}))?,
            ctx: None,
        };
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

    struct CountingReader {
        reader: std::sync::Arc<dyn layr_fs_core::TreeRead>,
        opened: std::sync::Mutex<Vec<Vec<u8>>>,
    }
    impl layr_fs_core::TreeRead for CountingReader {
        fn kind(&self, path: &layr_fs_core::RelDir) -> layr_fs_core::Result<Option<crate::fsutil::FType>> {
            self.reader.kind(path)
        }
        fn metadata(&self, path: &layr_fs_core::RelDir) -> layr_fs_core::Result<Option<layr_fs_core::EntryMetadata>> {
            self.reader.metadata(path)
        }
        fn entries(
            &self,
            path: &layr_fs_core::RelDir,
            visit: &mut dyn FnMut(&layr_fs_core::RelPath) -> layr_fs_core::Result<()>,
        ) -> layr_fs_core::Result<()> {
            self.reader.entries(path, visit)
        }
        fn open_file(&self, path: &layr_fs_core::RelPath) -> layr_fs_core::Result<Box<dyn layr_fs_core::ReadSeek + '_>> {
            self.opened.lock().unwrap().push(path.as_bytes().to_vec());
            self.reader.open_file(path)
        }
        fn read_link(&self, path: &layr_fs_core::RelPath) -> layr_fs_core::Result<Vec<u8>> {
            self.reader.read_link(path)
        }
    }

    #[test]
    fn sparse_layer_write_does_not_read_unchanged_contents() -> Result<()> {
        use layr_fs_common::{LayeredTree, ReadLayer};
        let root = std::env::temp_dir().join(format!("layr-sparse-object-base-{}", crate::ids::new_id()));
        let folder = root.join("source");
        let layer = root.join("private");
        std::fs::create_dir_all(folder.join("sub"))?;
        std::fs::create_dir_all(layer.join("sub"))?;
        for i in 0..100 {
            std::fs::write(folder.join(format!("sub/f{i}")), format!("original {i}"))?;
        }
        let store = Store::open(&root.join("objects"), Keying::Plain)?;
        let original = layr_objects::write_tree(&store, &folder, &options())?;
        let refs = StateRef { tree: original.tree.unwrap(), times: original.times.unwrap() };
        let objects =
            crate::object_tree::ObjectTree::new(Store::open(&root.join("objects"), Keying::Plain)?, refs, &root.join("reads"))?;
        std::fs::write(layer.join("sub/f9"), b"automatic change")?;
        let source = CountingReader {
            reader: std::sync::Arc::new(LayeredTree::new(vec![
                ReadLayer { reader: crate::fs::reader(&layer)?, removed: Default::default() },
                ReadLayer { reader: objects, removed: Default::default() },
            ])?),
            opened: Default::default(),
        };
        // The same path collector serves installed and private APFS layers. Only the
        // sparse physical layer is scanned; its inherited siblings are never candidates.
        let scanned_paths = || -> Result<Vec<String>> {
            let mut paths = Vec::new();
            crate::fsutil::walk(&layer, "", &mut |path, _| {
                paths.push(path.to_owned());
                Ok(true)
            })?;
            Ok(paths)
        };
        let paths = crate::source::storage::layer_paths(scanned_paths()?, []);
        assert_eq!(paths, [b"sub".to_vec(), b"sub/f9".to_vec()]);
        let base = layr_objects::write::Base { tree: refs.tree, times: refs.times };
        let report = layr_objects::write::write_changed_from(&store, &SnapshotSource(&source), &base, &paths, &options())?;
        assert_eq!(report.mode, layr_objects::write::WriteMode::Incremental { changed_paths: 2 });
        assert_eq!(report.files, 1);
        assert_eq!(*source.opened.lock().unwrap(), [b"sub/f9".to_vec()]);
        let full = layr_objects::write::write_tree_from(&store, &SnapshotSource(&source), &options())?;
        assert_eq!((report.tree, report.times), (full.tree, full.times));
        drop(source);
        // Removals and both sides of a rename must use the layered source too.
        std::fs::copy(folder.join("sub/f11"), layer.join("sub/renamed"))?;
        let removed = ["sub/f10", "sub/f11"].map(str::to_owned);
        let objects =
            crate::object_tree::ObjectTree::new(Store::open(&root.join("objects"), Keying::Plain)?, refs, &root.join("reads"))?;
        let source = CountingReader {
            reader: std::sync::Arc::new(LayeredTree::new(vec![
                ReadLayer {
                    reader: crate::fs::reader(&layer)?,
                    removed: removed
                        .iter()
                        .map(|path| layr_fs_core::RelPath::new(path.as_bytes()))
                        .collect::<layr_fs_core::Result<_>>()?,
                },
                ReadLayer { reader: objects, removed: Default::default() },
            ])?),
            opened: Default::default(),
        };
        let paths = crate::source::storage::layer_paths(scanned_paths()?, removed);
        assert_eq!(paths.len(), 5);
        assert!(paths.contains(&b"sub/f10".to_vec()) && paths.contains(&b"sub/f11".to_vec()));
        let report = layr_objects::write::write_changed_from(&store, &SnapshotSource(&source), &base, &paths, &options())?;
        assert_eq!(report.files, 2);
        let mut opened = source.opened.lock().unwrap().clone();
        opened.sort();
        assert_eq!(opened, [b"sub/f9".to_vec(), b"sub/renamed".to_vec()]);
        let full = layr_objects::write::write_tree_from(&store, &SnapshotSource(&source), &options())?;
        assert_eq!((report.tree, report.times), (full.tree, full.times));
        let tree = crate::object_tree::ObjectTree::new(
            Store::open(&root.join("objects"), Keying::Plain)?,
            StateRef { tree: report.tree.unwrap(), times: report.times.unwrap() },
            &root.join("result-reads"),
        )?;
        assert!(tree.entry("sub/f10")?.is_none() && tree.entry("sub/f11")?.is_none());
        assert_eq!(tree.read("sub/renamed")?, b"original 11");
        // Native ACLs cannot be represented by the object format. A sparse layer
        // must retain the same explicit rejection as a complete encoding.
        use layr_fs_core::{TreeRead, TreeWrite};
        let mut attributes = source.metadata(&layr_fs_core::RelDir::Root)?.unwrap().attributes;
        let acl_layer = layr_fs_plain::MemoryTree::new(attributes.clone());
        acl_layer.put(&layr_fs_core::RelPath::new(b"sub")?, layr_fs_core::EntryData::Directory, &attributes)?;
        attributes.acl = Some(b"native ACL bytes".to_vec());
        acl_layer.put(
            &layr_fs_core::RelPath::new(b"sub/f9")?,
            layr_fs_core::EntryData::File(&mut &b"automatic change"[..]),
            &attributes,
        )?;
        let acl_source = LayeredTree::new(vec![
            ReadLayer { reader: std::sync::Arc::new(acl_layer), removed: Default::default() },
            ReadLayer { reader: source.reader.clone(), removed: Default::default() },
        ])?;
        let error =
            layr_objects::write::write_changed_from(&store, &SnapshotSource(&acl_source), &base, &paths, &options()).unwrap_err();
        assert!(error.to_string().contains("native ACL"));
        assert!(layr_objects::write::write_tree_from(&store, &SnapshotSource(&acl_source), &options())
            .unwrap_err()
            .to_string()
            .contains("native ACL"));
        drop(tree);
        drop(source);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn private_apfs_layer_uses_its_base_regardless_of_parent() -> Result<()> {
        use std::{collections::BTreeSet, os::unix::fs::PermissionsExt};
        // Clones refuse symbolic links in their paths, and macOS's /tmp is one.
        let root = std::env::temp_dir().canonicalize()?.join(format!("layr-private-layer-base-{}", crate::ids::new_id()));
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
        let folder = root.join("source");
        std::fs::create_dir_all(folder.join("sub"))?;
        for i in 0..100 {
            std::fs::write(folder.join(format!("sub/f{i}")), format!("original {i}"))?;
        }
        let objects = open(&p)?;
        let original = layr_objects::write_tree(&objects, &folder, &options())?;
        let refs = StateRef { tree: original.tree.unwrap(), times: original.times.unwrap() };
        let base_id = crate::ids::new_id();
        objects.set_state(&base_id, &refs)?;
        // Another record parent disagrees outside the sparse layer. The writer must
        // reuse the actual layer base, including its original f99 entry.
        std::fs::write(folder.join("sub/f99"), b"other parent")?;
        let other = layr_objects::write_tree(&objects, &folder, &options())?;
        let other_id = crate::ids::new_id();
        objects.set_state(&other_id, &StateRef { tree: other.tree.unwrap(), times: other.times.unwrap() })?;
        std::fs::write(folder.join("sub/f99"), b"original 99")?;
        std::fs::write(folder.join("sub/f9"), b"automatic change")?;
        std::fs::remove_file(folder.join("sub/f10"))?;
        std::fs::rename(folder.join("sub/f11"), folder.join("sub/renamed"))?;
        std::fs::remove_file(folder.join("sub/f12"))?;
        std::fs::create_dir(folder.join("sub/f12"))?;
        std::fs::set_permissions(folder.join("sub/f13"), std::fs::Permissions::from_mode(0o600))?;
        layr_objects::fsx::set_xattrs(&folder.join("sub/f14"), &[(b"user.test".to_vec(), b"new".to_vec())])?;
        layr_objects::times::set(&folder.join("sub/f15"), 1_700_000_000_123_456_789)?;
        layr_objects::times::set(&folder.join("sub"), 1_700_000_000_234_567_890)?;
        let diff = crate::thin::Diff {
            write: ["sub/f9", "sub/renamed", "sub/f12", "sub/f13", "sub/f14", "sub/f15"].map(str::to_owned).into(),
            removed: ["sub/f10", "sub/f11", "sub/f12"].map(str::to_owned).into(),
            whole: BTreeSet::new(),
        };
        let layer = root.join("private");
        let capture = crate::thin::write_layer_owned(&folder, &layer, &diff, &base_id, None)?.freeze()?;
        let parent = crate::object_tree::ObjectTree::new(open(&p)?, refs, &root.join("reads"))?;
        let source: crate::source::Source = crate::thin::Layered::over(&layer, crate::thin::Layered::objects(parent))?.into();
        assert!(matches!(source, crate::source::Source::Layers(_)));
        let state = crate::ids::new_id();
        assert!(!p.delta_path(&state).exists(), "the capture is still private");
        let evidence = crate::source::storage::capture_delta(&source)?.expect("layer base");
        assert_eq!(evidence.0, base_id);
        assert_eq!(evidence.1.len(), 9);
        assert!(!evidence.1.contains(&b"sub/f99".to_vec()), "inherited siblings are not candidates");
        let expected = layr_objects::write::write_tree_from(&objects, &SnapshotSource(&source), &options())?;
        // Sparse evidence chooses its own base even with a different or absent record parent.
        for parent in [Some(base_id.as_str()), Some(other_id.as_str()), None] {
            let (base, paths) = state_write_base(&p, &objects, &state, parent, &source)?.expect("object layer base");
            assert_eq!(base, refs);
            assert_eq!(paths, evidence.1);
            let report = layr_objects::write::write_changed_from(
                &objects,
                &SnapshotSource(&source),
                &layr_objects::write::Base { tree: base.tree, times: base.times },
                &paths,
                &options(),
            )?;
            assert_eq!((report.tree, report.times), (expected.tree, expected.times));
        }
        drop(source);
        // Publication changes only the location, never the sparse evidence.
        std::fs::create_dir_all(p.deltas_dir())?;
        capture.release();
        crate::apfs::move_tree(&layer, &p.delta_path(&state))?;
        assert_eq!(crate::source::storage::stored_delta(&p, &state)?, Some(evidence));
        drop(objects);
        drop(p);
        drop(store);
        crate::fs::remove(&root)?;
        Ok(())
    }

    #[test]
    fn object_base_keeps_incremental_writes_for_sources_without_a_folder() -> Result<()> {
        use crate::fsutil::Src;
        use layr_objects::write::WriteMode;
        let root = std::env::temp_dir().join(format!("layr-reader-write-base-{}", crate::ids::new_id()));
        let folder = root.join("source");
        std::fs::create_dir_all(folder.join("sub"))?;
        for i in 0..100 {
            std::fs::write(folder.join(format!("sub/f{i}")), format!("original {i}"))?;
        }
        let store = Store::open(&root.join("objects"), Keying::Plain)?;
        let original = layr_objects::write_tree(&store, &folder, &options())?;
        let refs = StateRef { tree: original.tree.unwrap(), times: original.times.unwrap() };
        let parent = crate::source::Source::Objects(crate::object_tree::ObjectTree::new(
            Store::open(&root.join("objects"), Keying::Plain)?,
            refs,
            &root.join("parent-reads"),
        )?);
        std::fs::write(folder.join("sub/f9"), b"automatic change")?;
        let full = layr_objects::write_tree(&store, &folder, &options())?;
        let source = crate::source::Source::Objects(crate::object_tree::ObjectTree::new(
            Store::open(&root.join("objects"), Keying::Plain)?,
            StateRef { tree: full.tree.unwrap(), times: full.times.unwrap() },
            &root.join("child-reads"),
        )?);
        assert!(source.folder().is_none(), "private layered captures also have no single folder");
        let (base, paths) = changed_write_base(refs, &parent, &source)?.expect("reader-only sources must keep their delta");
        assert_eq!(paths, [b"sub/f9".to_vec()]);
        let report = layr_objects::write::write_changed_from(
            &store,
            &SnapshotSource(&source),
            &layr_objects::write::Base { tree: base.tree, times: base.times },
            &paths,
            &options(),
        )?;
        assert_eq!(report.mode, WriteMode::Incremental { changed_paths: 1 });
        assert_eq!(report.files, 1);
        assert_eq!((report.tree, report.times), (full.tree, full.times));
        drop(source);
        drop(parent);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn object_only_parent_writes_five_files_without_resolving_unchanged_files() -> Result<()> {
        if crate::test_runs_as_root() {
            return Ok(());
        }
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
        let compared = compare(&parent, crate::repo::snap(&folder), None)?;
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
        let compared = compare(&parent, crate::repo::snap(&folder), None)?;
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
