//! Portable state objects are the durable source after migration. Generic readers use
//! ObjectTree directly. Native-only consumers can request a disposable tree through
//! build_state; retire_native removes that copy after the command releases its readers.

use crate::repo::Repo;
use crate::store::Project;
use anyhow::{Context, Result};
use layr_objects::{Id, Keying, Node, StateRef, Store};
use std::path::{Path, PathBuf};

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
    #[allow(unused_mut)]
    let mut store = Store::open(&dir(p), Keying::Plain)?;
    // A sealed APFS state has immutable files, and a clone keeps the flag.
    #[cfg(target_os = "macos")]
    {
        store.clone_hook = Some(crate::sys::apfs_unseal_fd);
    }
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
    std::fs::create_dir_all(repo.p.work_dir())?;
    let work = repo.p.work_dir().join(crate::store::temp_name());
    let made = (|| -> Result<()> {
        crate::storage::create_subvolume(&work)?;
        layr_objects::checkout_with(&store, &r.tree, Some(&r.times), &work)?;
        crate::storage::snapshot(&work, &repo.p.state_path(state), true)
    })();
    let _ = crate::storage::delete_tree(&work);
    if let Err(e) = made {
        let _ = crate::storage::delete_tree(&repo.p.state_path(state));
        return Err(e.context(format!("cannot build state {} from the object store", crate::ids::short(state))));
    }
    queue_native(&repo.p, state)?;
    repo.p.lock_or_borrow()?.write(|tx| tx.set_present(state, true))?;
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

/// Enable object writes only after a successful migration. This is a project setting so other
/// updated writers also keep new history in the portable format.
pub const ENABLED: &str = "objects.enabled";

// Select the native reader once; backends without placeholders use the ordinary kind.
pub(crate) const ENTRY_KIND: fn(&Path, &std::fs::Metadata) -> Result<layr_objects::fsx::EntryKind> = {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        crate::storage::entry_kind
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    {
        |_, _| Ok(layr_objects::fsx::EntryKind::Ordinary)
    }
};

