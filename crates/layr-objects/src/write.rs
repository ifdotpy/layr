//! Write a tree from a folder: a parallel walk that reads full metadata, stores each file as a
//! blob and each folder as a tree (docs/objects.md, "Writing a tree").

use crate::fsx;
use crate::id::Id;
use crate::store::{Store, Temperature};
use crate::times::{ns_of, Record, Times};
use crate::tree::{Entry, Node, Tree, MODE_BITS};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A folder or a layered snapshot. Paths resolve to immutable files; names are the merged
/// immediate children of a folder. This lets saves read changed entries without a full checkout.
pub trait Source: Sync {
    fn path(&self, relative: &[u8]) -> Result<PathBuf>;
    fn names(&self, relative: &[u8]) -> Result<Vec<Vec<u8>>>;
}

struct Folder<'a>(&'a Path);
impl Source for Folder<'_> {
    fn path(&self, relative: &[u8]) -> Result<PathBuf> {
        Ok(self.0.join(fsx::os(relative)))
    }
    fn names(&self, relative: &[u8]) -> Result<Vec<Vec<u8>>> {
        fs::read_dir(self.path(relative)?)?.map(|e| Ok(e?.file_name().as_bytes().to_vec())).collect()
    }
}

#[derive(Clone, Debug)]
pub struct WriteOptions {
    pub temperature: Temperature,
    /// The tree of the previous version: a cold write stores a changed file as a delta against
    /// the blob at the same path in this tree.
    pub previous: Option<Id>,
    /// Names at the top of the folder to leave out (`.git` of a working folder).
    pub exclude: Vec<Vec<u8>>,
    /// A cold file without a blob at its path in `previous` tries delta bases by similarity: the
    /// blobs of `previous` with the same name, then with the same extension and a near size (at
    /// most `SIMILAR_WINDOW`), against a whole frame.
    pub similar: bool,
}

/// The most delta bases that one file tries.
pub const SIMILAR_WINDOW: usize = 3;

/// The blobs of the previous tree by file name and by extension, for delta bases of renamed and
/// new files.
#[derive(Default)]
struct Similar {
    by_name: HashMap<Vec<u8>, Vec<(u64, Id)>>,
    /// Sorted by size.
    by_ext: HashMap<Vec<u8>, Vec<(u64, Id)>>,
}

fn extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|b| *b == b'.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => b"",
    }
}

impl Similar {
    fn of(store: &Store, tree: &Id) -> Result<Similar> {
        let mut s = Similar::default();
        let mut stack = vec![*tree];
        let mut seen = HashSet::new();
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            for e in store.get_tree(&t)?.entries {
                match e.node {
                    Node::Dir { tree } => stack.push(tree),
                    Node::File { size, blob } => {
                        s.by_ext.entry(extension(&e.name).to_vec()).or_default().push((size, blob));
                        s.by_name.entry(e.name).or_default().push((size, blob));
                    }
                    Node::Symlink { .. } => {}
                }
            }
        }
        for v in s.by_ext.values_mut().chain(s.by_name.values_mut()) {
            v.sort();
            v.dedup();
        }
        Ok(s)
    }

    fn candidates(&self, name: &[u8], size: u64) -> Vec<Id> {
        let mut out: Vec<Id> = Vec::new();
        let near = |v: &Vec<(u64, Id)>, out: &mut Vec<Id>, limit: usize| {
            let mut by_distance: Vec<&(u64, Id)> =
                v.iter().filter(|(n, _)| *n <= size.saturating_mul(2) && n.saturating_mul(2) >= size).collect();
            by_distance.sort_by_key(|(n, _)| n.abs_diff(size));
            for (_, id) in by_distance {
                if out.len() >= limit {
                    break;
                }
                if !out.contains(id) {
                    out.push(*id);
                }
            }
        };
        if let Some(v) = self.by_name.get(name) {
            near(v, &mut out, 2);
        }
        if let Some(v) = self.by_ext.get(extension(name)) {
            near(v, &mut out, SIMILAR_WINDOW);
        }
        out
    }
}

