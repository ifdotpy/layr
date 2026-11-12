//! Portable state objects are the durable source after migration. Generic readers use
//! ObjectTree directly. Native-only consumers can request a disposable tree through
//! build_state; retire_native removes that copy after the command releases its readers.

use crate::repo::Repo;
use crate::store::Project;
use anyhow::{Context, Result};
use layr_objects::{Id, Keying, Node, StateRef, Store};
use std::path::PathBuf;

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
    let _lock = (!repo.p.holds_lock()).then(|| repo.p.lock()).transpose()?;
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
    repo.p.db.set_present(state, true)?;
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

pub struct SnapshotSource<'a>(pub &'a crate::repo::Snap);
impl layr_objects::write::Source for SnapshotSource<'_> {
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
        use std::os::unix::ffi::OsStrExt;
        let rel = std::str::from_utf8(relative).context("a layr path is not UTF-8")?;
        let mut names = std::collections::BTreeSet::new();
        #[cfg(target_os = "macos")]
        let folders: Vec<PathBuf> = self.0.layers.iter().map(|l| l.root.join(rel)).collect();
        #[cfg(not(target_os = "macos"))]
        let folders = {
            use crate::fsutil::Src;
            self.0.folder().map(|p| vec![p.join(rel)]).unwrap_or_default()
        };
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
        let source = SnapshotSource(&source);
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        #[cfg(target_os = "macos")]
        let incremental = incremental_base(p, &store, &state.id)?;
        #[cfg(not(target_os = "macos"))]
        let incremental: Option<(StateRef, Vec<Vec<u8>>)> = None;
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
        let refs = StateRef {
            tree: written.tree.context("object write gave no tree")?,
            times: written.times.context("object write gave no times")?,
        };
        store.set_state(&state.id, &refs)?;
        state.tree = Some(refs.tree.hex());
        state.times = Some(refs.times.hex());
        // A part of a build-output overlay (`cache_of`) gives up its native copy too: the cache code reads it through
        // `state_root`, which builds the copy again from these objects. Other overlays keep theirs for now: their
        // code reads parts as folders.
        if !p.is_overlay() || state.cache_of.is_some() {
            queue_native(p, &state.id)?;
        }
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
    #[cfg(target_os = "macos")]
    if let Some(base) = incremental_base(&repo.p, store, state)? {
        return Ok(Some(base));
    }
    let Some(parent_state) = parent else {
        return Ok(None);
    };
    let Some(refs) = store.state_ref(parent_state)? else {
        return Ok(None);
    };
    if !repo.p.has_state(parent_state) {
        return Ok(None);
    }
    let _pin = repo.pin(&[parent_state])?;
    let parent = repo.state_src(parent_state)?;
    // Portable readers can supply ordinary folders. A btrfs send stream requires both
    // roots to be subvolumes; keep ioctl failures visible when those prerequisites hold.
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    if !crate::fsutil::Src::folder(&parent).is_some_and(crate::storage::is_subvolume) || !crate::fsutil::Src::folder(source).is_some_and(crate::storage::is_subvolume) {
        return Ok(None);
    }
    #[cfg(target_os = "macos")]
    let changes = crate::storage::with_stored_compare(|| crate::changes::candidates(&parent, source))?;
    #[cfg(not(target_os = "macos"))]
    let changes = crate::changes::candidates(&parent, source)?;
    let mut paths = std::collections::BTreeSet::new();
    for change in changes {
        // OverlayFS can report unchanged entries after a complete snapshot copy. Do not
        // turn those candidates into full subtree reads. Linked files remain candidates:
        // equal bytes and link counts alone do not prove equal hard-link groups.
        #[cfg(feature = "overlayfs")]
        if change.old_path.is_none() && change.old.is_some() && change.new.is_some() {
            use std::os::unix::fs::MetadataExt;
            let a = crate::fsutil::Src::at(&parent, &change.path)?;
            let b = crate::fsutil::Src::at(source, &change.path)?;
            let am = std::fs::symlink_metadata(&a)?;
            let bm = std::fs::symlink_metadata(&b)?;
            let same_data = (am.is_dir() && bm.is_dir())
                || (am.is_file()
                    && bm.is_file()
                    && am.nlink() == 1
                    && bm.nlink() == 1
                    && change.ranges.as_ref().is_some_and(|r| r.is_empty()));
            if same_data
                && am.mode() == bm.mode()
                && (am.mtime(), am.mtime_nsec()) == (bm.mtime(), bm.mtime_nsec())
                && layr_objects::fsx::xattrs(&a)? == layr_objects::fsx::xattrs(&b)?
            {
                continue;
            }
        }
        paths.insert(change.path);
        paths.extend(change.old_path);
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
    let dir = repo.p.meta_dir().join("object-native");
    if !dir.is_dir() { return Ok(()); }
    let _lock = (!repo.p.holds_lock()).then(|| repo.p.lock()).transpose()?;
    #[allow(unused_mut)]
    let mut keep = crate::cmd::admin::overlay_named_states(repo)?;
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
    let gone = crate::cmd::admin::drop_trees(repo, &ids)?;
    for id in gone { std::fs::remove_file(dir.join(id))?; }
    Ok(())
}

/// Drop from the object store of `p` what its kept states do not reach (`Store::prune`), under the project lock, so no
/// save publishes a state while the walk runs. None when `p` has no object store.
pub fn prune(p: &Project) -> Result<Option<layr_objects::store::PruneReport>> {
    if !exists(p) {
        return Ok(None);
    }
    let _lock = (!p.holds_lock()).then(|| p.lock()).transpose()?;
    let keep = p.db.kept_state_ids()?;
    Ok(Some(open(p)?.prune(&keep)?))
}
