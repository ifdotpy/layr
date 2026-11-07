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