#[derive(Debug, Default)]
pub struct WriteReport {
    pub tree: Option<Id>,
    /// The times table of the folder (`crate::times`), in tree order.
    pub times: Option<Id>,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub bytes: u64,
    pub hard_link_groups: u64,
}

/// The paths of each multiply linked inode, by (device, inode).
type Inodes = HashMap<(u64, u64), Vec<Vec<u8>>>;

struct Walk<'a> {
    source: &'a dyn Source,
    store: &'a Store,
    options: &'a WriteOptions,
    files: AtomicU64,
    dirs: AtomicU64,
    symlinks: AtomicU64,
    bytes: AtomicU64,
    /// (device, inode) of each file with more than one link, and its paths in the tree.
    inodes: Mutex<Inodes>,
    similar: Option<Similar>,
}

/// Write the tree of the folder `root` into `store`.
pub fn write_tree(store: &Store, root: &Path, options: &WriteOptions) -> Result<WriteReport> {
    write_tree_from(store, &Folder(root), options)
}

pub fn write_tree_from(store: &Store, source: &dyn Source, options: &WriteOptions) -> Result<WriteReport> {
    write(store, source, options, None)
}

/// A tree and its times table that an earlier write made.
#[derive(Clone, Copy, Debug)]
pub struct Base {
    pub tree: Id,
    pub times: Id,
}

/// Write the tree of `root` from `base` and the paths that changed since (`changed`: every path
/// whose entry was added, removed, or changed in content, mode, attributes or time). Only the
/// folders on the way to a changed path are read; every other entry and its times come from
/// `base`. A save of a few files costs milliseconds, not a walk of the folder.
///
/// If a changed path touches hard links, a full walk preserves the complete link groups.
pub fn write_changed(
    store: &Store,
    root: &Path,
    base: &Base,
    changed: &[Vec<u8>],
    options: &WriteOptions,
) -> Result<WriteReport> {
    write_changed_from(store, &Folder(root), base, changed, options)
}

pub fn write_changed_from(
    store: &Store,
    source: &dyn Source,
    base: &Base,
    changed: &[Vec<u8>],
    options: &WriteOptions,
) -> Result<WriteReport> {
    // A changed member can still share an inode with an unchanged path. Rebuild link groups
    // from the source in that case; retaining only the untouched groups would split the link.
    let previous = store.get_tree(&base.tree)?;
    let affects =
        |path: &[u8], changed: &[u8]| path == changed || (path.starts_with(changed) && path.get(changed.len()) == Some(&b'/'));
    let links_changed = previous.links.iter().flatten().any(|path| changed.iter().any(|c| affects(path, c)));
    let new_link = changed.iter().any(|path| {
        source.path(path).ok().and_then(|p| fs::symlink_metadata(p).ok()).is_some_and(|m| m.is_file() && m.nlink() > 1)
    });
    if links_changed || new_link {
        return write_tree_from(store, source, options);
    }
    let mut plan = Plan { changed: HashSet::new(), dirty: HashSet::new() };
    for path in changed {
        plan.changed.insert(path.clone());
        let mut at = path.as_slice();
        while let Some(i) = at.iter().rposition(|b| *b == b'/') {
            at = &at[..i];
            plan.dirty.insert(at.to_vec());
        }
    }
    plan.dirty.insert(Vec::new());
    write(store, source, options, Some((base, &plan)))
}

struct Plan {
    changed: HashSet<Vec<u8>>,
    /// The folders on the way to a changed path (the root included).
    dirty: HashSet<Vec<u8>>,
}

