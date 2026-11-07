//! Make a folder from a tree (docs/objects.md, "Checkout"). A hot blob becomes a clone of its
//! store file (no data is copied on APFS, btrfs and XFS); other blobs are decoded and written.

use crate::fsx;
use crate::id::Id;
use crate::store::Store;
use crate::tree::Node;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct CheckoutReport {
    pub files: u64,
    pub clones: u64,
    pub copies: u64,
    pub decoded: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub hard_links: u64,
    /// Entries whose modification time came from the times table.
    pub times: u64,
}

struct Run<'a> {
    store: &'a Store,
    files: AtomicU64,
    clones: AtomicU64,
    copies: AtomicU64,
    decoded: AtomicU64,
    dirs: AtomicU64,
    symlinks: AtomicU64,
    /// Folder modes, set after the folder is full (a read-only folder takes no new entries).
    modes: Mutex<Vec<(PathBuf, u32)>>,
    /// The paths of the regular files made (relative to the root): hard link groups may name only these.
    files_made: Mutex<HashSet<Vec<u8>>>,
}

/// Make the folder `dst` (absent, or an empty folder) with the content of `tree`. The owner of
/// every entry is the caller; times are the time of the checkout.
pub fn checkout(store: &Store, tree: &Id, dst: &Path) -> Result<CheckoutReport> {
    checkout_with(store, tree, None, dst)
}

/// `checkout`, and with `times` (the times table of the state) every entry gets its recorded
/// modification time, so build tools that compare times see an unchanged tree.
pub fn checkout_with(store: &Store, tree: &Id, times: Option<&Id>, dst: &Path) -> Result<CheckoutReport> {
    match fs::read_dir(dst) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                bail!("{} is not empty", dst.display());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(dst).with_context(|| format!("cannot make {}", dst.display()))?
        }
        Err(e) => return Err(e).with_context(|| dst.display().to_string()),
    }
    let run = Run {
        store,
        files: AtomicU64::new(0),
        clones: AtomicU64::new(0),
        copies: AtomicU64::new(0),
        decoded: AtomicU64::new(0),
        dirs: AtomicU64::new(0),
        symlinks: AtomicU64::new(0),
        modes: Mutex::new(Vec::new()),
        files_made: Mutex::new(HashSet::new()),
    };
    let root = store.get_tree(tree)?;
    run.folder(&root, dst, b"")?;
    // Hard links: the first path of a group stays, the others become its names. Each path must be
    // a regular file of this tree, in one group only: a path through a symbolic link or into a
    // folder would make the link step write outside `dst`.
    let made = run.files_made.into_inner().expect("no panic holds the lock");
    let mut seen = HashSet::new();
    for path in root.links.iter().flatten() {
        if !made.contains(path) || !seen.insert(path) {
            bail!("hard link path {} is not one regular file of the tree", String::from_utf8_lossy(path));
        }
    }
    let mut hard_links = 0;
    for group in &root.links {
        let first = dst.join(fsx::os(&group[0]));
        for other in &group[1..] {
            let path = dst.join(fsx::os(other));
            fs::remove_file(&path).with_context(|| path.display().to_string())?;
            fs::hard_link(&first, &path).with_context(|| path.display().to_string())?;
            hard_links += 1;
        }
    }
    let mut modes = run.modes.into_inner().expect("no panic holds the lock");
    // Deepest first: a folder that loses its write bit has no folder below it left to change.
    modes.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
    for (path, mode) in modes {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
    }
    let restored = match times {
        Some(id) => restore_times(store, &root, &store.get_times(id)?, dst)?,
        None => 0,
    };
    Ok(CheckoutReport {
        files: run.files.into_inner(),
        clones: run.clones.into_inner(),
        copies: run.copies.into_inner(),
        decoded: run.decoded.into_inner(),
        dirs: run.dirs.into_inner(),
        symlinks: run.symlinks.into_inner(),
        hard_links,
        times: restored,
    })
}

