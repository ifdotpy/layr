//! The project's object store (docs/objects.md) as a source of states. After `layr migrate`, a
//! state record names a tree and a times table (`StateRec::tree`, `times`). Readers use them:
//! `layr show <rev>:<path>` reads the entry from the trees, `layr line new` checks out the tree of
//! a state whose snapshot is not on disk, and every other reader gets a snapshot that is made
//! again from the objects (`build_state`, from `Repo::check_state`).

use crate::repo::Repo;
use crate::store::Project;
use anyhow::{Context, Result};
use layr_objects::{Id, Keying, Node, StateRef, Store};
use std::path::{Path, PathBuf};

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

/// The tree and times table that the record of `state` names, when the local object store holds
/// both. None for a state that `layr migrate` did not record, or whose objects are elsewhere (a
/// record from another machine).
pub fn verified(p: &Project, state: &str) -> Result<Option<(Store, StateRef)>> {
    if !exists(p) {
        return Ok(None);
    }
    let Some(rec) = p.db.state(state)? else { return Ok(None) };
    let (Some(tree), Some(times)) = (rec.tree.as_deref(), rec.times.as_deref()) else { return Ok(None) };
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
    let Some((store, r)) = verified(&repo.p, state)? else { return Ok(false) };
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
    repo.p.db.set_present(state, true)?;
    Ok(true)
}

/// Make the folder `dst` (absent or empty) with the tree and times of a state: hot blobs become
/// clones, other blobs are decoded.
pub fn checkout(ctx: &crate::ctx::Ctx, store: &Store, r: &StateRef, dst: &Path) -> Result<layr_objects::CheckoutReport> {
    match crate::privs::CallerFs::of(ctx)? {
        Some(caller) => layr_objects::checkout_as(store, &r.tree, Some(&r.times), dst, &|step| caller.run(step)),
        None => layr_objects::checkout_with(store, &r.tree, Some(&r.times), dst),
    }
}

/// The entry at `path` ("" is the root) of a tree, read from the objects (`layr show`).
pub fn entry(store: &Store, root: &Id, path: &str) -> Result<crate::gitbridge::Entry> {
    use crate::gitbridge::Entry;
    let mut tree = store.get_tree(root)?;
    let parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    for (i, name) in parts.iter().enumerate() {
        let Some(e) = tree.entries.iter().find(|e| e.name == name.as_bytes()) else { return Ok(Entry::Missing) };
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
        let folders = vec![self.0.join(rel)];
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

/// Attach portable objects before signing each new source state. Snapshots remain the hot
/// working representation; the record is never published before its objects have been written.
pub fn record_states(p: &Project, records: &mut [crate::model::Record]) -> Result<()> {
    if p.is_overlay() || p.config_value(ENABLED)?.and_then(|v| v.as_bool()) != Some(true) {
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
        let source = p.state_path(&state.id);
        let source = SnapshotSource(&source);
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        #[allow(unused_mut)]
        let mut incremental: Option<(StateRef, Vec<Vec<u8>>)> = None;
        #[cfg(target_os = "macos")]
        {
            incremental = incremental_base(p, &store, &state.id)?;
        }
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
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn incremental_base(p: &Project, store: &Store, state: &str) -> Result<Option<(StateRef, Vec<Vec<u8>>)>> {
    let Some(base) = crate::thin::layer_base(p, state)? else { return Ok(None) };
    let Some(refs) = store.state_ref(&base)? else { return Ok(None) };
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
    let Some(parent_state) = parent else { return Ok(None) };
    let Some(refs) = store.state_ref(parent_state)? else { return Ok(None) };
    if !repo.p.has_state(parent_state) {
        return Ok(None);
    }
    let _pin = repo.pin(&[parent_state])?;
    let parent = repo.state_src(parent_state)?;
    #[cfg(target_os = "macos")]
    let changes = crate::storage::with_stored_compare(|| crate::changes::candidates(&parent, source))?;
    #[cfg(not(target_os = "macos"))]
    let changes = crate::changes::candidates(&parent, source)?;
    let mut paths = std::collections::BTreeSet::new();
    for change in changes {
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