fn write(store: &Store, source: &dyn Source, options: &WriteOptions, base: Option<(&Base, &Plan)>) -> Result<WriteReport> {
    let root = source.path(b"")?;
    let walk = Walk {
        source,
        store,
        options,
        files: AtomicU64::new(0),
        dirs: AtomicU64::new(0),
        symlinks: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        inodes: Mutex::new(HashMap::new()),
        similar: match options.previous {
            Some(tree) if options.similar && options.temperature == Temperature::Cold => Some(Similar::of(store, &tree)?),
            _ => None,
        },
    };
    let meta = fs::symlink_metadata(&root).with_context(|| root.display().to_string())?;
    if !meta.is_dir() {
        bail!("{} is not a folder", root.display());
    }
    let mut kept_groups = Vec::new();
    let (mut tree, below) = match base {
        None => walk.folder(&root, b"", options.previous)?,
        Some((base, plan)) => {
            let base_tree = store.get_tree(&base.tree)?;
            let base_times = store.get_times(&base.times)?;
            let touched =
                |p: &Vec<u8>| plan.changed.iter().any(|c| p == c || (p.starts_with(c) && p.get(c.len()) == Some(&b'/')));
            kept_groups = base_tree.links.iter().filter(|g| !g.iter().any(touched)).cloned().collect();
            walk.folder_changed(&root, b"", &base_tree, &base_times, plan)?
        }
    };
    let inodes = walk.inodes.into_inner().expect("no panic holds the lock");
    if base.is_some() && !inodes.is_empty() {
        return write_tree_from(store, source, options);
    }
    let mut groups: Vec<Vec<Vec<u8>>> = inodes.into_values().filter(|g| g.len() > 1).collect();
    groups.extend(kept_groups);
    let report_groups = groups.len() as u64;
    tree.links = groups;
    tree.normalize();
    let id = store.put_tree(&tree)?;
    let times = store.put_times(&Times { own: ns_of(&meta), records: below })?;
    Ok(WriteReport {
        tree: Some(id),
        times: Some(times),
        files: walk.files.into_inner(),
        dirs: walk.dirs.into_inner(),
        symlinks: walk.symlinks.into_inner(),
        bytes: walk.bytes.into_inner(),
        hard_link_groups: report_groups,
    })
}

/// The entries of a folder with their metadata, sorted by name bytes (the top-level excludes left
/// out).
fn list(source: &dyn Source, rel: &[u8], options: &WriteOptions) -> Result<Vec<(Vec<u8>, fs::Metadata)>> {
    let mut names = Vec::new();
    for name in source.names(rel)? {
        if rel.is_empty() && options.exclude.contains(&name) {
            continue;
        }
        let meta = fs::symlink_metadata(source.path(&join(rel, &name))?)?;
        names.push((name, meta));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(names)
}

fn join(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        [prefix, b"/", name].concat()
    }
}