/// The paths of a tree in times order (`crate::times`): the root, then pre-order by name.
fn tree_paths(
    store: &Store,
    tree: &crate::tree::Tree,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(PathBuf, usize, bool)>,
) -> Result<()> {
    for e in &tree.entries {
        let path = dir.join(fsx::os(&e.name));
        match &e.node {
            Node::Dir { tree } => {
                out.push((path.clone(), depth + 1, true));
                tree_paths(store, &store.get_tree(tree)?, &path, depth + 1, out)?;
            }
            _ => out.push((path, depth + 1, false)),
        }
    }
    Ok(())
}

/// Set the recorded times: files and links first, then folders deepest first (a new entry
/// changes the time of its folder).
fn restore_times(store: &Store, root: &crate::tree::Tree, times: &crate::times::Times, dst: &Path) -> Result<u64> {
    let mut paths = vec![(dst.to_path_buf(), 0, true)];
    tree_paths(store, root, dst, 0, &mut paths)?;
    if paths.len() != times.records.len() || paths.iter().zip(&times.records).any(|((_, _, dir), r)| *dir != r.below.is_some()) {
        bail!("the times table does not match the tree ({} records, {} entries)", times.records.len(), paths.len());
    }
    let ns: Vec<i64> = times.records.iter().map(|r| r.ns).collect();
    let set = |(path, _, _): &(PathBuf, usize, bool), ns: &i64| -> Result<()> {
        crate::times::set(path, *ns).with_context(|| format!("cannot set the time of {}", path.display()))
    };
    paths.par_iter().zip(ns.par_iter()).filter(|((_, _, dir), _)| !dir).try_for_each(|(p, t)| set(p, t))?;
    let mut dirs: Vec<(&(PathBuf, usize, bool), &i64)> = paths.iter().zip(&ns).filter(|((_, _, dir), _)| *dir).collect();
    dirs.sort_by_key(|((_, depth, _), _)| std::cmp::Reverse(*depth));
    for (p, t) in dirs {
        set(p, t)?;
    }
    Ok(paths.len() as u64)
}

impl Run<'_> {
    fn folder(&self, tree: &crate::tree::Tree, dir: &Path, rel: &[u8]) -> Result<()> {
        tree.entries.par_iter().try_for_each(|e| -> Result<()> {
            let path = dir.join(fsx::os(&e.name));
            let child = if rel.is_empty() { e.name.clone() } else { [rel, b"/", &e.name].concat() };
            match &e.node {
                Node::Dir { tree } => {
                    fs::create_dir(&path).with_context(|| path.display().to_string())?;
                    let sub = self.store.get_tree(tree)?;
                    self.folder(&sub, &path, &child)?;
                    fsx::set_xattrs(&path, &e.xattrs)?;
                    self.modes.lock().expect("no panic holds the lock").push((path, e.mode));
                    self.dirs.fetch_add(1, Ordering::Relaxed);
                }
                Node::File { blob, size } => {
                    self.file(blob, *size, &path).with_context(|| format!("cannot write {}", path.display()))?;
                    if !e.xattrs.is_empty() {
                        // macOS sets no attribute on a file without the owner write bit.
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                        fsx::set_xattrs(&path, &e.xattrs)?;
                    }
                    fs::set_permissions(&path, fs::Permissions::from_mode(e.mode))?;
                    self.files_made.lock().expect("no panic holds the lock").insert(child);
                    self.files.fetch_add(1, Ordering::Relaxed);
                }
                Node::Symlink { target } => {
                    std::os::unix::fs::symlink(fsx::os(target), &path).with_context(|| path.display().to_string())?;
                    fsx::set_xattrs(&path, &e.xattrs)?;
                    self.symlinks.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })
    }

    fn file(&self, blob: &Id, size: u64, path: &Path) -> Result<()> {
        if let Some(src) = self.store.hot_path(blob) {
            let cloned = fsx::clone_or_copy(&src, path)?;
            let counter = if cloned { &self.clones } else { &self.copies };
            counter.fetch_add(1, Ordering::Relaxed);
            // The caller sets the tree's mode next.
            let len = fs::symlink_metadata(path)?.len();
            if len != size {
                bail!("blob {blob} has {len} bytes, the tree says {size}");
            }
            return Ok(());
        }
        let data = self.store.read_blob(blob)?;
        if data.len() as u64 != size {
            bail!("blob {blob} has {} bytes, the tree says {size}", data.len());
        }
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(&data)?;
        self.decoded.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
