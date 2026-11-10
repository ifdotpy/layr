//! Make a folder from a tree (docs/objects.md, "Checkout"). A hot blob becomes a clone of its
//! store file (no data is copied on APFS, btrfs and XFS); other blobs are decoded and written.

use crate::fsx;
use crate::id::Id;
use crate::store::Store;
use crate::times::{Record, Times};
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

/// Runs one file system step that writes into the checkout folder. A root service passes one
/// that takes the caller's file identity for the step (`privs` in layr), so every file belongs to
/// the caller and root never writes into a caller's folder. Reads of the store stay outside it.
pub type Writer<'a> = dyn Fn(&mut dyn FnMut() -> std::io::Result<()>) -> std::io::Result<()> + Sync + 'a;

fn as_is(step: &mut dyn FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    step()
}

struct Run<'a> {
    store: &'a Store,
    writer: &'a Writer<'a>,
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
    run_checkout(store, tree, times, dst, &as_is)
}

/// `checkout_with`, with every write through `writer`. The steps then run on one thread: on macOS
/// a file identity is a process property (`seteuid`), so only one step may run at a time.
pub fn checkout_as(store: &Store, tree: &Id, times: Option<&Id>, dst: &Path, writer: &Writer<'_>) -> Result<CheckoutReport> {
    rayon::ThreadPoolBuilder::new().num_threads(1).build()?.install(|| run_checkout(store, tree, times, dst, writer))
}

/// One write step through `writer`, with its result.
fn step<T>(writer: &Writer<'_>, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut f = Some(f);
    let mut out = None;
    writer(&mut || {
        out = Some((f.take().expect("a writer runs a step once"))()?);
        Ok(())
    })?;
    out.ok_or_else(|| std::io::Error::other("the writer did not run the step"))
}

fn run_checkout(store: &Store, tree: &Id, times: Option<&Id>, dst: &Path, writer: &Writer<'_>) -> Result<CheckoutReport> {
    let empty = step(writer, || match fs::read_dir(dst) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(dst).map(|_| true),
        Err(e) => Err(e),
    })
    .with_context(|| format!("cannot use {}", dst.display()))?;
    if !empty {
        bail!("{} is not empty", dst.display());
    }
    let run = Run {
        store,
        writer,
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
            step(writer, || {
                fs::remove_file(&path)?;
                fs::hard_link(&first, &path)
            })
            .with_context(|| path.display().to_string())?;
            hard_links += 1;
        }
    }
    let mut modes = run.modes.into_inner().expect("no panic holds the lock");
    // Deepest first: a folder that loses its write bit has no folder below it left to change.
    modes.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
    for (path, mode) in modes {
        step(writer, || fs::set_permissions(&path, fs::Permissions::from_mode(mode)))?;
    }
    let restored = match times {
        Some(id) => restore_times(store, &root, &store.get_times(id)?, dst, writer)?,
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

/// Set the recorded times of a folder and everything below it; the folder last, because a new
/// entry changes the time of its folder. The number of times set.
fn restore_times(store: &Store, tree: &crate::tree::Tree, times: &Times, dir: &Path, writer: &Writer<'_>) -> Result<u64> {
    if tree.entries.len() != times.records.len() {
        bail!("the times table does not match the tree ({} records, {} entries)", times.records.len(), tree.entries.len());
    }
    let set = |path: &Path, ns: i64| -> Result<()> {
        step(writer, || crate::times::set(path, ns)).with_context(|| format!("cannot set the time of {}", path.display()))
    };
    let below = tree
        .entries
        .par_iter()
        .zip(times.records.par_iter())
        .map(|(e, r)| -> Result<u64> {
            let path = dir.join(fsx::os(&e.name));
            match (&e.node, r) {
                (Node::Dir { tree }, Record::Folder(id)) => {
                    restore_times(store, &store.get_tree(tree)?, &store.get_times(id)?, &path, writer)
                }
                (Node::Dir { .. }, _) | (_, Record::Folder(_)) => {
                    bail!("the times table does not match the tree at {}", path.display())
                }
                (_, Record::Time(ns)) => {
                    set(&path, *ns)?;
                    Ok(1)
                }
            }
        })
        .try_reduce(|| 0, |a, b| Ok(a + b))?;
    set(dir, times.own)?;
    Ok(below + 1)
}

impl Run<'_> {
    fn folder(&self, tree: &crate::tree::Tree, dir: &Path, rel: &[u8]) -> Result<()> {
        tree.entries.par_iter().try_for_each(|e| -> Result<()> {
            let path = dir.join(fsx::os(&e.name));
            let child = if rel.is_empty() { e.name.clone() } else { [rel, b"/", &e.name].concat() };
            match &e.node {
                Node::Dir { tree } => {
                    self.w(|| fs::create_dir(&path)).with_context(|| path.display().to_string())?;
                    let sub = self.store.get_tree(tree)?;
                    self.folder(&sub, &path, &child)?;
                    self.w(|| fsx::set_xattrs(&path, &e.xattrs).map_err(std::io::Error::other))?;
                    self.modes.lock().expect("no panic holds the lock").push((path, e.mode));
                    self.dirs.fetch_add(1, Ordering::Relaxed);
                }
                Node::File { blob, size } => {
                    self.file(blob, *size, &path).with_context(|| format!("cannot write {}", path.display()))?;
                    self.w(|| {
                        if !e.xattrs.is_empty() {
                            // macOS sets no attribute on a file without the owner write bit.
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                            fsx::set_xattrs(&path, &e.xattrs).map_err(std::io::Error::other)?;
                        }
                        fs::set_permissions(&path, fs::Permissions::from_mode(e.mode))
                    })?;
                    self.files_made.lock().expect("no panic holds the lock").insert(child);
                    self.files.fetch_add(1, Ordering::Relaxed);
                }
                Node::Symlink { target } => {
                    self.w(|| {
                        std::os::unix::fs::symlink(fsx::os(target), &path)?;
                        fsx::set_xattrs(&path, &e.xattrs).map_err(std::io::Error::other)
                    })
                    .with_context(|| path.display().to_string())?;
                    self.symlinks.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })
    }

    fn w<T>(&self, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
        step(self.writer, f)
    }

    fn file(&self, blob: &Id, size: u64, path: &Path) -> Result<()> {
        if let Some(src) = self.store.hot_path(blob) {
            // The store file opens with the store's rights; the new file is made by the writer.
            let source = fs::File::open(&src)?;
            let len = source.metadata()?.len();
            if len != size {
                bail!("blob {blob} has {len} bytes, the tree says {size}");
            }
            let cloned = self.w(|| fsx::clone_or_copy_from(&source, path))?;
            let counter = if cloned { &self.clones } else { &self.copies };
            counter.fetch_add(1, Ordering::Relaxed);
            // The caller sets the tree's mode next.
            return Ok(());
        }
        let data = self.store.read_blob(blob)?;
        if data.len() as u64 != size {
            bail!("blob {blob} has {} bytes, the tree says {size}", data.len());
        }
        self.w(|| fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(&data))?;
        self.decoded.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