impl Walk<'_> {
    /// A folder read again from `base`: entries that did not change keep their base entry and
    /// times record; folders on the way to a change are read the same way; the rest is new.
    fn folder_changed(&self, dir: &Path, rel: &[u8], base: &Tree, times: &Times, plan: &Plan) -> Result<(Tree, Vec<Record>)> {
        if base.entries.len() != times.records.len() {
            bail!("the base times do not match the base tree");
        }
        let mut base_at: HashMap<&[u8], (&Entry, &Record)> = HashMap::new();
        for (e, r) in base.entries.iter().zip(&times.records) {
            if matches!(r, Record::Folder(_)) != matches!(e.node, Node::Dir { .. }) {
                bail!("the base times do not match the base tree");
            }
            base_at.insert(&e.name, (e, r));
        }
        let before: HashMap<Vec<u8>, Node> = base.entries.iter().map(|e| (e.name.clone(), e.node.clone())).collect();
        let names = list(self.source, rel, self.options)?;
        let done = names
            .into_par_iter()
            .map(|(name, meta)| -> Result<(Entry, Record)> {
                let child = join(rel, &name);
                match base_at.get(name.as_slice()) {
                    Some(&(e, r)) if !plan.changed.contains(&child) && !plan.dirty.contains(&child) => Ok((e.clone(), *r)),
                    Some(&(e, r))
                        if (!plan.changed.contains(&child) || plan.dirty.contains(&child))
                            && meta.is_dir()
                            && matches!(e.node, Node::Dir { .. }) =>
                    {
                        let (Node::Dir { tree }, Record::Folder(sub_times)) = (&e.node, r) else { unreachable!() };
                        let path = self.source.path(&join(rel, &name))?;
                        let (mut sub, below) = self.folder_changed(
                            &path,
                            &child,
                            &self.store.get_tree(tree)?,
                            &self.store.get_times(sub_times)?,
                            plan,
                        )?;
                        sub.normalize();
                        let xattrs = fsx::xattrs(&path)?;
                        let record = Record::Folder(self.store.put_times(&Times { own: ns_of(&meta), records: below })?);
                        self.dirs.fetch_add(1, Ordering::Relaxed);
                        let entry = Entry {
                            name,
                            mode: meta.mode() & MODE_BITS,
                            node: Node::Dir { tree: self.store.put_tree(&sub)? },
                            xattrs,
                        };
                        Ok((entry, record))
                    }
                    _ => self.entry(dir, rel, name, meta, &before),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let (entries, records) = done.into_iter().unzip();
        Ok((Tree { entries, links: Vec::new() }, records))
    }

    /// The tree of one folder, without hard link groups (the root gets them), and the times
    /// records of its entries, in the tree's order.
    fn folder(&self, dir: &Path, rel: &[u8], previous: Option<Id>) -> Result<(Tree, Vec<Record>)> {
        let names = list(self.source, rel, self.options)?;
        let before: HashMap<Vec<u8>, Node> = match previous {
            Some(id) => self.store.get_tree(&id)?.entries.into_iter().map(|e| (e.name, e.node)).collect(),
            None => HashMap::new(),
        };
        let done = names
            .into_par_iter()
            .map(|(name, meta)| self.entry(dir, rel, name, meta, &before))
            .collect::<Result<Vec<(Entry, Record)>>>()?;
        let (entries, records) = done.into_iter().unzip();
        Ok((Tree { entries, links: Vec::new() }, records))
    }

    fn entry(
        &self,
        _dir: &Path,
        rel: &[u8],
        name: Vec<u8>,
        meta: fs::Metadata,
        before: &HashMap<Vec<u8>, Node>,
    ) -> Result<(Entry, Record)> {
        let path = self.source.path(&join(rel, &name))?;
        let ft = meta.file_type();
        let xattrs = fsx::xattrs(&path).with_context(|| format!("cannot read the attributes of {}", path.display()))?;
        let mode = meta.mode() & MODE_BITS;
        let child = join(rel, &name);
        let mut record = Record::Time(ns_of(&meta));
        let node = if ft.is_dir() {
            let previous = match before.get(&name) {
                Some(Node::Dir { tree }) => Some(*tree),
                _ => None,
            };
            let (mut sub, below) = self.folder(&path, &child, previous)?;
            record = Record::Folder(self.store.put_times(&Times { own: ns_of(&meta), records: below })?);
            sub.normalize();
            self.dirs.fetch_add(1, Ordering::Relaxed);
            Node::Dir { tree: self.store.put_tree(&sub)? }
        } else if ft.is_file() {
            let (bases, judge) = match (before.get(&name), &self.similar) {
                (Some(Node::File { blob, .. }), _) => (vec![*blob], false),
                (_, Some(similar)) => (similar.candidates(&name, meta.len()), true),
                _ => (Vec::new(), false),
            };
            let (blob, size) = self
                .store
                .put_file_with(&path, self.options.temperature, &bases, judge)
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
            return Ok((Entry { name, mode: 0, node: Node::Symlink { target }, xattrs }, record));
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
        Ok((Entry { name, mode, node, xattrs }, record))
    }
}