pub struct SnapshotSource<'a>(pub &'a crate::repo::Snap);
impl layr_objects::write::Source for SnapshotSource<'_> {
    fn entry_kind(&self, path: &Path, meta: &std::fs::Metadata) -> Result<layr_objects::fsx::EntryKind> {
        ENTRY_KIND(path, meta)
    }

    fn path(&self, relative: &[u8]) -> Result<PathBuf> {
        use crate::fsutil::Src;
        let path = self.0.at(std::str::from_utf8(relative).context("a layr path is not UTF-8")?)?;
        #[cfg(target_os = "macos")]
        if path.symlink_metadata().is_ok() && crate::sys::apfs_has_acl(&path)? {
            anyhow::bail!("cannot migrate {}: native ACLs are not represented by the object format", path.display());
        }
        Ok(path)
    }

    fn names(&self, relative: &[u8]) -> Result<Vec<Vec<u8>>> {
        use crate::fsutil::Src;
        use std::os::unix::ffi::OsStrExt;
        if self.0.folder().is_some() {
            // A complete immutable folder already defines the visible names. Resolving
            // each sibling here would defeat the changed writer's metadata pruning.
            return std::fs::read_dir(self.path(relative)?)?
                .map(|entry| Ok(entry?.file_name().as_bytes().to_vec()))
                .collect();
        }
        let rel = std::str::from_utf8(relative).context("a layr path is not UTF-8")?;
        let mut names = std::collections::BTreeSet::new();
        #[cfg(target_os = "macos")]
        let folders: Vec<PathBuf> = self.0.layers.iter().map(|l| l.root.join(rel)).collect();
        #[cfg(not(target_os = "macos"))]
        let folders = self.0.folder().map(|p| vec![p.join(rel)]).unwrap_or_default();
        #[cfg(target_os = "macos")]
        let objects = self.0.object_base.as_deref();
        #[cfg(not(target_os = "macos"))]
        let objects = crate::fsutil::Src::objects(self.0);
        if let Some(objects) = objects {
            objects.walk(rel, &mut |path, _| {
                let child = path.rsplit('/').next().unwrap();
                #[cfg(target_os = "macos")]
                let present = self.0.entry(path)?.is_some();
                #[cfg(not(target_os = "macos"))]
                let present = objects.entry(path)?.is_some();
                if present { names.insert(child.as_bytes().to_vec()); }
                Ok(false)
            })?;
        }
        for folder in folders {
            match std::fs::symlink_metadata(&folder) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR) => continue,
                Err(e) => return Err(e.into()),
            }
            for entry in std::fs::read_dir(folder)? {
                let name = entry?.file_name().as_bytes().to_vec();
                let child = if relative.is_empty() { name.clone() } else { [relative, b"/", &name].concat() };
                match std::fs::symlink_metadata(self.path(&child)?) {
                    Ok(_) => {
                        names.insert(name);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(names.into_iter().collect())
    }
}

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
    Ok(on(crate::db::Db::open(&path)?.config(ENABLED)?))
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
    #[cfg(target_os = "macos")]
    let files = crate::storage::with_stored_compare(|| crate::changes::file_changes(parent, &source))?;
    #[cfg(not(target_os = "macos"))]
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
pub fn record_states(p: &Project, records: &mut [crate::model::Record]) -> Result<()> {
    if !enabled(p)? {
        return Ok(());
    }
    let store = open(p)?;
    for state in records.iter_mut().flat_map(|r| &mut r.states) {
        if state.kind.auxiliary() || state.tree.is_some() || !p.has_state(&state.id) {
            continue;
        }
        #[cfg(target_os = "macos")]
        let source = crate::thin::Layered::of_state(p, &state.id)?;
        #[cfg(not(target_os = "macos"))]
        let source = crate::repo::snap(&p.state_path(&state.id));
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        let incremental = state_write_base(p, &store, &state.id, state.parents.first().map(String::as_str), &source)?;
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

#[cfg(target_os = "macos")]
pub fn incremental_base(p: &Project, store: &Store, state: &str) -> Result<Option<(StateRef, Vec<Vec<u8>>)>> {
    let Some(base) = crate::thin::layer_base(p, state)? else {
        return Ok(None);
    };
    let Some(refs) = store.state_ref(&base)? else {
        return Ok(None);
    };
    let delta = p.delta_path(state);
    let mut paths: std::collections::BTreeSet<String> =
        crate::storage::scan_shared(&delta, false)?.keys().filter(|s| !s.is_empty()).cloned().collect();
    paths.extend(crate::thin::removed_of(&delta)?);
    Ok(Some((refs, paths.into_iter().map(String::into_bytes).collect())))
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
    #[cfg(target_os = "macos")]
    if let Some(base) = incremental_base(p, store, state)? {
        return Ok(Some(base));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = state;
    let Some(parent_state) = parent else { return Ok(None) };
    let Some(refs) = store.state_ref(parent_state)? else { return Ok(None) };
    let parent = if p.has_state(parent_state) {
        #[cfg(target_os = "macos")]
        let source = crate::thin::Layered::of_state(p, parent_state)?;
        #[cfg(not(target_os = "macos"))]
        let source = crate::repo::snap(&p.state_path(parent_state));
        source
    } else {
        let tree = crate::object_tree::ObjectTree::new(open(p)?, refs, &p.meta_dir().join("object-reads"))?;
        #[cfg(target_os = "macos")]
        let source = crate::thin::Layered::objects(tree);
        #[cfg(not(target_os = "macos"))]
        let source = crate::repo::Snap::Objects(tree);
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
    // Portable readers can supply ordinary folders. A btrfs send stream requires both
    // roots to be subvolumes; keep ioctl failures visible when those prerequisites hold.
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    if !crate::fsutil::Src::folder(parent).is_some_and(crate::storage::is_subvolume)
        || !crate::fsutil::Src::folder(source).is_some_and(crate::storage::is_subvolume)
    {
        return Ok(None);
    }
    #[cfg(target_os = "macos")]
    let changes = crate::storage::with_stored_compare(|| crate::changes::candidates(parent, source))?;
    #[cfg(not(target_os = "macos"))]
    let changes = crate::changes::candidates(parent, source)?;
    let mut paths = std::collections::BTreeSet::new();
    for change in changes {
        // OverlayFS can report unchanged entries after a complete snapshot copy. Do not
        // turn those candidates into full subtree reads. Linked files remain candidates:
        // equal bytes and link counts alone do not prove equal hard-link groups.
        #[cfg(feature = "overlayfs")]
        if change.old_path().is_none() && change.old().is_some() && change.new().is_some() {
            use std::os::unix::fs::MetadataExt;
            let a = crate::fsutil::Src::at(parent, change.path())?;
            let b = crate::fsutil::Src::at(source, change.path())?;
            let am = std::fs::symlink_metadata(&a)?;
            let bm = std::fs::symlink_metadata(&b)?;
            let same_data = (am.is_dir() && bm.is_dir())
                || (am.is_file()
                    && bm.is_file()
                    && am.nlink() == 1
                    && bm.nlink() == 1
                    && matches!(change.data(), crate::compare::DataEvidence::Equal));
            if same_data
                && am.mode() == bm.mode()
                && (am.mtime(), am.mtime_nsec()) == (bm.mtime(), bm.mtime_nsec())
                && layr_objects::fsx::xattrs(&a)? == layr_objects::fsx::xattrs(&b)?
            {
                continue;
            }
        }
        paths.insert(change.path().clone());
        paths.extend(change.old_path().cloned());
    }
    #[cfg(target_os = "macos")]
    {
        let old = parent.scan(false)?;
        for (path, entry) in source.scan(false)? {
            if !path.is_empty()
                && old.get(&path).is_none_or(|previous| {
                    (previous.fingerprint.4, previous.fingerprint.5) != (entry.fingerprint.4, entry.fingerprint.5)
                })
            {
                paths.insert(path);
            }
        }
    }
    Ok(Some((refs, paths.into_iter().map(String::into_bytes).collect())))
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
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let id = entry?.file_name().to_string_lossy().into_owned();
        let Some(state) = repo.p.db.state(&id)? else { continue };
        if keep.contains(&id) || state.kind.auxiliary() { continue; }
        if verified(&repo.p, &id)?.is_none() { continue; }
        ids.push(id);
    }
    let gone = crate::retention::drop_trees(repo, &ids)?;
    for id in gone { std::fs::remove_file(dir.join(id))?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let parent = crate::thin::Layered::objects(tree);
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
        let parent = crate::thin::Layered::objects(tree);
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
    fn native_base_keeps_metadata_changes_and_requires_subvolumes_for_send() -> Result<()> {
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
        #[cfg(not(feature = "overlayfs"))]
        assert!(base.is_none(), "ordinary folders cannot supply a btrfs send stream");
        #[cfg(feature = "overlayfs")]
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
