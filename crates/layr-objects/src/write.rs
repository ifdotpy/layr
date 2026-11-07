//! Write a tree from a folder: a parallel walk that reads full metadata, stores each file as a
//! blob and each folder as a tree (docs/objects.md, "Writing a tree").

use crate::fsx;
use crate::id::Id;
use crate::store::{Store, Temperature};
use crate::tree::{Entry, Node, Tree, MODE_BITS};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct WriteOptions {
    pub temperature: Temperature,
    /// The tree of the previous version: a cold write stores a changed file as a delta against
    /// the blob at the same path in this tree.
    pub previous: Option<Id>,
    /// Names at the top of the folder to leave out (`.git` of a working folder).
    pub exclude: Vec<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct WriteReport {
    pub tree: Option<Id>,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub bytes: u64,
    pub hard_link_groups: u64,
}

/// The paths of each multiply linked inode, by (device, inode).
type Inodes = HashMap<(u64, u64), Vec<Vec<u8>>>;

struct Walk<'a> {
    store: &'a Store,
    options: &'a WriteOptions,
    files: AtomicU64,
    dirs: AtomicU64,
    symlinks: AtomicU64,
    bytes: AtomicU64,
    /// (device, inode) of each file with more than one link, and its paths in the tree.
    inodes: Mutex<Inodes>,
}

/// Write the tree of the folder `root` into `store`.
pub fn write_tree(store: &Store, root: &Path, options: &WriteOptions) -> Result<WriteReport> {
    let walk = Walk {
        store,
        options,
        files: AtomicU64::new(0),
        dirs: AtomicU64::new(0),
        symlinks: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        inodes: Mutex::new(HashMap::new()),
    };
    let meta = fs::symlink_metadata(root).with_context(|| root.display().to_string())?;
    if !meta.is_dir() {
        bail!("{} is not a folder", root.display());
    }
    let mut tree = walk.folder(root, b"", options.previous)?;
    let groups: Vec<Vec<Vec<u8>>> =
        walk.inodes.into_inner().expect("no panic holds the lock").into_values().filter(|g| g.len() > 1).collect();
    let report_groups = groups.len() as u64;
    tree.links = groups;
    tree.normalize();
    let id = store.put_tree(&tree)?;
    Ok(WriteReport {
        tree: Some(id),
        files: walk.files.into_inner(),
        dirs: walk.dirs.into_inner(),
        symlinks: walk.symlinks.into_inner(),
        bytes: walk.bytes.into_inner(),
        hard_link_groups: report_groups,
    })
}

fn join(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        [prefix, b"/", name].concat()
    }
}

impl Walk<'_> {
    /// The tree of one folder, without hard link groups (the root gets them).
    fn folder(&self, dir: &Path, rel: &[u8], previous: Option<Id>) -> Result<Tree> {
        let mut names: Vec<(Vec<u8>, fs::Metadata)> = Vec::new();
        for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
            let entry = entry?;
            if rel.is_empty() && self.options.exclude.iter().any(|x| x.as_slice() == entry.file_name().as_bytes()) {
                continue;
            }
            let meta = fs::symlink_metadata(entry.path())?;
            names.push((entry.file_name().as_bytes().to_vec(), meta));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let before: HashMap<Vec<u8>, Node> = match previous {
            Some(id) => self.store.get_tree(&id)?.entries.into_iter().map(|e| (e.name, e.node)).collect(),
            None => HashMap::new(),
        };
        let entries = names
            .into_par_iter()
            .map(|(name, meta)| self.entry(dir, rel, name, meta, &before))
            .collect::<Result<Vec<Entry>>>()?;
        Ok(Tree { entries, links: Vec::new() })
    }

    fn entry(&self, dir: &Path, rel: &[u8], name: Vec<u8>, meta: fs::Metadata, before: &HashMap<Vec<u8>, Node>) -> Result<Entry> {
        let path = dir.join(fsx::os(&name));
        let ft = meta.file_type();
        let xattrs = fsx::xattrs(&path).with_context(|| format!("cannot read the attributes of {}", path.display()))?;
        let mode = meta.mode() & MODE_BITS;
        let child = join(rel, &name);
        let node = if ft.is_dir() {
            let previous = match before.get(&name) {
                Some(Node::Dir { tree }) => Some(*tree),
                _ => None,
            };
            let mut sub = self.folder(&path, &child, previous)?;
            sub.normalize();
            self.dirs.fetch_add(1, Ordering::Relaxed);
            Node::Dir { tree: self.store.put_tree(&sub)? }
        } else if ft.is_file() {
            let previous = match before.get(&name) {
                Some(Node::File { blob, .. }) => Some(*blob),
                _ => None,
            };
            let (blob, size) = self
                .store
                .put_file(&path, self.options.temperature, previous)
                .with_context(|| format!("cannot store {}", path.display()))?;
            if meta.nlink() > 1 {
                self.inodes.lock().expect("no panic holds the lock").entry((meta.dev(), meta.ino())).or_default().push(child);
            }
            self.files.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(size, Ordering::Relaxed);
            Node::File { size, blob }
        } else if ft.is_symlink() {
            let target = fs::read_link(&path)?.as_os_str().as_bytes().to_vec();
            self.symlinks.fetch_add(1, Ordering::Relaxed);
            return Ok(Entry { name, mode: 0, node: Node::Symlink { target }, xattrs });
        } else {
            let kind = if ft.is_fifo() {
                "a FIFO"
            } else if ft.is_socket() {
                "a socket"
            } else {
                "a device"
            };
            bail!("{} is {kind}; a tree holds files, folders and symbolic links", path.display());
        };
        Ok(Entry { name, mode, node, xattrs })
    }
}
