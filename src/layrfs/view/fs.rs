//! The FUSE view of one line (docs/layrfs-fuse.md), served by `layr fs view serve`.
//!
//! - Lower: the tree of a saved v2 state. Folders load from tree and times objects at their first
//!   use; file reads open the hot blob (`blobs/`), or a cold blob decoded once into `cold/`.
//! - Upper: the content of changed and new files, one native file per file, `upper/<n>`. A name
//!   change does not move the upper file.
//! - Metadata (the tree of names, modes, times, removals) lives in this process. Each change of it
//!   appends one line to `journal`; a restart replays the journal over the head state.
//! - Save: the server knows every changed node (the `dirty` marks), so it writes the blobs of the
//!   changed files and the trees and times tables of the folders on the way to them. Nothing scans
//!   the tree, and no `btrfs send` runs.
//! - Caches: a name like `target` at the root is a symbolic link to a native folder outside the
//!   mount. Status and save leave it out.

use anyhow::{bail, Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner,
    OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use layr_objects::times::{Record, Times};
use layr_objects::{Entry, Id, Node as TreeNode, StateRef, Store, Temperature, Tree};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const ROOT: u64 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    File,
    Dir,
    Symlink,
}

struct Node {
    /// 0 once the node is removed from the tree.
    parent: u64,
    name: Vec<u8>,
    kind: Kind,
    mode: u32,
    size: u64,
    mtime: i64,
    ctime: i64,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// A file: its blob in the store (from the head, or the last save).
    blob: Option<Id>,
    /// A file: its content in `upper/<n>`, which wins over `blob`.
    upper: Option<u64>,
    target: Vec<u8>,
    /// A folder: its tree and times table, valid while the folder is not dirty.
    tree: Option<(Id, Id)>,
    children: Option<BTreeMap<Vec<u8>, u64>>,
    /// The node or something below it differs from the head.
    dirty: bool,
    /// A link to a native cache folder: not part of the line.
    cache: bool,
    writers: u32,
    /// The inode number, owner group and change time that git's index recorded for this file,
    /// when its size and time match the entry (`GitIndex`): `stat` shows them, so git does not
    /// read the file again.
    git: Option<(u32, u32)>,
    /// Small writes to a file without an upper copy (the no-open mode): offset and bytes, over
    /// the blob (or nothing, for a new file). The journal holds them too. A save stores the
    /// result; a copy-up writes it to the upper file (`content`).
    patches: Vec<(u64, Vec<u8>)>,
    patch_bytes: usize,
}

impl Node {
    fn new(parent: u64, name: Vec<u8>, kind: Kind) -> Node {
        Node {
            parent,
            name,
            kind,
            mode: 0o644,
            size: 0,
            mtime: 0,
            ctime: 0,
            xattrs: Vec::new(),
            blob: None,
            upper: None,
            target: Vec::new(),
            tree: None,
            children: None,
            dirty: false,
            cache: false,
            writers: 0,
            git: None,
            patches: Vec::new(),
            patch_bytes: 0,
        }
    }
}

/// One entry of git's index: the stat data git compares.
#[derive(Clone, Copy)]
struct GitEntry {
    ctime: i64,
    mtime_s: u32,
    ino: u32,
    gid: u32,
    size: u32,
}

/// Parse the entries of a git index (versions 2 to 4). None for anything it does not know.
fn parse_git_index(b: &[u8], hash: usize) -> Option<HashMap<Vec<u8>, GitEntry>> {
    let be = |at: usize| -> Option<u32> { b.get(at..at + 4).map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]])) };
    if b.get(..4)? != b"DIRC" {
        return None;
    }
    let version = be(4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = be(8)? as usize;
    let mut out = HashMap::with_capacity(count);
    let mut at = 12;
    let mut last: Vec<u8> = Vec::new();
    for _ in 0..count {
        let start = at;
        let e = GitEntry {
            ctime: be(at)? as i64 * 1_000_000_000 + be(at + 4)? as i64,
            mtime_s: be(at + 8)?,
            ino: be(at + 20)?,
            gid: be(at + 32)?,
            size: be(at + 36)?,
        };
        at += 40 + hash;
        let flags = u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]);
        at += 2;
        if version >= 3 && flags & 0x4000 != 0 {
            at += 2;
        }
        let stage = (flags >> 12) & 3;
        let path = if version == 4 {
            // A varint: the bytes to strip from the previous path.
            let mut strip: usize = 0;
            let mut first = true;
            loop {
                let c = *b.get(at)?;
                at += 1;
                if !first {
                    strip += 1;
                }
                first = false;
                strip = (strip << 7) | (c & 0x7f) as usize;
                if c & 0x80 == 0 {
                    break;
                }
            }
            let end = at + b.get(at..)?.iter().position(|&c| c == 0)?;
            let keep = last.len().checked_sub(strip)?;
            let mut p = last[..keep].to_vec();
            p.extend_from_slice(&b[at..end]);
            at = end + 1;
            p
        } else {
            let end = at + b.get(at..)?.iter().position(|&c| c == 0)?;
            let p = b[at..end].to_vec();
            // Entries are padded with 1 to 8 NUL bytes to a multiple of 8.
            at = start + (end - start + 8) / 8 * 8;
            p
        };
        last = path.clone();
        if stage == 0 {
            out.insert(path, e);
        }
    }
    Some(out)
}

pub fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

fn time_of(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

fn ns_of_time(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

fn eio<E: std::fmt::Display>(e: E) -> Errno {
    eprintln!("layrfs-fuse: {e}");
    Errno::EIO
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 {
        bail!("odd hex length");
    }
    (0..text.len()).step_by(2).map(|i| Ok(u8::from_str_radix(&text[i..i + 2], 16)?)).collect()
}

fn join(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        [prefix, b"/", name].concat()
    }
}

pub struct Inner {
    nodes: Vec<Node>,
    store: Store,
    line: PathBuf,
    /// The number of the next upper file; the pool thread reserves numbers here too.
    next_upper: Arc<AtomicU64>,
    /// Empty upper files made ahead of use, so a copy-up or a creation makes no file.
    pub pool: Arc<Pool>,
    journal: Option<File>,
    pub head: String,
    head_ref: StateRef,
    caches: Vec<(Vec<u8>, PathBuf)>,
    pub stats: Stats,
    /// The change sequence for git's fsmonitor hook: `instance` changes with every start, so a
    /// token of an earlier server means "everything may have changed".
    pub instance: String,
    seq: u64,
    /// The last change of each node, and the paths that left the tree (removed, renamed away).
    node_seq: HashMap<u64, u64>,
    gone: Vec<(u64, Vec<u8>, bool)>,
    /// git's index of the head (`.git/index`), while folders still load.
    git_index: Option<HashMap<Vec<u8>, GitEntry>>,
    /// Whether any node has extended attributes, once every folder is loaded (`index_parts`).
    pub has_xattrs: Option<bool>,
    /// The no-open mode: small writes become patches, new files get no upper file at first.
    pub lazy: bool,
    /// Lab: a store that holds the blobs this store lacks; a read copies a blob from it at first use.
    pub remote: Option<Store>,
    pub fetched_files: AtomicU64,
    pub fetched_bytes: AtomicU64,
    pub fetch_ns: AtomicU64,
    /// The last snapshot: its objects, the change sequence then, and the tree of each folder it
    /// built. `adopt` of a state with those objects makes them the clean base of the folders.
    /// The last snapshot: its objects, the view's `seq` then, the trees of the folders it built,
    /// and the blobs of the files whose content it stored.
    snap_mark: Option<(StateRef, u64, HashMap<u64, (Id, Id)>, HashMap<u64, (Id, u64)>)>,
}

/// Small files are copied at a copy-up, large ones cloned.
const SMALL: u64 = 256 << 10;
/// Patch limits of a file: one write, and all of its patches together.
const PATCH_ONE: usize = 16 << 10;
const PATCH_MAX: usize = 64 << 10;

/// Empty upper files, made by a thread ahead of use. The thread starts at the first use, and
/// each batch is twice the last (to 1,024), so a line that changes few files makes few.
pub struct Pool {
    files: Mutex<Vec<(u64, File)>>,
    want: std::sync::Condvar,
    /// A reset removes every upper file: files made before it are gone.
    epoch: AtomicU64,
    started: std::sync::Once,
    /// The view is gone: the thread that fills the pool ends.
    stopped: std::sync::atomic::AtomicBool,
    next: Arc<AtomicU64>,
    dir: PathBuf,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.pool.stop();
    }
}

impl Pool {
    const MAX_BATCH: usize = 1024;

    fn new(next: Arc<AtomicU64>, dir: PathBuf) -> Pool {
        Pool {
            files: Mutex::new(Vec::new()),
            want: std::sync::Condvar::new(),
            epoch: AtomicU64::new(0),
            started: std::sync::Once::new(),
            stopped: std::sync::atomic::AtomicBool::new(false),
            next,
            dir,
        }
    }

    fn take(self: &Arc<Self>) -> Option<(u64, File)> {
        self.started.call_once(|| {
            let p = self.clone();
            std::thread::spawn(move || p.fill());
        });
        let mut f = self.files.lock().unwrap();
        let x = f.pop();
        self.want.notify_one();
        x
    }

    /// Run `remove` (it removes upper files by name), then drop every pool file. The filling
    /// thread waits meanwhile, and a batch that it made before the end is not used: no pool file
    /// can lose its name.
    pub fn clear_with<R>(&self, remove: impl FnOnce() -> R) -> R {
        let mut f = self.files.lock().unwrap();
        let r = remove();
        f.clear();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        drop(f);
        self.want.notify_one();
        r
    }

    /// End the filling thread (the view is gone).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _f = self.files.lock().unwrap();
        self.want.notify_all();
    }

    fn fill(&self) {
        let mut batch = 64;
        loop {
            {
                let mut f = self.files.lock().unwrap();
                while f.len() >= batch / 2 && !self.stopped.load(Ordering::SeqCst) {
                    f = self.want.wait(f).unwrap();
                }
                if self.stopped.load(Ordering::SeqCst) {
                    f.clear();
                    return;
                }
                if f.is_empty() {
                    batch = (batch * 2).min(Self::MAX_BATCH);
                }
            }
            let epoch = self.epoch.load(Ordering::SeqCst);
            let mut made = Vec::with_capacity(batch);
            for _ in 0..batch {
                let u = self.next.fetch_add(1, Ordering::Relaxed);
                match OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(self.dir.join(u.to_string())) {
                    Ok(file) => made.push((u, file)),
                    Err(_) => return, // the line folder is gone
                }
            }
            let mut f = self.files.lock().unwrap();
            if self.epoch.load(Ordering::SeqCst) == epoch {
                f.extend(made);
            }
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub copy_ups: u64,
    pub journal_lines: u64,
    pub loaded_dirs: u64,
}

impl Inner {
    /// Open the view of the line folder `line` (made if absent) over the state `head` of `store`.
    pub fn open(store: Store, line: &Path, head: &str, caches: Vec<(Vec<u8>, PathBuf)>) -> Result<Inner> {
        fs::create_dir_all(line.join("upper"))?;
        fs::create_dir_all(line.join("cold"))?;
        let journal_path = line.join("journal");
        // The journal names the head that its changes rest on.
        let mut base = head.to_string();
        let mut ops = Vec::new();
        // Only a missing journal is empty: after another read error the cleanup below would
        // remove the upper files that it names.
        let journal = match fs::read(&journal_path) {
            Ok(data) => Some(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", journal_path.display()))),
        };
        if let Some(mut data) = journal {
            // A crash in the middle of a write leaves a last line without its end: it goes, and
            // the file keeps the complete lines (the next line appends after them).
            let complete = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            if complete < data.len() {
                OpenOptions::new().write(true).open(&journal_path)?.set_len(complete as u64)?;
                data.truncate(complete);
            }
            for l in data.as_slice().lines() {
                let l = l?;
                if let Some(state) = l.strip_prefix("B ") {
                    base = state.to_string();
                } else if !l.is_empty() {
                    ops.push(l);
                }
            }
        }
        let head_ref = store.state_ref(&base)?.with_context(|| format!("no state {base} in the object store"))?;
        let root_times = store.get_times(&head_ref.times)?;
        let mut root = Node::new(0, Vec::new(), Kind::Dir);
        root.mode = 0o755;
        root.tree = Some((head_ref.tree, head_ref.times));
        root.mtime = root_times.own;
        root.ctime = root.mtime;
        let mut nodes = vec![Node::new(0, Vec::new(), Kind::Dir)];
        nodes.push(root);
        // A new upper file never takes the number of an earlier one, even of a removed one: a replay
        // of the journal must not see another file under that number.
        let mut next_upper = 1;
        for e in fs::read_dir(line.join("upper"))? {
            if let Ok(n) = e?.file_name().to_string_lossy().parse::<u64>() {
                next_upper = next_upper.max(n + 1);
            }
        }
        for op in &ops {
            let f: Vec<&str> = op.split(' ').collect();
            if matches!(f[0], "C" | "U") {
                if let Some(n) = f.get(2).and_then(|n| n.parse::<u64>().ok()) {
                    next_upper = next_upper.max(n + 1);
                }
            }
        }
        let next_upper = Arc::new(AtomicU64::new(next_upper));
        let mut inner = Inner {
            nodes,
            store,
            line: line.to_path_buf(),
            next_upper: next_upper.clone(),
            pool: Arc::new(Pool::new(next_upper, line.join("upper"))),
            journal: None,
            head: base.clone(),
            head_ref,
            caches,
            snap_mark: None,
            stats: Stats::default(),
            instance: format!("{:016x}", rand::random::<u64>()),
            seq: 0,
            node_seq: HashMap::new(),
            gone: Vec::new(),
            git_index: None,
            has_xattrs: None,
            lazy: false,
            remote: None,
            fetched_files: AtomicU64::new(0),
            fetched_bytes: AtomicU64::new(0),
            fetch_ns: AtomicU64::new(0),
        };
        let replayed = ops.len();
        for op in &ops {
            inner.replay(op).with_context(|| format!("journal line {op}"))?;
        }
        if replayed > 0 {
            eprintln!("layrfs-fuse: replayed {replayed} journal lines over {base}");
        }
        // The journal names the upper files of `C` and `U` lines. A replay that made one more (an
        // old patch past the end of a small file) names it now, or the next replay makes another
        // and drops this one with the writes that it gets.
        let named: std::collections::HashSet<u64> = ops
            .iter()
            .filter_map(|op| {
                let f: Vec<&str> = op.split(' ').collect();
                matches!(f[0], "C" | "U").then(|| f.get(2).and_then(|n| n.parse().ok())).flatten()
            })
            .collect();
        let mut unnamed = String::new();
        for i in 0..inner.nodes.len() {
            let Some(u) = inner.nodes[i].upper else { continue };
            // Writes after the journal's lines went to the upper file: its size and time count.
            if let Ok(meta) = fs::metadata(inner.upper_path(u)) {
                let n = &mut inner.nodes[i];
                n.size = meta.len();
                n.mtime = layr_objects::times::ns_of(&meta);
            }
            if !named.contains(&u) && inner.attached(i as u64) {
                unnamed.push_str(&format!("U {} {u}\n", hex(&inner.path_of(i as u64))));
            }
        }
        // Upper files that no node in the tree uses (pool files of an earlier run, files of
        // removed nodes) go.
        let used: std::collections::HashSet<u64> =
            (0..inner.nodes.len() as u64).filter(|i| inner.attached(*i)).filter_map(|i| inner.nodes[i as usize].upper).collect();
        for e in fs::read_dir(line.join("upper"))? {
            let e = e?;
            if e.file_name().to_string_lossy().parse::<u64>().is_ok_and(|n| !used.contains(&n)) {
                let _ = fs::remove_file(e.path());
            }
        }
        if !journal_path.exists() {
            fs::write(&journal_path, format!("B {base}\n"))?;
        }
        let mut j = OpenOptions::new().append(true).open(&journal_path)?;
        if !unnamed.is_empty() {
            j.write_all(unnamed.as_bytes())?;
            j.sync_data()?;
        }
        inner.journal = Some(j);
        Ok(inner)
    }

    fn add(&mut self, n: Node) -> u64 {
        self.nodes.push(n);
        (self.nodes.len() - 1) as u64
    }

    fn n(&self, ino: u64) -> Result<&Node, Errno> {
        self.nodes.get(ino as usize).filter(|_| ino != 0).ok_or(Errno::ENOENT)
    }

    fn nm(&mut self, ino: u64) -> Result<&mut Node, Errno> {
        if ino == 0 {
            return Err(Errno::ENOENT);
        }
        self.nodes.get_mut(ino as usize).ok_or(Errno::ENOENT)
    }

    fn upper_path(&self, n: u64) -> PathBuf {
        self.line.join("upper").join(n.to_string())
    }

    /// Load the entries of a folder from its tree.
    /// The objects of a folder: its tree, its times table, and the own time of each subfolder.
    fn fetch(store: &Store, tree_id: &Id, times_id: &Id) -> Result<(Tree, Times, Vec<i64>), Errno> {
        let tree = store.get_tree(tree_id).map_err(eio)?;
        let times = store.get_times(times_id).map_err(eio)?;
        if tree.entries.len() != times.records.len() {
            return Err(eio("the times table does not match the tree"));
        }
        let mut owns = Vec::new();
        for r in &times.records {
            if let Record::Folder(tid) = r {
                owns.push(store.get_times(tid).map_err(eio)?.own);
            }
        }
        Ok((tree, times, owns))
    }

    /// Load the entries of a folder from its tree.
    fn load(&mut self, ino: u64) -> Result<(), Errno> {
        let n = self.n(ino)?;
        if n.kind != Kind::Dir {
            return Err(Errno::ENOTDIR);
        }
        if n.children.is_some() {
            return Ok(());
        }
        let fetched = match n.tree {
            Some((tree_id, times_id)) => Some(Self::fetch(&self.store, &tree_id, &times_id)?),
            None => None,
        };
        self.apply_load(ino, fetched)
    }

    /// Make the nodes of a folder from its fetched objects.
    fn apply_load(&mut self, ino: u64, fetched: Option<(Tree, Times, Vec<i64>)>) -> Result<(), Errno> {
        let mut map = BTreeMap::new();
        let dir_path = if self.git_index.is_some() { self.path_of(ino) } else { Vec::new() };
        if let Some((tree, times, owns)) = fetched {
            let mut owns = owns.into_iter();
            for (e, r) in tree.entries.into_iter().zip(times.records) {
                let mut c = Node::new(ino, e.name.clone(), Kind::File);
                c.mode = e.mode;
                c.xattrs = e.xattrs;
                match (e.node, r) {
                    (TreeNode::File { size, blob }, Record::Time(t)) => {
                        c.size = size;
                        c.blob = Some(blob);
                        c.mtime = t;
                    }
                    (TreeNode::Symlink { target }, Record::Time(t)) => {
                        c.kind = Kind::Symlink;
                        c.size = target.len() as u64;
                        c.target = target;
                        c.mtime = t;
                    }
                    (TreeNode::Dir { tree }, Record::Folder(tid)) => {
                        c.kind = Kind::Dir;
                        c.tree = Some((tree, tid));
                        c.mtime = owns.next().ok_or_else(|| eio("missing folder time"))?;
                    }
                    _ => return Err(eio("the times table does not match the tree")),
                }
                c.ctime = c.mtime;
                if let (Kind::File, Some(gi)) = (c.kind, &self.git_index) {
                    if let Some(g) = gi.get(&join(&dir_path, &e.name)) {
                        if g.size as u64 == (c.size & 0xffff_ffff) && g.mtime_s as i64 == c.mtime.div_euclid(1_000_000_000) {
                            c.git = Some((g.ino, g.gid));
                            c.ctime = g.ctime;
                        }
                    }
                }
                let id = self.add(c);
                map.insert(e.name, id);
            }
        }
        if ino == ROOT {
            for (name, path) in self.caches.clone() {
                let mut c = Node::new(ROOT, name.clone(), Kind::Symlink);
                c.target = path.as_os_str().as_bytes().to_vec();
                c.size = c.target.len() as u64;
                c.cache = true;
                c.mtime = now_ns();
                c.ctime = c.mtime;
                let id = self.add(c);
                map.insert(name, id);
            }
        }
        self.stats.loaded_dirs += 1;
        self.nm(ino)?.children = Some(map);
        Ok(())
    }

    /// Load every folder, a level at a time, reading the objects of a level in parallel.
    pub fn load_all(&mut self) -> Result<(), Errno> {
        let mut level = vec![ROOT];
        while !level.is_empty() {
            let todo: Vec<(u64, Option<(Id, Id)>)> = level
                .iter()
                .filter(|&&d| self.nodes[d as usize].children.is_none())
                .map(|&d| (d, self.nodes[d as usize].tree))
                .collect();
            let store = &self.store;
            let fetched: Vec<(u64, Result<Option<(Tree, Times, Vec<i64>)>, Errno>)> =
                todo.par_iter().map(|(d, ids)| (*d, ids.map(|(t, m)| Self::fetch(store, &t, &m)).transpose())).collect();
            for (d, f) in fetched {
                self.apply_load(d, f?)?;
            }
            let mut next = Vec::new();
            for d in level {
                for &c in self.nodes[d as usize].children.as_ref().expect("loaded").values() {
                    if self.nodes[c as usize].kind == Kind::Dir {
                        next.push(c);
                    }
                }
            }
            level = next;
        }
        Ok(())
    }

    /// Every entry below the root (index keys, NUL separated) and the time stamps of every
    /// folder and ignore file, as a walk of the mount gives them: for the index of `layr tool`.
    /// Loads every folder.
    pub fn index_parts(&mut self) -> Result<(Vec<(Vec<u8>, crate::layrfs::index::Kind)>, Vec<(Vec<u8>, (i64, i64))>), Errno> {
        use crate::layrfs::index::Kind as K;
        let stamp = |ns: i64| (ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000));
        let mut all = Vec::new();
        let mut stamps = vec![(Vec::new(), stamp(self.nodes[ROOT as usize].mtime))];
        self.load_all()?;
        let mut stack: Vec<(u64, Vec<u8>)> = vec![(ROOT, Vec::new())];
        let mut any_xattrs = false;
        while let Some((dir, key)) = stack.pop() {
            self.load(dir)?;
            let kids: Vec<(Vec<u8>, u64)> =
                self.nodes[dir as usize].children.as_ref().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
            for (name, c) in kids {
                let mut k = key.clone();
                if !k.is_empty() {
                    k.push(0);
                }
                k.extend_from_slice(&name);
                let n = &self.nodes[c as usize];
                let kind = match n.kind {
                    Kind::File => K::File,
                    Kind::Dir => K::Dir,
                    Kind::Symlink => K::Link,
                };
                if n.kind == Kind::Dir
                    || (n.kind == Kind::File && matches!(name.as_slice(), b".gitignore" | b".ignore" | b".rgignore"))
                {
                    stamps.push((k.clone(), stamp(n.mtime)));
                }
                any_xattrs |= !n.xattrs.is_empty();
                all.push((k.clone(), kind));
                if n.kind == Kind::Dir {
                    stack.push((c, k));
                }
            }
        }
        self.has_xattrs = Some(any_xattrs);
        Ok((all, stamps))
    }

    /// Read git's index of the head, so that loaded files show the stat data it recorded.
    /// Folders loaded on the way to it get the data afterwards.
    pub fn read_git_index(&mut self) {
        let Ok(ino) = self.resolve(b".git/index") else { return };
        let Ok(path) = self.read_path(ino) else { return };
        let Ok(bytes) = fs::read(&path) else { return };
        let sha256 = self
            .resolve(b".git/config")
            .ok()
            .and_then(|c| self.read_path(c).ok())
            .and_then(|p| fs::read_to_string(p).ok())
            .is_some_and(|c| c.to_ascii_lowercase().contains("objectformat = sha256"));
        let Some(map) = parse_git_index(&bytes, if sha256 { 32 } else { 20 }) else { return };
        for i in (ROOT + 1)..self.nodes.len() as u64 {
            if self.nodes[i as usize].kind != Kind::File {
                continue;
            }
            let p = self.path_of(i);
            let n = &mut self.nodes[i as usize];
            if let Some(g) = map.get(&p) {
                if g.size as u64 == (n.size & 0xffff_ffff) && g.mtime_s as i64 == n.mtime.div_euclid(1_000_000_000) {
                    n.git = Some((g.ino, g.gid));
                    n.ctime = g.ctime;
                }
            }
        }
        self.git_index = Some(map);
    }

    pub fn child(&mut self, parent: u64, name: &[u8]) -> Result<Option<u64>, Errno> {
        self.load(parent)?;
        Ok(self.n(parent)?.children.as_ref().and_then(|c| c.get(name).copied()))
    }

    fn resolve(&mut self, path: &[u8]) -> Result<u64, Errno> {
        let mut ino = ROOT;
        if path.is_empty() {
            return Ok(ino);
        }
        for part in path.split(|b| *b == b'/') {
            ino = self.child(ino, part)?.ok_or(Errno::ENOENT)?;
        }
        Ok(ino)
    }

    fn split(path: &[u8]) -> (&[u8], &[u8]) {
        match path.iter().rposition(|b| *b == b'/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => (b"", path),
        }
    }

    pub fn path_of(&self, mut ino: u64) -> Vec<u8> {
        let mut parts = Vec::new();
        while ino != ROOT && ino != 0 {
            let n = &self.nodes[ino as usize];
            parts.push(n.name.clone());
            ino = n.parent;
        }
        parts.reverse();
        parts.join(&b'/')
    }

    /// Mark a node and the folders above it as changed.
    /// The folders above a dirty node are dirty, so the walk stops at the first dirty folder.
    fn touch(&mut self, mut ino: u64) {
        self.mark(ino);
        let mut first = true;
        while ino != 0 {
            let n = &mut self.nodes[ino as usize];
            if n.dirty && !first {
                return;
            }
            n.dirty = true;
            first = false;
            if ino == ROOT {
                return;
            }
            ino = n.parent;
        }
    }

    /// Record a change of a node for the fsmonitor hook.
    pub fn mark(&mut self, ino: u64) {
        self.seq += 1;
        self.node_seq.insert(ino, self.seq);
    }

    /// Record a path that leaves the tree (before the node moves or goes).
    fn mark_gone(&mut self, ino: u64) {
        if self.attached(ino) {
            self.seq += 1;
            let dir = self.nodes[ino as usize].kind == Kind::Dir;
            self.gone.push((self.seq, self.path_of(ino), dir));
        }
    }

    /// The answer of fsmonitor protocol version 2 to `token`: the new token, then every path
    /// that changed since it, each followed by a NUL (folders end with `/`). A token of another
    /// server instance, or none, gives `/`: everything. Files open for writing are always listed,
    /// because the kernel's writeback cache holds their newest bytes.
    pub fn fsmonitor(&self, token: &str) -> (Vec<u8>, bool) {
        let mut out = format!("layrfs:{}:{}", self.instance, self.seq).into_bytes();
        out.push(0);
        let since = token
            .strip_prefix("layrfs:")
            .and_then(|t| t.split_once(':'))
            .filter(|(inst, _)| *inst == self.instance)
            .and_then(|(_, n)| n.parse::<u64>().ok());
        let Some(since) = since else {
            out.extend_from_slice(b"/\0");
            return (out, false);
        };
        let mut paths: Vec<Vec<u8>> = Vec::new();
        for (&ino, &s) in &self.node_seq {
            if s > since && self.attached(ino) && ino != ROOT {
                let mut p = self.path_of(ino);
                if self.nodes[ino as usize].kind == Kind::Dir {
                    p.push(b'/');
                }
                paths.push(p);
            }
        }
        for (s, p, dir) in &self.gone {
            if *s > since {
                let mut p = p.clone();
                if *dir {
                    p.push(b'/');
                }
                paths.push(p);
            }
        }
        for (ino, n) in self.nodes.iter().enumerate() {
            if n.writers > 0 && self.attached(ino as u64) {
                paths.push(self.path_of(ino as u64));
            }
        }
        paths.sort();
        paths.dedup();
        for p in paths {
            out.extend_from_slice(&p);
            out.push(0);
        }
        (out, true)
    }

    /// The child `name` of a folder whose entries are loaded; None when they are not loaded yet.
    pub fn loaded_child(&self, parent: u64, name: &[u8]) -> Option<Result<Option<u64>, Errno>> {
        match self.n(parent) {
            Err(e) => Some(Err(e)),
            Ok(n) if n.kind != Kind::Dir => Some(Err(Errno::ENOTDIR)),
            Ok(n) => n.children.as_ref().map(|c| Ok(c.get(name).copied())),
        }
    }

    /// The file that holds the bytes at an index key (components separated by NUL bytes): the
    /// upper file or the blob. The index of `layr tool rg` reads it, never the mount. Only
    /// loaded folders are looked at (a shared lock): None makes the index read the mount.
    pub fn key_content(&self, key: &[u8]) -> Option<PathBuf> {
        let mut ino = ROOT;
        for part in key.split(|&b| b == 0) {
            ino = self.loaded_child(ino, part)?.ok()??;
        }
        let n = self.n(ino).ok()?;
        match (n.kind, n.upper, n.blob) {
            (Kind::File, Some(u), _) => Some(self.upper_path(u)),
            (Kind::File, None, Some(b)) => self.blob_file(&b).ok(),
            _ => None,
        }
    }

    /// Drop every unsaved change: the view shows the head again. The kernel caches names and
    /// attributes for an hour, so the caller invalidates what this returns: every node the
    /// kernel may know (`inval_inode`) and every name of a loaded folder (`inval_entry`).
    /// Lab: every node and every name that the kernel may cache.
    pub fn known(&self) -> (Vec<u64>, Vec<(u64, Vec<u8>)>) {
        let mut entries = Vec::new();
        for (ino, n) in self.nodes.iter().enumerate() {
            if let (Some(c), true) = (&n.children, ino as u64 == ROOT || n.parent != 0) {
                for name in c.keys() {
                    entries.push((ino as u64, name.clone()));
                }
            }
        }
        let inodes: Vec<u64> = (ROOT..self.nodes.len() as u64).filter(|i| self.attached(*i)).collect();
        (inodes, entries)
    }

    pub fn reset(&mut self) -> Result<(Vec<u64>, Vec<(u64, Vec<u8>)>)> {
        let mut entries = Vec::new();
        for (ino, n) in self.nodes.iter().enumerate() {
            if let (Some(c), true) = (&n.children, ino as u64 == ROOT || n.parent != 0) {
                for name in c.keys() {
                    entries.push((ino as u64, name.clone()));
                }
            }
        }
        let inodes: Vec<u64> = (ROOT..self.nodes.len() as u64).filter(|i| self.attached(*i)).collect();
        // A name of the head that the kernel holds as missing (a negative entry) must go too. The
        // names below the root go with their folder's entry.
        for e in self.store.get_tree(&self.head_ref.tree)?.entries {
            entries.push((ROOT, e.name));
        }
        // The journal first: a reset that stops before its end leaves the changes readable.
        self.replace_journal(&format!("B {}\n", self.head))?;
        let _ = fs::remove_file(self.line.join("rebase-base"));
        // Every path may have changed: a new instance makes git's next fsmonitor query a full scan.
        self.instance = format!("{:016x}", rand::random::<u64>());
        for n in self.nodes.iter_mut().skip(ROOT as usize + 1) {
            n.parent = 0;
        }
        let open: std::collections::HashSet<u64> = self.nodes.iter().filter(|n| n.writers > 0).filter_map(|n| n.upper).collect();
        let upper = self.line.join("upper");
        self.pool.clear_with(|| -> Result<()> {
            for e in fs::read_dir(&upper)? {
                let e = e?;
                let keep = e.file_name().to_string_lossy().parse::<u64>().is_ok_and(|n| open.contains(&n));
                if !keep {
                    let _ = fs::remove_file(e.path());
                }
            }
            Ok(())
        })?;
        let times = self.store.get_times(&self.head_ref.times)?;
        let root = &mut self.nodes[ROOT as usize];
        root.children = None;
        root.tree = Some((self.head_ref.tree, self.head_ref.times));
        root.mtime = times.own;
        root.ctime = now_ns();
        root.dirty = false;
        self.mark(ROOT);
        Ok((inodes, entries))
    }

    fn dir_changed(&mut self, dir: u64) {
        let t = now_ns();
        let n = &mut self.nodes[dir as usize];
        n.mtime = t;
        n.ctime = t;
        self.touch(dir);
    }

    /// False for a node that is no longer in the tree (removed, or replaced by a rename). The
    /// kernel still sends calls for it while it is open; they change only memory.
    fn attached(&self, ino: u64) -> bool {
        ino == ROOT || self.nodes.get(ino as usize).is_some_and(|n| n.parent != 0)
    }

    /// Make the journal and the folder of upper files durable: the journal names each upper
    /// file and holds the patches, renames and removals (`fsync`, `fsyncdir`).
    pub fn sync(&self) -> Result<(), Errno> {
        if let Some(j) = &self.journal {
            j.sync_data().map_err(Errno::from)?;
        }
        File::open(self.line.join("upper")).and_then(|d| d.sync_all()).map_err(Errno::from)
    }

    /// The journal of a view that rests on the state `name`: its base and every upper file that
    /// an attached node keeps (`drops`: the nodes whose upper file goes with this change).
    fn base_journal(&self, name: &str, drops: impl Fn(&Node) -> bool) -> String {
        let mut journal = format!("B {name}\n");
        for i in 0..self.nodes.len() {
            let n = &self.nodes[i];
            if let (Some(u), true, false) = (n.upper, self.attached(i as u64), drops(n)) {
                journal.push_str(&format!("U {} {u}\n", hex(&self.path_of(i as u64))));
            }
        }
        journal
    }

    /// Replace the journal with `text`: the objects of the store and the new journal reach the
    /// disk first. From the rename on, the view appends to the new journal; a failed sync of the
    /// line folder after it still returns its error.
    fn replace_journal(&mut self, text: &str) -> Result<()> {
        rustix::fs::syncfs(File::open(self.store.root())?)?;
        let tmp = self.line.join("journal.tmp");
        let j = {
            let mut f = File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            OpenOptions::new().append(true).open(&tmp)?
        };
        fs::rename(&tmp, self.line.join("journal"))?;
        self.journal = Some(j);
        Ok(())
    }

    /// Append one line to the journal. A failed write leaves no part of the line (the next line
    /// would continue it) and gives its own error (ENOSPC, not EIO).
    fn log(&mut self, line: String) -> Result<(), Errno> {
        if let Some(j) = &mut self.journal {
            let end = j.metadata().map_err(Errno::from)?.len();
            if let Err(e) = j.write_all(line.as_bytes()) {
                let _ = j.set_len(end);
                return Err(Errno::from(e));
            }
            self.stats.journal_lines += 1;
        }
        Ok(())
    }

    /// The file that holds the bytes of a lower blob.
    fn blob_file(&self, blob: &Id) -> Result<PathBuf, Errno> {
        if let Some(p) = self.store.hot_path(blob) {
            return Ok(p);
        }
        if let Some(remote) = &self.remote {
            if !self.store.has(blob) {
                return self.fetch_blob(remote, blob);
            }
        }
        let p = self.line.join("cold").join(blob.hex());
        if !p.exists() {
            let bytes = self.store.read_blob(blob).map_err(eio)?;
            // Readers share the lock: each decodes to its own temporary name.
            let tmp = self.line.join("cold").join(format!("{}.{:016x}.tmp", blob.hex(), rand::random::<u64>()));
            fs::write(&tmp, bytes).map_err(eio)?;
            fs::rename(&tmp, &p).map_err(eio)?;
        }
        Ok(p)
    }

    /// Lab: copy a blob of the remote store into the hot blobs of this store.
    fn fetch_blob(&self, remote: &Store, blob: &Id) -> Result<PathBuf, Errno> {
        let t = Instant::now();
        let dst = self.store.hot_candidate(blob);
        let tmp = self.store.root().join("tmp").join(format!("fetch-{}.{:016x}", blob.hex(), rand::random::<u64>()));
        let bytes = match remote.hot_path(blob) {
            Some(src) => fs::copy(&src, &tmp).map_err(eio)?,
            None => {
                let b = remote.read_blob(blob).map_err(eio)?;
                fs::write(&tmp, &b).map_err(eio)?;
                b.len() as u64
            }
        };
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d).map_err(eio)?;
        }
        fs::rename(&tmp, &dst).map_err(eio)?;
        self.fetched_files.fetch_add(1, Ordering::Relaxed);
        self.fetched_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.fetch_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok(dst)
    }

    /// The file nodes of the view whose content is unchanged (`unchanged_content`).
    pub fn unchanged_files(&self) -> Vec<u64> {
        (ROOT..self.nodes.len() as u64)
            .filter(|i| self.attached(*i))
            .filter(|i| self.nodes.get(*i as usize).is_some_and(|n| n.kind == Kind::File && n.upper.is_none() && n.patches.is_empty() && n.blob.is_some()))
            .collect()
    }

    /// The content file of an unchanged file node (its blob: no upper copy, no small writes, not
    /// only on a remote). None for every other node.
    pub fn unchanged_content(&self, ino: u64) -> Option<PathBuf> {
        let n = self.n(ino).ok()?;
        if n.kind != Kind::File || n.upper.is_some() || !n.patches.is_empty() {
            return None;
        }
        let b = n.blob.as_ref()?;
        if self.remote.is_some() && !self.store.has(b) {
            return None;
        }
        self.blob_file(b).ok()
    }

    /// The file to read a node from.
    pub fn read_path(&self, ino: u64) -> Result<PathBuf, Errno> {
        let n = self.n(ino)?;
        if n.kind != Kind::File {
            return Err(Errno::EISDIR);
        }
        match (n.upper, n.blob) {
            (Some(u), _) => Ok(self.upper_path(u)),
            (None, Some(b)) => self.blob_file(&b),
            (None, None) => Err(eio("a file without content")),
        }
    }

    /// A new upper file: one of the pool, else a new one.
    fn new_upper(&self) -> Result<(u64, File), Errno> {
        // Not in the replay of `open` (no journal yet): its cleanup would remove pool files.
        if self.journal.is_some() {
            if let Some(x) = self.pool.take() {
                return Ok(x);
            }
        }
        let u = self.next_upper.fetch_add(1, Ordering::Relaxed);
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.upper_path(u))
            .map_err(Errno::from)?;
        Ok((u, f))
    }

    /// The bytes of a small file: its upper file, its blob or nothing, with its patches.
    pub fn content(&self, ino: u64) -> Result<Vec<u8>, Errno> {
        use std::io::Read as _;
        let n = self.n(ino)?;
        let mut bytes = Vec::with_capacity(n.size as usize);
        if n.upper.is_some() || n.blob.is_some() {
            File::open(self.read_path(ino)?).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(Errno::from)?;
        }
        for (off, data) in &n.patches {
            let end = *off as usize + data.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[*off as usize..end].copy_from_slice(data);
        }
        bytes.truncate(n.size as usize);
        bytes.resize(n.size as usize, 0);
        Ok(bytes)
    }

    /// True when a write of `len` bytes at `off` to `ino` can become a patch. The file stays
    /// small: `content` reads it into memory up to its end.
    pub fn can_patch(&self, ino: u64, off: u64, len: usize) -> bool {
        self.lazy
            && self.n(ino).is_ok_and(|n| {
                n.kind == Kind::File
                    && n.upper.is_none()
                    && !n.cache
                    && n.size <= SMALL
                    && off.checked_add(len as u64).is_some_and(|end| end <= SMALL)
                    && len <= PATCH_ONE
                    && n.patch_bytes + len <= PATCH_MAX
                    && n.patches.len() < 256
            })
    }

    /// A write as a patch (`can_patch`): memory and one journal line, no file.
    pub fn patch(&mut self, ino: u64, off: u64, data: &[u8], mtime: Option<i64>) -> Result<(), Errno> {
        let t = mtime.unwrap_or_else(now_ns);
        self.n(ino)?;
        // The journal line first: a write that fails changes nothing.
        if mtime.is_none() && self.attached(ino) {
            let path = self.path_of(ino);
            self.log(format!("W {} {off} {t} {}\n", hex(&path), hex(data)))?;
        }
        let n = self.nm(ino)?;
        n.patches.push((off, data.to_vec()));
        n.patch_bytes += data.len();
        n.size = n.size.max(off + data.len() as u64);
        n.mtime = t;
        n.ctime = t;
        self.touch(ino);
        Ok(())
    }

    /// Store the content of every patched file as a blob (a save does this first).
    fn store_patched(&mut self) -> Result<usize> {
        let mut todo = Vec::new();
        self.collect_patched(ROOT, &mut todo);
        let contents: Vec<(u64, Vec<u8>)> = todo
            .into_iter()
            .map(|ino| Ok((ino, self.content(ino).map_err(|e| anyhow::anyhow!("errno {e:?}"))?)))
            .collect::<Result<_>>()?;
        let store = &self.store;
        let stored: Vec<(u64, Result<(Id, u64)>)> = contents.par_iter().map(|(ino, b)| (*ino, store.put_hot_bytes(b))).collect();
        let count = stored.len();
        for (ino, r) in stored {
            let (blob, size) = r?;
            let n = &mut self.nodes[ino as usize];
            n.blob = Some(blob);
            n.size = size;
            n.patches.clear();
            n.patch_bytes = 0;
        }
        Ok(count)
    }

    fn collect_patched(&self, ino: u64, out: &mut Vec<u64>) {
        let n = &self.nodes[ino as usize];
        if !n.dirty {
            return;
        }
        match n.kind {
            Kind::File if n.upper.is_none() && (!n.patches.is_empty() || n.blob.is_none()) => out.push(ino),
            Kind::Dir => {
                if let Some(c) = &n.children {
                    for k in c.values() {
                        self.collect_patched(*k, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// Give every patched file in the tree its upper copy (`layr tool` reads backing files).
    pub fn materialize_patched(&mut self) {
        let mut todo = Vec::new();
        self.collect_patched(ROOT, &mut todo);
        for ino in todo {
            let _ = self.copy_up(ino);
        }
    }

    /// Give a file its own upper copy: a byte copy into an empty pool file for small files, a
    /// clone of the blob for large ones. Returns the path and, when it made one, the open file.
    pub fn copy_up_file(&mut self, ino: u64) -> Result<(PathBuf, Option<File>), Errno> {
        self.copy_up_write(ino, None).map(|(p, f, _)| (p, f))
    }

    /// `copy_up_file` with the first write: a small file gets the blob and the write in one
    /// write to the new upper file (each write costs a file system transaction on btrfs). The
    /// third value is true when the write is done.
    pub fn copy_up_write(&mut self, ino: u64, first: Option<(u64, &[u8])>) -> Result<(PathBuf, Option<File>, bool), Errno> {
        let n = self.n(ino)?;
        if n.kind != Kind::File {
            return Err(Errno::EISDIR);
        }
        if n.cache {
            return Err(Errno::EPERM);
        }
        if let Some(u) = n.upper {
            return Ok((self.upper_path(u), None, false));
        }
        let size = n.size;
        let small = size <= SMALL || !n.patches.is_empty() || n.blob.is_none();
        let mut written = false;
        let src = if small { PathBuf::new() } else { self.read_path(ino)? };
        let small_bytes = if small { Some(self.content(ino)?) } else { None };
        let (u, mut file) = self.new_upper()?;
        let dst = self.upper_path(u);
        let copied = if let Some(bytes) = small_bytes {
            // A clone of a small file costs more than the copy, and every later write to a cloned
            // extent copies it on btrfs anyway. The first write goes to the file at its offset:
            // past the end it leaves a hole, not zeros in memory.
            (|| -> std::io::Result<()> {
                file.write_all(&bytes)?;
                if let Some((off, data)) = first {
                    std::os::unix::fs::FileExt::write_all_at(&file, data, off)?;
                    written = true;
                }
                Ok(())
            })()
        } else {
            rustix::fs::ioctl_ficlone(&file, &File::open(&src).map_err(eio)?).or_else(|_| {
                let mut from = File::open(&src)?;
                std::io::copy(&mut from, &mut file).map(|_| ())
            })
        };
        if let Err(e) = copied {
            let _ = fs::remove_file(&dst);
            return Err(eio(e));
        }
        if !written {
            // A replay takes the time of an upper file from the file.
            let _ = layr_objects::times::set(&dst, self.n(ino)?.mtime);
        }
        // The journal line first: a copy-up that fails leaves the node on its old content.
        if self.attached(ino) {
            let path = self.path_of(ino);
            if let Err(e) = self.log(format!("U {} {u}\n", hex(&path))) {
                let _ = fs::remove_file(&dst);
                return Err(e);
            }
        }
        {
            let n = self.nm(ino)?;
            n.upper = Some(u);
            n.patches.clear();
            n.patch_bytes = 0;
        }
        self.stats.copy_ups += 1;
        self.touch(ino);
        Ok((dst, Some(file), written))
    }

    /// Give a file its own upper copy (a clone of the blob where the file system can).
    pub fn copy_up(&mut self, ino: u64) -> Result<PathBuf, Errno> {
        self.copy_up_file(ino).map(|(p, _)| p)
    }

    fn check_new(&mut self, parent: u64, name: &[u8]) -> Result<(), Errno> {
        if !layr_objects::tree::valid_name(name) {
            return Err(Errno::EINVAL);
        }
        if self.child(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        Ok(())
    }

    fn insert(&mut self, parent: u64, name: &[u8], node: Node) -> Result<u64, Errno> {
        let ino = self.add(node);
        self.nm(parent)?.children.as_mut().expect("loaded").insert(name.to_vec(), ino);
        self.dir_changed(parent);
        self.touch(ino);
        Ok(ino)
    }

    /// A new empty file. `existing`: the upper file of a replayed creation.
    pub fn create(&mut self, parent: u64, name: &[u8], mode: u32, existing: Option<u64>) -> Result<u64, Errno> {
        self.check_new(parent, name)?;
        if existing.is_none() && self.lazy {
            return self.create_lazy(parent, name, mode, None);
        }
        let u = match existing {
            Some(u) => u,
            None => self.new_upper()?.0,
        };
        // The journal line first: a create that fails adds nothing.
        if existing.is_none() {
            let path = join(&self.path_of(parent), name);
            if let Err(e) = self.log(format!("C {} {u} {:o}\n", hex(&path), mode & 0o7777)) {
                let _ = fs::remove_file(self.upper_path(u));
                return Err(e);
            }
        }
        let mut n = Node::new(parent, name.to_vec(), Kind::File);
        n.mode = mode & 0o7777;
        n.upper = Some(u);
        n.mtime = now_ns();
        n.ctime = n.mtime;
        if existing.is_some() {
            // A file that a later journal line removes has no upper file any more.
            if let Ok(meta) = fs::metadata(self.upper_path(u)) {
                n.size = meta.len();
                n.mtime = layr_objects::times::ns_of(&meta);
            }
        }
        self.insert(parent, name, n)
    }

    /// A new empty file without an upper file (the no-open mode): its writes become patches.
    /// `mtime`: the time of a replayed creation.
    fn create_lazy(&mut self, parent: u64, name: &[u8], mode: u32, mtime: Option<i64>) -> Result<u64, Errno> {
        self.check_new(parent, name)?;
        let mut n = Node::new(parent, name.to_vec(), Kind::File);
        n.mode = mode & 0o7777;
        n.mtime = mtime.unwrap_or_else(now_ns);
        n.ctime = n.mtime;
        if mtime.is_none() {
            let path = join(&self.path_of(parent), name);
            self.log(format!("N {} {:o} {}\n", hex(&path), mode & 0o7777, n.mtime))?;
        }
        self.insert(parent, name, n)
    }

    pub fn mkdir(&mut self, parent: u64, name: &[u8], mode: u32) -> Result<u64, Errno> {
        self.check_new(parent, name)?;
        let mut n = Node::new(parent, name.to_vec(), Kind::Dir);
        n.mode = mode & 0o7777;
        n.children = Some(BTreeMap::new());
        n.mtime = now_ns();
        n.ctime = n.mtime;
        let path = join(&self.path_of(parent), name);
        self.log(format!("D {} {:o}\n", hex(&path), mode & 0o7777))?;
        self.insert(parent, name, n)
    }

    pub fn symlink(&mut self, parent: u64, name: &[u8], target: &[u8]) -> Result<u64, Errno> {
        self.check_new(parent, name)?;
        if target.is_empty() || target.contains(&0) {
            return Err(Errno::EINVAL);
        }
        let mut n = Node::new(parent, name.to_vec(), Kind::Symlink);
        n.target = target.to_vec();
        n.size = target.len() as u64;
        n.mtime = now_ns();
        n.ctime = n.mtime;
        // The line folder is `vlines/<line id>/fs`.
        n.cache = self.line.parent().and_then(crate::fast_cache::root_of_view).is_some_and(|caches| {
            crate::fast_cache::is_cache_target(Path::new(OsStr::from_bytes(target)), &caches)
        });
        let path = join(&self.path_of(parent), name);
        self.log(format!("L {} {}\n", hex(&path), hex(target)))?;
        self.insert(parent, name, n)
    }

    fn detach(&mut self, parent: u64, name: &[u8]) -> Result<u64, Errno> {
        let ino = self.nm(parent)?.children.as_mut().expect("loaded").remove(name).ok_or(Errno::ENOENT)?;
        // The upper file stays until the kernel forgets the node (`forget`): a program can
        // still read and write it through a descriptor that it opened before.
        self.nm(ino)?.parent = 0;
        self.dir_changed(parent);
        Ok(ino)
    }

    /// The kernel no longer knows the node: a removed node's upper file goes.
    pub fn forget(&mut self, ino: u64) -> bool {
        if ino == ROOT || self.attached(ino) {
            return false;
        }
        let Some(n) = self.nodes.get_mut(ino as usize) else { return false };
        let Some(u) = n.upper.take() else { return false };
        let _ = fs::remove_file(self.upper_path(u));
        true
    }

    pub fn remove(&mut self, parent: u64, name: &[u8], dir: bool) -> Result<(), Errno> {
        let ino = self.child(parent, name)?.ok_or(Errno::ENOENT)?;
        let n = self.n(ino)?;
        // A link to a cache tree may go (`cargo clean`): the cache goes with it at the next save.
        if n.cache && n.kind != Kind::Symlink {
            return Err(Errno::EPERM);
        }
        match (dir, n.kind) {
            (true, Kind::Dir) => {
                self.load(ino)?;
                if !self.n(ino)?.children.as_ref().expect("loaded").is_empty() {
                    return Err(Errno::ENOTEMPTY);
                }
            }
            (true, _) => return Err(Errno::ENOTDIR),
            (false, Kind::Dir) => return Err(Errno::EISDIR),
            (false, _) => {}
        }
        let path = join(&self.path_of(parent), name);
        // The journal line first: a remove that fails leaves the name.
        self.log(format!("R {}\n", hex(&path)))?;
        self.mark_gone(ino);
        self.detach(parent, name)?;
        Ok(())
    }

    pub fn rename(&mut self, parent: u64, name: &[u8], to_parent: u64, to_name: &[u8], noreplace: bool) -> Result<(), Errno> {
        if !layr_objects::tree::valid_name(to_name) {
            return Err(Errno::EINVAL);
        }
        let ino = self.child(parent, name)?.ok_or(Errno::ENOENT)?;
        self.load(to_parent)?;
        // A link to a cache tree may move (`mv target target-old`); the lab's fixed caches may not.
        if self.n(ino)?.cache && self.n(ino)?.kind != Kind::Symlink {
            return Err(Errno::EPERM);
        }
        // A folder cannot move into itself.
        let mut at = to_parent;
        while at != 0 {
            if at == ino {
                return Err(Errno::EINVAL);
            }
            at = self.n(at)?.parent;
        }
        let from_path = join(&self.path_of(parent), name);
        let to_path = join(&self.path_of(to_parent), to_name);
        if let Some(existing) = self.child(to_parent, to_name)? {
            if existing == ino {
                return Ok(());
            }
            if noreplace {
                return Err(Errno::EEXIST);
            }
            let (src_dir, dst) = (self.n(ino)?.kind == Kind::Dir, self.n(existing)?);
            if dst.cache && dst.kind != Kind::Symlink {
                return Err(Errno::EPERM);
            }
            match (src_dir, dst.kind == Kind::Dir) {
                (true, true) => {
                    self.load(existing)?;
                    if !self.n(existing)?.children.as_ref().expect("loaded").is_empty() {
                        return Err(Errno::ENOTEMPTY);
                    }
                }
                (true, false) => return Err(Errno::ENOTDIR),
                (false, true) => return Err(Errno::EISDIR),
                (false, false) => {}
            }
        }
        // The journal line first: a rename that fails leaves both names as they were.
        self.log(format!("V {} {}\n", hex(&from_path), hex(&to_path)))?;
        if let Some(existing) = self.child(to_parent, to_name)? {
            self.mark_gone(existing);
            self.detach(to_parent, to_name)?;
        }
        self.mark_gone(ino);
        self.nm(parent)?.children.as_mut().expect("loaded").remove(name);
        self.nm(to_parent)?.children.as_mut().expect("loaded").insert(to_name.to_vec(), ino);
        let n = self.nm(ino)?;
        n.parent = to_parent;
        n.name = to_name.to_vec();
        n.ctime = now_ns();
        self.mark(ino);
        self.dir_changed(parent);
        self.dir_changed(to_parent);
        Ok(())
    }

    pub fn chmod(&mut self, ino: u64, mode: u32) -> Result<(), Errno> {
        if self.n(ino)?.cache {
            return Err(Errno::EPERM);
        }
        if self.attached(ino) {
            let path = self.path_of(ino);
            self.log(format!("M {} {:o}\n", hex(&path), mode & 0o7777))?;
        }
        let n = self.nm(ino)?;
        n.mode = mode & 0o7777;
        n.ctime = now_ns();
        self.touch(ino);
        Ok(())
    }

    /// The links of the view to cache trees (`fast_cache`), as (path, target) text pairs.
    pub fn cache_links(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (ino, n) in self.nodes.iter().enumerate() {
            if n.cache && n.kind == Kind::Symlink && ino != 0 && self.attached(ino as u64) {
                out.push((
                    String::from_utf8_lossy(&self.path_of(ino as u64)).into_owned(),
                    String::from_utf8_lossy(&n.target).into_owned(),
                ));
            }
        }
        out
    }

    /// Set (`Some`) or remove (`None`) the extended attribute `name`. Only `user.` attributes:
    /// the object format keeps no others.
    pub fn set_xattr(&mut self, ino: u64, name: &[u8], value: Option<&[u8]>, flags: i32) -> Result<(), Errno> {
        if !name.starts_with(b"user.") || name.len() > 255 {
            return Err(Errno::ENOTSUP);
        }
        let n = self.n(ino)?;
        if n.cache {
            return Err(Errno::EPERM);
        }
        let at = n.xattrs.iter().position(|(k, _)| k.as_slice() == name);
        match (value, at) {
            (Some(_), Some(_)) if flags & libc::XATTR_CREATE != 0 => return Err(Errno::EEXIST),
            (Some(_), None) if flags & libc::XATTR_REPLACE != 0 => return Err(Errno::NO_XATTR),
            (None, None) => return Err(Errno::NO_XATTR),
            _ => {}
        }
        // The journal line first: a change that fails leaves the attributes as they were.
        if self.attached(ino) {
            let path = self.path_of(ino);
            match value {
                Some(v) => self.log(format!("X {} {} {}\n", hex(&path), hex(name), hex(v)))?,
                None => self.log(format!("Y {} {}\n", hex(&path), hex(name)))?,
            }
        }
        let n = self.nm(ino)?;
        match (value, at) {
            (Some(v), Some(i)) => n.xattrs[i].1 = v.to_vec(),
            (Some(v), None) => {
                n.xattrs.push((name.to_vec(), v.to_vec()));
                n.xattrs.sort();
            }
            (None, Some(i)) => {
                n.xattrs.remove(i);
            }
            (None, None) => unreachable!(),
        }
        n.ctime = now_ns();
        if value.is_some() {
            self.has_xattrs = Some(true);
        }
        self.touch(ino);
        Ok(())
    }

    pub fn set_mtime(&mut self, ino: u64, ns: i64) -> Result<(), Errno> {
        if self.n(ino)?.cache {
            return Err(Errno::EPERM);
        }
        if self.attached(ino) {
            let path = self.path_of(ino);
            self.log(format!("T {} {ns}\n", hex(&path)))?;
        }
        let n = self.nm(ino)?;
        n.mtime = ns;
        n.ctime = now_ns();
        let upper = n.upper;
        self.touch(ino);
        if let Some(u) = upper {
            // The upper file keeps the time: a replay reads it there.
            let _ = layr_objects::times::set(&self.upper_path(u), ns);
        }
        Ok(())
    }

    pub fn truncate(&mut self, ino: u64, size: u64) -> Result<(), Errno> {
        let path = self.copy_up(ino)?;
        OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(size)).map_err(Errno::from)?;
        // After `adopt` the file and its folders can be clean: the next snapshot must see it.
        self.touch(ino);
        let n = self.nm(ino)?;
        n.size = size;
        n.mtime = now_ns();
        n.ctime = n.mtime;
        Ok(())
    }

    /// A write through an open upper file.
    pub fn wrote(&mut self, ino: u64, end: u64) {
        // The file and its folders are changed again (after `adopt` they can be clean).
        self.touch(ino);
        if let Ok(n) = self.nm(ino) {
            n.size = n.size.max(end);
            n.mtime = now_ns();
            n.ctime = n.mtime;
        }
    }

    pub fn set_writers(&mut self, ino: u64, delta: i32) {
        if let Ok(n) = self.nm(ino) {
            n.writers = (n.writers as i32 + delta).max(0) as u32;
        }
    }

    fn replay(&mut self, line: &str) -> Result<()> {
        let f: Vec<&str> = line.split(' ').collect();
        let path = unhex(f.get(1).copied().unwrap_or(""))?;
        let (dir, name) = Self::split(&path);
        let e = |e: Errno| anyhow::anyhow!("errno {e:?}");
        let fields = match f[0] {
            "C" | "N" => 4,
            "W" => 5,
            "U" | "D" | "L" | "V" | "M" | "T" | "X" | "Y" => 3,
            _ => 2,
        };
        if f.len() < fields {
            bail!("a journal line with {} of {fields} fields: {line}", f.len());
        }
        match f[0] {
            "C" => {
                let parent = self.resolve(dir).map_err(e)?;
                let ino = self.create(parent, name, u32::from_str_radix(f[3], 8)?, Some(f[2].parse()?)).map_err(e)?;
                let _ = ino;
            }
            "U" => {
                let ino = self.resolve(&path).map_err(e)?;
                let u: u64 = f[2].parse()?;
                let meta = fs::metadata(self.upper_path(u)).ok();
                let n = self.nm(ino).map_err(e)?;
                n.upper = Some(u);
                n.patches.clear();
                n.patch_bytes = 0;
                if let Some(meta) = meta {
                    n.size = meta.len();
                    n.mtime = layr_objects::times::ns_of(&meta);
                }
                self.touch(ino);
            }
            "N" => {
                let parent = self.resolve(dir).map_err(e)?;
                self.create_lazy(parent, name, u32::from_str_radix(f[2], 8)?, Some(f[3].parse()?)).map_err(e)?;
            }
            "W" => {
                let ino = self.resolve(&path).map_err(e)?;
                let (off, data, mtime): (u64, Vec<u8>, i64) = (f[2].parse()?, unhex(f[4])?, f[3].parse()?);
                // A journal of an older layr can hold a patch far past the end of a small file:
                // it becomes a sparse upper file, not bytes in memory.
                if off.checked_add(data.len() as u64).is_none_or(|end| end > SMALL) {
                    let (_, file, written) = self.copy_up_write(ino, Some((off, &data))).map_err(e)?;
                    if !written {
                        let up = self.read_path(ino).map_err(e)?;
                        OpenOptions::new().write(true).open(up)?.write_all_at(&data, off)?;
                    }
                    drop(file);
                    let n = self.nm(ino).map_err(e)?;
                    n.size = n.size.max(off + data.len() as u64);
                    n.mtime = mtime;
                    n.ctime = mtime;
                } else {
                    self.patch(ino, off, &data, Some(mtime)).map_err(e)?;
                }
            }
            "D" => {
                let parent = self.resolve(dir).map_err(e)?;
                self.mkdir(parent, name, u32::from_str_radix(f[2], 8)?).map_err(e)?;
            }
            "L" => {
                let parent = self.resolve(dir).map_err(e)?;
                self.symlink(parent, name, &unhex(f[2])?).map_err(e)?;
            }
            "R" => {
                let parent = self.resolve(dir).map_err(e)?;
                let ino = self.child(parent, name).map_err(e)?.ok_or_else(|| anyhow::anyhow!("no entry"))?;
                // A folder is empty when it goes: its entries went before it.
                let is_dir = self.n(ino).map_err(e)?.kind == Kind::Dir;
                self.remove(parent, name, is_dir).map_err(e)?;
            }
            "V" => {
                let to = unhex(f[2])?;
                let (to_dir, to_name) = Self::split(&to);
                let parent = self.resolve(dir).map_err(e)?;
                let to_parent = self.resolve(to_dir).map_err(e)?;
                self.rename(parent, name, to_parent, to_name, false).map_err(e)?;
            }
            "M" => {
                let ino = self.resolve(&path).map_err(e)?;
                self.chmod(ino, u32::from_str_radix(f[2], 8)?).map_err(e)?;
            }
            "T" => {
                // An upper file keeps its later time (writes after this line are not in the
                // journal): `open` reads it from the file after the replay.
                let ino = self.resolve(&path).map_err(e)?;
                let n = self.nm(ino).map_err(e)?;
                n.mtime = f[2].parse()?;
                n.ctime = n.mtime;
                self.touch(ino);
            }
            "X" => {
                let ino = self.resolve(&path).map_err(e)?;
                self.set_xattr(ino, &unhex(f[2])?, Some(&unhex(f.get(3).copied().unwrap_or(""))?), 0).map_err(e)?;
            }
            "Y" => {
                let ino = self.resolve(&path).map_err(e)?;
                self.set_xattr(ino, &unhex(f[2])?, None, 0).map_err(e)?;
            }
            other => bail!("unknown journal operation {other}"),
        }
        Ok(())
    }

    // ---- status and save -------------------------------------------------------------------

    /// The changes against the head, `A`, `M` and `D` lines. Only dirty folders are read.
    pub fn status(&mut self) -> Result<Vec<String>> {
        let tree = self.store.get_tree(&self.head_ref.tree)?;
        let mut out = Vec::new();
        self.diff_dir(ROOT, Some(&tree), b"", &mut out)?;
        Ok(out)
    }

    fn list_tree(&self, tree: &Id, prefix: &[u8], tag: char, out: &mut Vec<String>) -> Result<()> {
        for e in self.store.get_tree(tree)?.entries {
            let p = join(prefix, &e.name);
            match e.node {
                TreeNode::Dir { tree } => self.list_tree(&tree, &p, tag, out)?,
                _ => out.push(format!("{tag} {}", String::from_utf8_lossy(&p))),
            }
        }
        Ok(())
    }

    fn list_added(&mut self, ino: u64, prefix: &[u8], out: &mut Vec<String>) -> Result<()> {
        if self.n(ino).map_err(|e| anyhow::anyhow!("{e:?}"))?.kind != Kind::Dir {
            out.push(format!("A {}", String::from_utf8_lossy(prefix)));
            return Ok(());
        }
        self.load(ino).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let kids: Vec<(Vec<u8>, u64)> =
            self.nodes[ino as usize].children.as_ref().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (name, c) in kids {
            self.list_added(c, &join(prefix, &name), out)?;
        }
        Ok(())
    }

    fn diff_dir(&mut self, ino: u64, base: Option<&Tree>, prefix: &[u8], out: &mut Vec<String>) -> Result<()> {
        let ae = |e: Errno| anyhow::anyhow!("errno {e:?}");
        self.load(ino).map_err(ae)?;
        let kids: BTreeMap<Vec<u8>, u64> = self.nodes[ino as usize].children.clone().unwrap();
        let base_at: BTreeMap<&[u8], &Entry> =
            base.map(|t| t.entries.iter().map(|e| (e.name.as_slice(), e)).collect()).unwrap_or_default();
        let mut names: Vec<&[u8]> = kids.keys().map(|k| k.as_slice()).chain(base_at.keys().copied()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let p = join(prefix, name);
            let kid = kids.get(name).copied();
            if kid.is_some_and(|k| self.nodes[k as usize].cache) {
                continue;
            }
            match (base_at.get(name), kid) {
                (None, Some(k)) => self.list_added(k, &p, out)?,
                (Some(e), None) => match &e.node {
                    TreeNode::Dir { tree } => self.list_tree(tree, &p, 'D', out)?,
                    _ => out.push(format!("D {}", String::from_utf8_lossy(&p))),
                },
                (Some(e), Some(k)) => {
                    let n = &self.nodes[k as usize];
                    match (&e.node, n.kind) {
                        (TreeNode::Dir { tree }, Kind::Dir) => {
                            if n.dirty || n.tree.map(|t| t.0) != Some(*tree) {
                                let sub = self.store.get_tree(tree)?;
                                self.diff_dir(k, Some(&sub), &p, out)?;
                            }
                        }
                        (TreeNode::File { blob, .. }, Kind::File) => {
                            if n.upper.is_some() || !n.patches.is_empty() || n.blob != Some(*blob) || n.mode != e.mode {
                                out.push(format!("M {}", String::from_utf8_lossy(&p)));
                            }
                        }
                        (TreeNode::Symlink { target }, Kind::Symlink) => {
                            if *target != n.target {
                                out.push(format!("M {}", String::from_utf8_lossy(&p)));
                            }
                        }
                        (TreeNode::Dir { tree }, _) => {
                            self.list_tree(tree, &p, 'D', out)?;
                            out.push(format!("A {}", String::from_utf8_lossy(&p)));
                        }
                        (_, Kind::Dir) => {
                            out.push(format!("D {}", String::from_utf8_lossy(&p)));
                            self.list_added(k, &p, out)?;
                        }
                        _ => out.push(format!("M {}", String::from_utf8_lossy(&p))),
                    }
                }
                (None, None) => {}
            }
        }
        Ok(())
    }

    fn collect_upper(&self, ino: u64, out: &mut Vec<(u64, PathBuf, Option<Id>)>) {
        let n = &self.nodes[ino as usize];
        if !n.dirty {
            return;
        }
        match n.kind {
            Kind::File => {
                if let Some(u) = n.upper {
                    out.push((ino, self.upper_path(u), n.blob));
                }
            }
            Kind::Dir => {
                if let Some(c) = &n.children {
                    for k in c.values() {
                        self.collect_upper(*k, out);
                    }
                }
            }
            Kind::Symlink => {}
        }
    }

    fn build_dir(&mut self, ino: u64) -> Result<(Id, Id)> {
        let n = &self.nodes[ino as usize];
        if let (false, Some(t)) = (n.dirty, n.tree) {
            return Ok(t);
        }
        self.load(ino).map_err(|e| anyhow::anyhow!("errno {e:?}"))?;
        let kids: Vec<(Vec<u8>, u64)> =
            self.nodes[ino as usize].children.as_ref().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
        let mut entries = Vec::with_capacity(kids.len());
        let mut records = Vec::with_capacity(kids.len());
        for (name, k) in kids {
            let c = &self.nodes[k as usize];
            if c.cache {
                continue;
            }
            let (mode, xattrs, mtime) = (c.mode, c.xattrs.clone(), c.mtime);
            match c.kind {
                Kind::File => {
                    let blob = c.blob.context("a file without a blob")?;
                    entries.push(Entry { name, mode, node: TreeNode::File { size: c.size, blob }, xattrs });
                    records.push(Record::Time(mtime));
                }
                Kind::Symlink => {
                    entries.push(Entry { name, mode: 0, node: TreeNode::Symlink { target: c.target.clone() }, xattrs });
                    records.push(Record::Time(mtime));
                }
                Kind::Dir => {
                    let (tree, times) = self.build_dir(k)?;
                    entries.push(Entry { name, mode, node: TreeNode::Dir { tree }, xattrs });
                    records.push(Record::Folder(times));
                }
            }
        }
        let tree = self.store.put_tree(&Tree { entries, links: Vec::new() })?;
        let own = self.nodes[ino as usize].mtime;
        let times = self.store.put_times(&Times { own, records })?;
        self.nodes[ino as usize].tree = Some((tree, times));
        Ok((tree, times))
    }

    /// Mark the saved nodes clean. `dropped` gets the upper files that the saved blobs replace:
    /// the caller removes them once the new journal no longer names them.
    fn clean(&mut self, ino: u64, kept: &mut Vec<u64>, dropped: &mut Vec<u64>) {
        if !self.nodes[ino as usize].dirty {
            return;
        }
        let kids: Vec<u64> =
            self.nodes[ino as usize].children.as_ref().map(|c| c.values().copied().collect()).unwrap_or_default();
        for k in kids {
            self.clean(k, kept, dropped);
        }
        let n = &mut self.nodes[ino as usize];
        n.dirty = false;
        if let Some(u) = n.upper {
            if n.writers == 0 {
                // The blob now holds the content (a clone of the upper file).
                n.upper = None;
                dropped.push(u);
            } else {
                kept.push(ino);
            }
        }
    }

    /// The tree and times of the view's content, written to the store without a state name. The
    /// view keeps its head and its changes (layr records the state and moves the line's head).
    pub fn snapshot(&mut self) -> Result<String> {
        // No prune between the write of objects and the state that names them.
        let _lease = self.store.lease()?;
        let started = Instant::now();
        let (r, files) = self.snapshot_objects()?;
        Ok(format!(
            "{{\"tree\":\"{}\",\"times\":\"{}\",\"files\":{files},\"objects\":{},\"line\":{},\"seconds\":{:.4}}}",
            r.tree,
            r.times,
            serde_json::to_string(&self.store.root().display().to_string())?,
            serde_json::to_string(&self.line.display().to_string())?,
            started.elapsed().as_secs_f64()
        ))
    }

    /// Like `save` without the state: the blobs of changed files go to the store and into the
    /// trees, but the nodes keep their content (upper file, patches). A blob that no
    /// state names can go at the next `gc`; the view must never read from one.
    fn snapshot_objects(&mut self) -> Result<(StateRef, usize)> {
        let mut todo = Vec::new();
        self.collect_patched(ROOT, &mut todo);
        let contents: Vec<(u64, Vec<u8>)> = todo
            .into_iter()
            .map(|ino| Ok((ino, self.content(ino).map_err(|e| anyhow::anyhow!("errno {e:?}"))?)))
            .collect::<Result<_>>()?;
        let store = &self.store;
        let mut blobs: HashMap<u64, (Id, u64)> = HashMap::new();
        for (ino, r) in contents.par_iter().map(|(ino, b)| (*ino, store.put_hot_bytes(b))).collect::<Vec<_>>() {
            blobs.insert(ino, r?);
        }
        let patched = blobs.len();
        let mut files = Vec::new();
        self.collect_upper(ROOT, &mut files);
        for (ino, r) in files.par_iter().map(|(ino, path, base)| (*ino, store.put_file(path, Temperature::Hot, *base))).collect::<Vec<_>>() {
            blobs.insert(ino, r?);
        }
        let mut dirs = HashMap::new();
        let (tree, times) = self.build_dir_with(ROOT, &blobs, &mut dirs)?;
        let r = StateRef { tree, times };
        self.snap_mark = Some((r, self.seq, dirs, blobs));
        Ok((r, files.len() + patched))
    }

    /// The line saved a state whose objects are the last snapshot's, and nothing changed since:
    /// the folders built then become clean, with the state's trees. Later snapshots and status
    /// then read only what changes after it. The nodes keep their content (upper files,
    /// patches); the journal keeps its base. False when the view changed, or a file is open to
    /// write (a write in flight would not show).
    pub fn adopt(&mut self, name: &str) -> Result<bool> {
        let Some(r) = self.store.state_ref(name)? else { return Ok(false) };
        let Some((snap, seq, dirs, blobs)) = self.snap_mark.take() else { return Ok(false) };
        if snap != r || seq != self.seq || self.nodes.iter().any(|n| n.writers > 0) {
            return Ok(false);
        }
        for (ino, t) in &dirs {
            let n = &mut self.nodes[*ino as usize];
            n.tree = Some(*t);
            n.dirty = false;
        }
        for i in 0..self.nodes.len() {
            let parent = self.nodes[i].parent;
            if self.nodes[i].kind != Kind::Dir && dirs.contains_key(&parent) {
                let n = &mut self.nodes[i];
                n.dirty = false;
                // A clean file names the blob of its content: a later build of its folder (a
                // change of a sibling) takes it. An upper file keeps its content; patches are
                // in the blob now, as `store_patched` does (the journal still replays them over
                // its base).
                if let Some(&(blob, size)) = blobs.get(&(i as u64)) {
                    n.blob = Some(blob);
                    n.size = size;
                    if n.upper.is_none() {
                        n.patches.clear();
                        n.patch_bytes = 0;
                    }
                }
            }
        }
        self.head = name.to_string();
        self.head_ref = r;
        // `gc` keeps the state whose trees the clean folders name (`views::fast_bases`).
        fs::write(self.line.join("adopted"), name)?;
        Ok(true)
    }

    /// `build_dir` with the blobs of `blobs` in place of the nodes' own, without caching trees.
    fn build_dir_with(&mut self, ino: u64, blobs: &HashMap<u64, (Id, u64)>, dirs: &mut HashMap<u64, (Id, Id)>) -> Result<(Id, Id)> {
        let n = &self.nodes[ino as usize];
        if let (false, Some(t)) = (n.dirty, n.tree) {
            return Ok(t);
        }
        self.load(ino).map_err(|e| anyhow::anyhow!("errno {e:?}"))?;
        let kids: Vec<(Vec<u8>, u64)> =
            self.nodes[ino as usize].children.as_ref().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
        let mut entries = Vec::with_capacity(kids.len());
        let mut records = Vec::with_capacity(kids.len());
        for (name, k) in kids {
            let c = &self.nodes[k as usize];
            if c.cache {
                continue;
            }
            let (mode, xattrs, mtime) = (c.mode, c.xattrs.clone(), c.mtime);
            match c.kind {
                Kind::File => {
                    let (blob, size) = match blobs.get(&k) {
                        Some(b) => *b,
                        None => (c.blob.context("a file without a blob")?, c.size),
                    };
                    entries.push(Entry { name, mode, node: TreeNode::File { size, blob }, xattrs });
                    records.push(Record::Time(mtime));
                }
                Kind::Symlink => {
                    entries.push(Entry { name, mode: 0, node: TreeNode::Symlink { target: c.target.clone() }, xattrs });
                    records.push(Record::Time(mtime));
                }
                Kind::Dir => {
                    let (tree, times) = self.build_dir_with(k, blobs, dirs)?;
                    entries.push(Entry { name, mode, node: TreeNode::Dir { tree }, xattrs });
                    records.push(Record::Folder(times));
                }
            }
        }
        let tree = self.store.put_tree(&Tree { entries, links: Vec::new() })?;
        let own = self.nodes[ino as usize].mtime;
        let times = self.store.put_times(&Times { own, records })?;
        dirs.insert(ino, (tree, times));
        Ok((tree, times))
    }

    /// Save every change of the view as the state `name`; the view then rests on it.
    pub fn save(&mut self, name: &str) -> Result<String> {
        // No prune between the write of objects and the state that names them.
        let _lease = self.store.lease()?;
        if self.store.state_ref(name)?.is_some() {
            bail!("state {name} already exists");
        }
        let started = Instant::now();
        let patched = self.store_patched()?;
        let mut files = Vec::new();
        self.collect_upper(ROOT, &mut files);
        let store = &self.store;
        let stored: Vec<(u64, Result<(Id, u64)>)> =
            files.par_iter().map(|(ino, path, base)| (*ino, store.put_file(path, Temperature::Hot, *base))).collect();
        let hashed = started.elapsed().as_secs_f64();
        for (ino, r) in stored {
            let (blob, size) = r?;
            let n = &mut self.nodes[ino as usize];
            n.blob = Some(blob);
            n.size = size;
        }
        let (tree, times) = self.build_dir(ROOT)?;
        let r = StateRef { tree, times };
        // A state name is the line's new state: an existing one (another line's) stays as it is.
        self.store.create_state(name, &r)?;
        // The new journal names every upper file that a node keeps: the files open for writing
        // (they stay changed) and the files of clean nodes (`adopt` leaves them clean with their
        // upper file, and their next write goes there). A changed file that nobody writes gives
        // its upper file up: its blob holds the content now (`clean`).
        let journal = self.base_journal(name, |n| n.dirty && n.writers == 0);
        // Nothing in memory changes before the journal: a failed save leaves the view changed.
        self.replace_journal(&journal)?;
        self.head = name.to_string();
        self.head_ref = r;
        let (mut kept, mut dropped) = (Vec::new(), Vec::new());
        self.clean(ROOT, &mut kept, &mut dropped);
        for ino in &kept {
            self.touch(*ino);
        }
        File::open(&self.line)?.sync_all()?;
        for u in dropped {
            let _ = fs::remove_file(self.upper_path(u));
        }
        Ok(format!(
            "{{\"state\":\"{name}\",\"tree\":\"{tree}\",\"times\":\"{times}\",\"files\":{},\"hash_seconds\":{hashed:.4},\"seconds\":{:.4}}}",
            files.len() + patched,
            started.elapsed().as_secs_f64()
        ))
    }

    /// Every node and the change sequence, for another process that takes the connection over
    /// (`layr daemon` restarts): the kernel keeps its node ids, so the numbers must stay.
    pub fn dump(&self) -> serde_json::Value {
        let id = |i: &Id| i.hex();
        let nodes: Vec<serde_json::Value> = self
            .nodes
            .iter()
            .map(|n| {
                serde_json::json!([
                    n.parent,
                    hex(&n.name),
                    match n.kind {
                        Kind::File => 0,
                        Kind::Dir => 1,
                        Kind::Symlink => 2,
                    },
                    n.mode,
                    n.size,
                    n.mtime,
                    n.ctime,
                    n.xattrs.iter().map(|(k, v)| [hex(k), hex(v)]).collect::<Vec<_>>(),
                    n.blob.as_ref().map(id),
                    n.upper,
                    hex(&n.target),
                    n.tree.map(|(t, m)| [t.hex(), m.hex()]),
                    n.children.as_ref().map(|c| c.iter().map(|(k, v)| (hex(k), *v)).collect::<Vec<_>>()),
                    n.dirty,
                    n.cache,
                    n.writers,
                    n.git,
                    n.patches.iter().map(|(o, b)| (*o, hex(b))).collect::<Vec<_>>(),
                ])
            })
            .collect();
        serde_json::json!({
            "nodes": nodes,
            "head": self.head,
            "head_ref": [self.head_ref.tree.hex(), self.head_ref.times.hex()],
            "instance": self.instance,
            "seq": self.seq,
            "node_seq": self.node_seq.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
            "gone": self.gone.iter().map(|(s, p, d)| (*s, hex(p), *d)).collect::<Vec<_>>(),
            "has_xattrs": self.has_xattrs,
            "lazy": self.lazy,
            "next_upper": self.next_upper.load(Ordering::SeqCst),
        })
    }

    /// The view as `dump` left it, on `store` and the line folder `line`.
    pub fn restore(store: Store, line: &Path, caches: Vec<(Vec<u8>, PathBuf)>, d: &serde_json::Value) -> Result<Inner> {
        use serde_json::Value;
        let s = |v: &Value| -> Result<String> { Ok(v.as_str().context("a string")?.to_string()) };
        let u = |v: &Value| -> Result<u64> { v.as_u64().context("a number") };
        let i = |v: &Value| -> Result<i64> { v.as_i64().context("a number") };
        let bytes = |v: &Value| -> Result<Vec<u8>> { unhex(v.as_str().context("hex")?) };
        let idp = |v: &Value| -> Result<Id> { Ok(Id::parse(v.as_str().context("an id")?)?) };
        let mut nodes = Vec::new();
        for n in d["nodes"].as_array().context("nodes")? {
            let f = n.as_array().context("a node")?;
            let mut node = Node::new(u(&f[0])?, bytes(&f[1])?, Kind::File);
            node.kind = match u(&f[2])? {
                1 => Kind::Dir,
                2 => Kind::Symlink,
                _ => Kind::File,
            };
            node.mode = u(&f[3])? as u32;
            node.size = u(&f[4])?;
            node.mtime = i(&f[5])?;
            node.ctime = i(&f[6])?;
            for x in f[7].as_array().context("xattrs")? {
                node.xattrs.push((bytes(&x[0])?, bytes(&x[1])?));
            }
            node.blob = if f[8].is_null() { None } else { Some(idp(&f[8])?) };
            node.upper = f[9].as_u64();
            node.target = bytes(&f[10])?;
            node.tree = if f[11].is_null() { None } else { Some((idp(&f[11][0])?, idp(&f[11][1])?)) };
            node.children = match f[12].as_array() {
                None => None,
                Some(c) => Some(c.iter().map(|e| Ok((bytes(&e[0])?, u(&e[1])?))).collect::<Result<BTreeMap<_, _>>>()?),
            };
            node.dirty = f[13].as_bool().unwrap_or(false);
            node.cache = f[14].as_bool().unwrap_or(false);
            node.writers = u(&f[15])? as u32;
            node.git = f[16].as_array().map(|g| (g[0].as_u64().unwrap_or(0) as u32, g[1].as_u64().unwrap_or(0) as u32));
            for p in f[17].as_array().context("patches")? {
                let b = bytes(&p[1])?;
                node.patch_bytes += b.len();
                node.patches.push((u(&p[0])?, b));
            }
            nodes.push(node);
        }
        let head = s(&d["head"])?;
        let head_ref = StateRef { tree: idp(&d["head_ref"][0])?, times: idp(&d["head_ref"][1])? };
        let next_upper = Arc::new(AtomicU64::new(u(&d["next_upper"])?));
        let journal = OpenOptions::new().append(true).open(line.join("journal"))?;
        Ok(Inner {
            nodes,
            store,
            line: line.to_path_buf(),
            next_upper: next_upper.clone(),
            pool: Arc::new(Pool::new(next_upper, line.join("upper"))),
            journal: Some(journal),
            head,
            head_ref,
            caches,
            snap_mark: None,
            stats: Stats::default(),
            instance: s(&d["instance"])?,
            seq: u(&d["seq"])?,
            node_seq: d["node_seq"].as_array().context("node_seq")?.iter().map(|e| Ok((u(&e[0])?, u(&e[1])?))).collect::<Result<_>>()?,
            gone: d["gone"].as_array().context("gone")?.iter().map(|e| Ok((u(&e[0])?, bytes(&e[1])?, e[2].as_bool().unwrap_or(false)))).collect::<Result<_>>()?,
            git_index: None,
            has_xattrs: d["has_xattrs"].as_bool(),
            lazy: d["lazy"].as_bool().unwrap_or(false),
            remote: None,
            fetched_files: AtomicU64::new(0),
            fetched_bytes: AtomicU64::new(0),
            fetch_ns: AtomicU64::new(0),
        })
    }

    /// Whether the view has changes that no state holds.
    pub fn changed(&self) -> bool {
        self.nodes[ROOT as usize].dirty
    }

    /// Lab: rest the view on the state `name` (no unsaved changes may exist). The caller
    /// invalidates what this returns, as after `reset`.
    pub fn goto(&mut self, name: &str) -> Result<(Vec<u64>, Vec<(u64, Vec<u8>)>)> {
        if self.changed() {
            bail!("the view has unsaved changes");
        }
        let r = self.store.state_ref(name)?.with_context(|| format!("no state {name}"))?;
        self.move_to(name, r)
    }

    /// Lab: rest the view (without unsaved changes) on `r`. Only folders whose objects differ are
    /// visited; nodes of unchanged files and folders stay, so the kernel keeps their names, pages
    /// and attributes. Returns what the kernel must drop.
    fn move_to(&mut self, name: &str, r: StateRef) -> Result<(Vec<u64>, Vec<(u64, Vec<u8>)>)> {
        let own = self.store.get_times(&r.times)?.own;
        let (mut inodes, mut entries) = (Vec::new(), Vec::new());
        self.switch(ROOT, (r.tree, r.times), own, &mut inodes, &mut entries)?;
        self.head = name.to_string();
        self.head_ref = r;
        // git's next fsmonitor query is a full scan.
        self.instance = format!("{:016x}", rand::random::<u64>());
        // Nodes that stay keep their upper files (a clean file after `adopt`): the journal names them.
        let journal = self.base_journal(name, |_| false);
        self.replace_journal(&journal)?;
        let _ = fs::remove_file(self.line.join("rebase-base"));
        Ok((inodes, entries))
    }

    fn switch(
        &mut self,
        ino: u64,
        new: (Id, Id),
        mtime: i64,
        inodes: &mut Vec<u64>,
        entries: &mut Vec<(u64, Vec<u8>)>,
    ) -> Result<()> {
        if self.nodes[ino as usize].tree == Some(new) && !self.nodes[ino as usize].dirty {
            return Ok(());
        }
        inodes.push(ino);
        let Some(old) = self.nodes[ino as usize].children.clone() else {
            let n = &mut self.nodes[ino as usize];
            n.tree = Some(new);
            n.mtime = mtime;
            n.ctime = now_ns();
            n.dirty = false;
            return Ok(());
        };
        let (tree, times, owns) = Self::fetch(&self.store, &new.0, &new.1).map_err(|e| anyhow::anyhow!("errno {e:?}"))?;
        let mut owns = owns.into_iter();
        let mut map = BTreeMap::new();
        for (e, r) in tree.entries.into_iter().zip(times.records) {
            let name = e.name.clone();
            let keep = old.get(&name).copied().filter(|&c| {
                let n = &self.nodes[c as usize];
                !n.cache
                    && n.upper.is_none()
                    && n.patches.is_empty()
                    && n.mode == e.mode
                    && n.xattrs == e.xattrs
                    && match (&e.node, &r) {
                        (TreeNode::Dir { .. }, Record::Folder(_)) => n.kind == Kind::Dir,
                        (TreeNode::File { size, blob }, Record::Time(t)) => {
                            n.kind == Kind::File && n.blob == Some(*blob) && n.size == *size && n.mtime == *t
                        }
                        (TreeNode::Symlink { target }, Record::Time(t)) => {
                            n.kind == Kind::Symlink && &n.target == target && n.mtime == *t
                        }
                        _ => false,
                    }
            });
            let own = if matches!(r, Record::Folder(_)) { owns.next() } else { None };
            match (keep, &e.node, r) {
                (Some(c), TreeNode::Dir { tree }, Record::Folder(tid)) => {
                    self.switch(c, (*tree, tid), own.unwrap_or(0), inodes, entries)?;
                    map.insert(name, c);
                }
                (Some(c), _, _) => {
                    map.insert(name, c);
                }
                (None, _, r) => {
                    if let Some(&c) = old.get(&name) {
                        self.nodes[c as usize].parent = 0;
                        inodes.push(c);
                    }
                    entries.push((ino, name.clone()));
                    let mut c = Node::new(ino, name.clone(), Kind::File);
                    c.mode = e.mode;
                    c.xattrs = e.xattrs;
                    match (e.node, r) {
                        (TreeNode::File { size, blob }, Record::Time(t)) => {
                            c.size = size;
                            c.blob = Some(blob);
                            c.mtime = t;
                        }
                        (TreeNode::Symlink { target }, Record::Time(t)) => {
                            c.kind = Kind::Symlink;
                            c.size = target.len() as u64;
                            c.target = target;
                            c.mtime = t;
                        }
                        (TreeNode::Dir { tree }, Record::Folder(tid)) => {
                            c.kind = Kind::Dir;
                            c.tree = Some((tree, tid));
                            c.mtime = own.unwrap_or(0);
                        }
                        _ => bail!("the times table does not match the tree"),
                    }
                    c.ctime = c.mtime;
                    let id = self.add(c);
                    map.insert(name, id);
                }
            }
        }
        for (name, c) in &old {
            if self.nodes[*c as usize].cache {
                map.insert(name.clone(), *c);
            } else if !map.contains_key(name) {
                self.nodes[*c as usize].parent = 0;
                inodes.push(*c);
                entries.push((ino, name.clone()));
            }
        }
        let n = &mut self.nodes[ino as usize];
        n.children = Some(map);
        n.tree = Some(new);
        n.mtime = mtime;
        n.ctime = now_ns();
        n.dirty = false;
        Ok(())
    }

    /// Lab: save the changes of the view, then merge them onto the state `onto` (a three-way merge
    /// of tree objects: base = the head before, ours = the saved changes, theirs = `onto`).
    /// Folders that only one side changed are taken whole, so the cost follows the changed
    /// folders, not the tree. The result is the state `name`, and the view rests on it.
    /// A conflict leaves the view on the saved changes and lists the conflicting paths.
    pub fn rebase(&mut self, onto: &str, name: &str) -> Result<(String, Vec<u64>, Vec<(u64, Vec<u8>)>)> {
        // No prune between the write of objects and the state that names them.
        let _lease = self.store.lease()?;
        let t = Instant::now();
        // A rebase that stopped on a conflict saved the changes: the next one merges from the
        // same base, or the saved changes would count as the base and the conflict would vanish.
        let mark = self.line.join("rebase-base");
        let base = match fs::read_to_string(&mark) {
            Ok(b) => self.store.state_ref(b.trim())?.with_context(|| format!("no state {} (the base of a rebase)", b.trim()))?,
            Err(_) => self.head_ref,
        };
        if self.changed() {
            if !mark.exists() {
                fs::write(&mark, &self.head)?;
            }
            self.save(&format!("{name}-ours"))?;
        }
        let t_save = t.elapsed().as_secs_f64();
        let ours = self.head_ref;
        let theirs = self.store.state_ref(onto)?.with_context(|| format!("no state {onto}"))?;
        let mut m = Merge { store: &self.store, conflicts: Vec::new(), dirs: 0 };
        let merged = m.dir(Some((base.tree, base.times)), (ours.tree, ours.times), (theirs.tree, theirs.times), b"")?;
        let t_merge = t.elapsed().as_secs_f64() - t_save;
        if !m.conflicts.is_empty() {
            let list: Vec<String> = m.conflicts.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect();
            bail!("{} conflicts: {}", list.len(), list.join(", "));
        }
        let dirs = m.dirs;
        let r = StateRef { tree: merged.0, times: merged.1 };
        // A state name is new: an existing state (another line's) stays as it is.
        self.store.create_state(name, &r)?;
        let (inodes, entries) = self.move_to(name, r)?;
        Ok((
            format!(
                "{{\"state\":\"{name}\",\"tree\":\"{}\",\"merged_dirs\":{dirs},\"save_seconds\":{t_save:.4},\"merge_seconds\":{t_merge:.4},\"seconds\":{:.4}}}",
                r.tree,
                t.elapsed().as_secs_f64()
            ),
            inodes,
            entries,
        ))
    }
}

/// The merged value of one field: the side that changed it, None when both changed it differently.
fn pick<'a, T: PartialEq>(base: Option<&T>, ours: &'a T, theirs: &'a T) -> Option<&'a T> {
    if ours == theirs || base == Some(theirs) {
        Some(ours)
    } else if base == Some(ours) {
        Some(theirs)
    } else {
        None
    }
}

/// Lab: a three-way merge of folders by object ids.
struct Merge<'a> {
    store: &'a Store,
    conflicts: Vec<Vec<u8>>,
    dirs: u64,
}

type Side = Option<(Entry, Record)>;

impl Merge<'_> {
    fn entries(&self, d: (Id, Id)) -> Result<(BTreeMap<Vec<u8>, (Entry, Record)>, i64, Vec<Vec<Vec<u8>>>)> {
        let tree = self.store.get_tree(&d.0)?;
        let times = self.store.get_times(&d.1)?;
        let own = times.own;
        let links = tree.links;
        Ok((tree.entries.into_iter().zip(times.records).map(|(e, r)| (e.name.clone(), (e, r))).collect(), own, links))
    }

    fn dir(&mut self, b: Option<(Id, Id)>, o: (Id, Id), t: (Id, Id), path: &[u8]) -> Result<(Id, Id)> {
        if o == t || b == Some(t) {
            return Ok(o);
        }
        if b == Some(o) {
            return Ok(t);
        }
        self.dirs += 1;
        let (eb, ob, lb) = match b {
            Some(b) => self.entries(b)?,
            None => (BTreeMap::new(), 0, Vec::new()),
        };
        let (eo, oo, lo) = self.entries(o)?;
        let (et, ot, lt) = self.entries(t)?;
        let mut names: std::collections::BTreeSet<&Vec<u8>> = eo.keys().collect();
        names.extend(et.keys());
        let mut entries = Vec::new();
        let mut records = Vec::new();
        for name in names {
            let (sb, so, st): (Side, Side, Side) = (eb.get(name).cloned(), eo.get(name).cloned(), et.get(name).cloned());
            let pick = if so == st || sb == st {
                so
            } else if sb == so {
                st
            } else {
                match (&so, &st) {
                    (Some((e1, Record::Folder(x1))), Some((e2, Record::Folder(x2)))) => {
                        let (TreeNode::Dir { tree: t1 }, TreeNode::Dir { tree: t2 }) = (&e1.node, &e2.node) else {
                            unreachable!()
                        };
                        let bd = match &sb {
                            Some((Entry { node: TreeNode::Dir { tree }, .. }, Record::Folder(x))) => Some((*tree, *x)),
                            _ => None,
                        };
                        let sub = join(path, name);
                        let (tree, times) = self.dir(bd, (*t1, *x1), (*t2, *x2), &sub)?;
                        // The mode and the attributes of the folder merge as the folder does: the
                        // side that changed one wins; two different changes conflict.
                        let base = sb.as_ref().map(|s| &s.0);
                        let mode = pick(base.map(|b| &b.mode), &e1.mode, &e2.mode);
                        let xattrs = pick(base.map(|b| &b.xattrs), &e1.xattrs, &e2.xattrs);
                        match (mode, xattrs) {
                            (Some(mode), Some(xattrs)) => Some((
                                Entry { name: name.clone(), mode: *mode, node: TreeNode::Dir { tree }, xattrs: xattrs.clone() },
                                Record::Folder(times),
                            )),
                            _ => {
                                self.conflicts.push(sub);
                                so
                            }
                        }
                    }
                    _ => {
                        self.conflicts.push(join(path, name));
                        so
                    }
                }
            };
            if let Some((e, r)) = pick {
                entries.push(e);
                records.push(r);
            }
        }
        let own = if ob == oo { ot } else { oo };
        let links = if lb == lo { lt } else { lo };
        let tree = self.store.put_tree(&Tree { entries, links })?;
        let times = self.store.put_times(&Times { own, records })?;
        Ok((tree, times))
    }
}

// ---- FUSE ------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct Config {
    pub ttl: Duration,
    pub uid: u32,
    pub gid: u32,
    /// The users that may use the mount (with allow_other); empty: everyone the kernel lets in.
    pub allowed: Vec<u32>,
    pub writeback: bool,
    /// The kernel opens files without asking (open answers ENOSYS): reads and writes come with
    /// file handle 0 and use the descriptors of `Fds`.
    pub no_open: bool,
    /// Ask the kernel for FUSE over io_uring.
    pub uring: bool,
}

/// Lab: the nodes that requests touched while a trace runs. A trace sets the timeout of names and
/// attributes to zero, so every lookup and stat reaches the server.
#[derive(Default)]
pub struct Trace {
    pub on: std::sync::atomic::AtomicBool,
    sets: Mutex<TraceSets>,
}

#[derive(Default)]
pub struct TraceSets {
    pub stat: std::collections::HashSet<u64>,
    pub read: std::collections::HashSet<u64>,
    pub listed: std::collections::HashSet<u64>,
    pub missing: std::collections::HashSet<(u64, Vec<u8>)>,
    pub wrote: std::collections::HashSet<u64>,
}

impl Trace {
    fn note(&self, f: impl FnOnce(&mut TraceSets)) {
        if self.on.load(Ordering::Relaxed) {
            f(&mut self.sets.lock().unwrap());
        }
    }

    pub fn start(&self) {
        *self.sets.lock().unwrap() = TraceSets::default();
        self.on.store(true, Ordering::SeqCst);
    }

    pub fn stop(&self) -> TraceSets {
        self.on.store(false, Ordering::SeqCst);
        std::mem::take(&mut *self.sets.lock().unwrap())
    }
}

/// Descriptors of backing files by node, for the no-open mode. A writable one is an upper file.
/// A save and a reset clear it (they remove upper files) under the exclusive lock of the view;
/// reads and writes use it under the shared lock.
#[derive(Default)]
pub struct Fds {
    map: Mutex<HashMap<u64, (Arc<File>, bool)>>,
}

impl Fds {
    const MAX: usize = 16384;

    pub fn clear(&self) {
        self.map.lock().unwrap().clear();
    }

    fn get(&self, ino: u64, write: bool) -> Option<Arc<File>> {
        self.map.lock().unwrap().get(&ino).filter(|(_, w)| *w || !write).map(|(f, _)| f.clone())
    }

    fn put(&self, ino: u64, file: File, write: bool) -> Arc<File> {
        let f = Arc::new(file);
        let mut m = self.map.lock().unwrap();
        if m.len() >= Self::MAX {
            m.clear();
        }
        m.insert(ino, (f.clone(), write));
        f
    }

    fn drop_ino(&self, ino: u64) {
        self.map.lock().unwrap().remove(&ino);
    }
}

/// Requests per kind, for `stats`: they show what the kernel's caches answer without the server.
#[derive(Default)]
pub struct Counters {
    pub lookup: AtomicU64,
    pub getattr: AtomicU64,
    pub open: AtomicU64,
    pub read: AtomicU64,
    pub readdir: AtomicU64,
    pub write: AtomicU64,
    pub create: AtomicU64,
    pub other: AtomicU64,
    pub ops: Mutex<BTreeMap<&'static str, u64>>,
    /// Time spent in write handlers (ns).
    pub write_ns: AtomicU64,
    pub read_ns: AtomicU64,
}

impl Counters {
    pub fn json(&self) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "{{\"lookup\":{},\"getattr\":{},\"open\":{},\"read\":{},\"readdir\":{},\"write\":{},\"create\":{},\"other\":{},\"write_us\":{},\"read_us\":{},\"ops\":{}}}",
            g(&self.lookup),
            g(&self.getattr),
            g(&self.open),
            g(&self.read),
            g(&self.readdir),
            g(&self.write),
            g(&self.create),
            g(&self.other),
            g(&self.write_ns) / 1000,
            g(&self.read_ns) / 1000,
            serde_json::to_string(&*self.ops.lock().unwrap()).unwrap_or_default()
        )
    }
}

/// Adds the time since `1` to the counter `0` when dropped.
struct Timer<'a>(&'a AtomicU64, Instant);

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(self.1.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

struct Handle {
    file: Arc<File>,
    ino: u64,
    writer: bool,
}

pub struct View {
    pub inner: Arc<RwLock<Inner>>,
    pub counters: Arc<Counters>,
    fds: Arc<Fds>,
    /// Open folders of the hot blobs (`blobs/xx`): a read opens its blob relative to one.
    blob_dirs: RwLock<HashMap<PathBuf, std::os::fd::OwnedFd>>,
    open: Arc<OpenFiles>,
    cfg: Config,
    pub trace: Arc<Trace>,
}

/// The files that the kernel opened through the view (not in the no-open mode). They outlive a
/// `View`: a server that serves its line again after a failed handoff keeps them.
pub struct OpenFiles {
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
}

impl OpenFiles {
    pub fn new() -> OpenFiles {
        OpenFiles { handles: Mutex::new(HashMap::new()), next_fh: AtomicU64::new(1) }
    }

    /// True when no file is open: another process can take the line over.
    pub fn is_empty(&self) -> bool {
        self.handles.lock().unwrap().is_empty()
    }
}

impl View {
    pub fn new(
        inner: Arc<RwLock<Inner>>,
        counters: Arc<Counters>,
        fds: Arc<Fds>,
        open: Arc<OpenFiles>,
        cfg: Config,
        trace: Arc<Trace>,
    ) -> View {
        View { trace, inner, counters, fds, blob_dirs: RwLock::new(HashMap::new()), open, cfg }
    }

    /// The file behind a request: its handle, or in the no-open mode the descriptor of the node
    /// (a write gives the node its upper copy first). Called with the shared lock held.
    fn backing(&self, inner: &Inner, fh: FileHandle, ino: u64, write: bool) -> Result<Arc<File>, Errno> {
        if fh.0 != 0 {
            return self.handle(fh).map(|(f, _)| f).ok_or(Errno::EBADF);
        }
        let n = inner.n(ino)?;
        if !write {
            // Reads open their file each time (the kernel keeps the pages): no shared cache.
            return self.open_read(inner, ino).map(Arc::new);
        }
        if let Some(f) = self.fds.get(ino, write) {
            return Ok(f);
        }
        if write && n.upper.is_none() {
            // The caller copies up under the exclusive lock and asks again.
            return Err(Errno::EAGAIN);
        }
        let file = match (write, n.upper, n.blob) {
            // A lower file: open its hot blob without a check first; a cold blob is decoded.
            (false, None, Some(b)) => match File::open(inner.store.hot_candidate(&b)) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::open(inner.blob_file(&b)?).map_err(Errno::from)?,
                Err(e) => return Err(Errno::from(e)),
            },
            _ => OpenOptions::new().read(true).write(write).open(inner.read_path(ino)?).map_err(Errno::from)?,
        };
        Ok(self.fds.put(ino, file, write))
    }

    fn ttl(&self) -> Duration {
        if self.trace.on.load(Ordering::Relaxed) {
            Duration::ZERO
        } else {
            self.cfg.ttl
        }
    }

    fn allowed(&self, req: &Request) -> bool {
        self.cfg.allowed.is_empty() || self.cfg.allowed.contains(&req.uid())
    }

    fn attr(&self, inner: &Inner, ino: u64) -> Result<FileAttr, Errno> {
        let n = inner.n(ino)?;
        let kind = match n.kind {
            Kind::File => FileType::RegularFile,
            Kind::Dir => FileType::Directory,
            Kind::Symlink => FileType::Symlink,
        };
        // The root has no permission for others. With names, attributes and listings cached in
        // the kernel, a request of another user may never reach the server's uid check, so the
        // kernel's own check (default_permissions) must stop it at the root. A tree holds no mode
        // of its root, so every saved mode stays real. The group is the server's own: the server
        // reads its mount (the index of `layr tool`) through the group bits.
        let perm = match n.kind {
            Kind::Symlink => 0o777,
            _ if ino == ROOT => (n.mode & 0o7750) as u16,
            _ => n.mode as u16,
        };
        Ok(FileAttr {
            ino: INodeNo(ino),
            size: n.size,
            // An upper file can have holes: its own count.
            blocks: n
                .upper
                .and_then(|u| fs::metadata(inner.upper_path(u)).ok())
                .map_or(n.size.div_ceil(512), |m| std::os::unix::fs::MetadataExt::blocks(&m)),
            atime: time_of(n.mtime),
            mtime: time_of(n.mtime),
            ctime: time_of(n.ctime),
            crtime: time_of(n.ctime),
            kind,
            perm,
            // 1 for a folder: "unknown", as btrfs gives. A count of 2 would tell find that a folder has no subfolders.
            nlink: if inner.attached(ino) { 1 } else { 0 },
            uid: self.cfg.uid,
            gid: n.git.map(|g| g.1).unwrap_or(self.cfg.gid),
            rdev: 0,
            blksize: 4096,
            flags: 0,
            // git's recorded inode number; other nodes above the 32 bits that git stores.
            st_ino: match n.git {
                Some((g, _)) => g as u64,
                None => ino | (1 << 32),
            },
        })
    }

    fn negative(&self) -> FileAttr {
        FileAttr {
            ino: INodeNo(0),
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0,
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
            st_ino: 0,
        }
    }

    fn open_handle(&self, file: File, ino: u64, writer: bool) -> u64 {
        let fh = self.open.next_fh.fetch_add(1, Ordering::Relaxed);
        self.open.handles.lock().unwrap().insert(fh, Handle { file: Arc::new(file), ino, writer });
        fh
    }

    fn handle(&self, fh: FileHandle) -> Option<(Arc<File>, u64)> {
        self.open.handles.lock().unwrap().get(&fh.0).map(|h| (h.file.clone(), h.ino))
    }

    /// A shared lock on the view with the entries of folder `ino` loaded (they are loaded under
    /// the exclusive lock first when needed; a reset can drop them in between, so it loops).
    fn loaded_dir(&self, ino: u64) -> Result<std::sync::RwLockReadGuard<'_, Inner>, Errno> {
        loop {
            let inner = self.inner.read().unwrap();
            let n = inner.n(ino)?;
            if n.kind != Kind::Dir {
                return Err(Errno::ENOTDIR);
            }
            if n.children.is_some() {
                return Ok(inner);
            }
            drop(inner);
            self.inner.write().unwrap().load(ino)?;
        }
    }

    /// Open the file to read a node from: its upper file, or its hot blob relative to the open
    /// folder of the blob (a cold blob is decoded first).
    fn open_read(&self, inner: &Inner, ino: u64) -> Result<File, Errno> {
        let n = inner.n(ino)?;
        if let (None, Some(b)) = (n.upper, n.blob) {
            let p = inner.store.hot_candidate(&b);
            if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
                let opened = {
                    let dirs = self.blob_dirs.read().unwrap();
                    dirs.get(dir).map(|d| {
                        rustix::fs::openat(
                            d,
                            name,
                            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
                            rustix::fs::Mode::empty(),
                        )
                    })
                };
                let opened = match opened {
                    Some(r) => r,
                    None => match rustix::fs::open(
                        dir,
                        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    ) {
                        Ok(d) => {
                            let r = rustix::fs::openat(
                                &d,
                                name,
                                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
                                rustix::fs::Mode::empty(),
                            );
                            self.blob_dirs.write().unwrap().insert(dir.to_path_buf(), d);
                            r
                        }
                        Err(e) => Err(e),
                    },
                };
                match opened {
                    Ok(fd) => return Ok(File::from(fd)),
                    Err(e) if e == rustix::io::Errno::NOENT => {}
                    Err(e) => return Err(Errno::from_i32(e.raw_os_error())),
                }
            }
        }
        File::open(inner.read_path(ino)?).map_err(Errno::from)
    }

    fn entry_reply(&self, inner: &Inner, ino: u64, reply: ReplyEntry) {
        match self.attr(inner, ino) {
            Ok(a) => reply.entry(&self.ttl(), &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }
}

macro_rules! check {
    ($self:ident, $req:ident, $reply:ident) => {
        if !$self.allowed($req) {
            $reply.error(Errno::EACCES);
            return;
        }
    };
}

macro_rules! tri {
    ($e:expr, $reply:ident) => {
        match $e {
            Ok(v) => v,
            Err(e) => {
                $reply.error(e);
                return;
            }
        }
    };
}

impl Filesystem for View {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let mut want = InitFlags::FUSE_PARALLEL_DIROPS
            // Always readdirplus (not the adaptive mode): a walk leaves every name and its
            // attributes in the kernel, so later lookups need no request.
            | InitFlags::FUSE_DO_READDIRPLUS
            | InitFlags::FUSE_CACHE_SYMLINKS
            | InitFlags::FUSE_MAX_PAGES
            | InitFlags::FUSE_ASYNC_READ
            | InitFlags::FUSE_BIG_WRITES
            // The kernel asks no `security.capability` attribute before each write.
            | InitFlags::FUSE_HANDLE_KILLPRIV_V2;
        if self.cfg.writeback {
            want |= InitFlags::FUSE_WRITEBACK_CACHE;
        }
        if self.cfg.uring {
            want |= InitFlags::FUSE_OVER_IO_URING;
        }
        for flag in want.iter() {
            if config.add_capabilities(flag).is_err() {
                eprintln!("layrfs-fuse: the kernel lacks {flag:?}");
            }
        }
        let _ = config.set_max_readahead(1 << 20);
        let _ = config.set_max_background(64);
        let _ = config.set_max_write(1 << 20);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        check!(self, req, reply);
        self.counters.lookup.fetch_add(1, Ordering::Relaxed);
        {
            let inner = self.inner.read().unwrap();
            if let Some(found) = inner.loaded_child(parent.0, name.as_bytes()) {
                match tri!(found, reply) {
                    Some(ino) => {
                        self.trace.note(|t| {
                            t.stat.insert(ino);
                        });
                        self.entry_reply(&inner, ino, reply)
                    }
                    None => {
                        self.trace.note(|t| {
                            t.missing.insert((parent.0, name.as_bytes().to_vec()));
                        });
                        reply.entry(&self.ttl(), &self.negative(), Generation(0))
                    }
                }
                return;
            }
        }
        let mut inner = self.inner.write().unwrap();
        match tri!(inner.child(parent.0, name.as_bytes()), reply) {
            Some(ino) => {
                self.trace.note(|t| {
                    t.stat.insert(ino);
                });
                self.entry_reply(&inner, ino, reply)
            }
            // A zero node id with a timeout: the kernel keeps the miss (a negative entry).
            None => {
                self.trace.note(|t| {
                    t.missing.insert((parent.0, name.as_bytes().to_vec()));
                });
                reply.entry(&self.ttl(), &self.negative(), Generation(0))
            }
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        check!(self, req, reply);
        self.counters.getattr.fetch_add(1, Ordering::Relaxed);
        self.trace.note(|t| {
            t.stat.insert(ino.0);
        });
        let inner = self.inner.read().unwrap();
        match self.attr(&inner, ino.0) {
            Ok(a) => reply.attr(&self.ttl(), &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("setattr").or_default() += 1;
        check!(self, req, reply);
        if uid.is_some_and(|u| u != self.cfg.uid) || gid.is_some_and(|g| g != self.cfg.gid) {
            reply.error(Errno::EPERM);
            return;
        }
        let mut inner = self.inner.write().unwrap();
        if let Some(m) = mode {
            tri!(inner.chmod(ino.0, m), reply);
        }
        if let Some(s) = size {
            match fh.and_then(|fh| self.handle(fh)) {
                Some((file, h_ino)) if h_ino == ino.0 && inner.n(ino.0).map(|n| n.upper.is_some()).unwrap_or(false) => {
                    tri!(file.set_len(s).map_err(Errno::from), reply);
                    inner.wrote(ino.0, 0);
                    if let Ok(n) = inner.nm(ino.0) {
                        n.size = s;
                    }
                }
                _ => {
                    tri!(inner.truncate(ino.0, s), reply);
                    // A cached read-only descriptor must not serve the node after its copy-up.
                    self.fds.drop_ino(ino.0);
                }
            }
        }
        if let Some(t) = mtime {
            let ns = match t {
                TimeOrNow::Now => now_ns(),
                TimeOrNow::SpecificTime(t) => ns_of_time(t),
            };
            tri!(inner.set_mtime(ino.0, ns), reply);
        }
        match self.attr(&inner, ino.0) {
            Ok(a) => reply.attr(&self.ttl(), &a),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, req: &Request, ino: INodeNo, reply: ReplyData) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("readlink").or_default() += 1;
        check!(self, req, reply);
        let inner = self.inner.read().unwrap();
        let n = tri!(inner.n(ino.0), reply);
        if n.kind != Kind::Symlink {
            reply.error(Errno::EINVAL);
            return;
        }
        reply.data(&n.target);
    }

    fn mknod(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        // A tree holds files, folders and links only.
        reply.error(Errno::EPERM);
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("mkdir").or_default() += 1;
        check!(self, req, reply);
        let mut inner = self.inner.write().unwrap();
        let ino = tri!(inner.mkdir(parent.0, name.as_bytes(), mode & !umask), reply);
        self.entry_reply(&inner, ino, reply);
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("unlink").or_default() += 1;
        check!(self, req, reply);
        let mut inner = self.inner.write().unwrap();
        tri!(inner.remove(parent.0, name.as_bytes(), false), reply);
        reply.ok();
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("rmdir").or_default() += 1;
        check!(self, req, reply);
        let mut inner = self.inner.write().unwrap();
        tri!(inner.remove(parent.0, name.as_bytes(), true), reply);
        reply.ok();
    }

    fn symlink(&self, req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        check!(self, req, reply);
        let mut inner = self.inner.write().unwrap();
        let ino = tri!(inner.symlink(parent.0, link_name.as_bytes(), target.as_os_str().as_bytes()), reply);
        self.entry_reply(&inner, ino, reply);
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("rename").or_default() += 1;
        check!(self, req, reply);
        if flags.contains(RenameFlags::RENAME_EXCHANGE) || flags.contains(RenameFlags::RENAME_WHITEOUT) {
            reply.error(Errno::EINVAL);
            return;
        }
        let mut inner = self.inner.write().unwrap();
        tri!(
            inner.rename(
                parent.0,
                name.as_bytes(),
                newparent.0,
                newname.as_bytes(),
                flags.contains(RenameFlags::RENAME_NOREPLACE)
            ),
            reply
        );
        reply.ok();
    }

    fn link(&self, _req: &Request, _ino: INodeNo, _newparent: INodeNo, _newname: &OsStr, reply: ReplyEntry) {
        // Hard links are not part of the prototype; git and cargo fall back to a copy or a rename.
        reply.error(Errno::EPERM);
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        check!(self, req, reply);
        self.counters.open.fetch_add(1, Ordering::Relaxed);
        if self.cfg.no_open {
            // The kernel then opens every file itself (FUSE_NO_OPEN_SUPPORT) and keeps its cache.
            reply.error(Errno::ENOSYS);
            return;
        }
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY || flags.0 & libc::O_TRUNC != 0;
        if !write {
            self.trace.note(|t| {
                t.read.insert(ino.0);
            });
            let path = tri!(self.inner.read().unwrap().read_path(ino.0), reply);
            let file = tri!(File::open(&path).map_err(Errno::from), reply);
            let fh = self.open_handle(file, ino.0, false);
            reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
            return;
        }
        let mut inner = self.inner.write().unwrap();
        let path = if write {
            let p = tri!(inner.copy_up(ino.0), reply);
            // A second writer of an upper file makes no copy-up: record the change here.
            inner.mark(ino.0);
            p
        } else {
            tri!(inner.read_path(ino.0), reply)
        };
        // With the writeback cache the kernel can read through a handle that was opened to write.
        let file = tri!(OpenOptions::new().read(true).write(write).open(&path).map_err(Errno::from), reply);
        if write {
            inner.set_writers(ino.0, 1);
        }
        drop(inner);
        let fh = self.open_handle(file, ino.0, write);
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        // No uid check on a handle: it was checked at open, and the kernel's own requests
        // (writeback, readahead) carry uid 0.
        let _ = req;
        self.counters.read.fetch_add(1, Ordering::Relaxed);
        self.trace.note(|t| {
            t.read.insert(_ino.0);
        });
        let _timer = Timer(&self.counters.read_ns, Instant::now());
        {
            // A patched or new file: its bytes come from memory and the journal's patches.
            let inner = self.inner.read().unwrap();
            if let Ok(n) = inner.n(_ino.0) {
                if n.kind == Kind::File && n.upper.is_none() && (!n.patches.is_empty() || n.blob.is_none()) {
                    let bytes = tri!(inner.content(_ino.0), reply);
                    let start = (offset as usize).min(bytes.len());
                    let end = (start + size as usize).min(bytes.len());
                    reply.data(&bytes[start..end]);
                    return;
                }
            }
        }
        let (file, len) = {
            let inner = self.inner.read().unwrap();
            // A lower file never changes: read no more than its size (the kernel asks for its
            // whole readahead window, and the buffer is zeroed first).
            let len = match inner.n(_ino.0) {
                Ok(n) if n.upper.is_none() => (size as u64).min(n.size.saturating_sub(offset)) as usize,
                _ => size as usize,
            };
            (tri!(self.backing(&inner, fh, _ino.0, false), reply), len)
        };
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < buf.len() {
            match file.read_at(&mut buf[got..], offset + got as u64) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    reply.error(Errno::from(e));
                    return;
                }
            }
        }
        reply.data(&buf[..got]);
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.trace.note(|t| {
            t.wrote.insert(ino.0);
        });
        let _ = req;
        self.counters.write.fetch_add(1, Ordering::Relaxed);
        let _timer = Timer(&self.counters.write_ns, Instant::now());
        if _write_flags.contains(WriteFlags::FUSE_WRITE_KILL_SUIDGID) {
            // FUSE_HANDLE_KILLPRIV_V2: the server clears set-user-id and set-group-id bits, for a
            // patch and for a write to a file.
            let mut w = self.inner.write().unwrap();
            let mode = w.n(ino.0).map(|n| n.mode).unwrap_or(0);
            if mode & 0o6000 != 0 {
                tri!(w.chmod(ino.0, mode & !0o6000), reply);
            }
        }
        if fh.0 == 0 && self.cfg.no_open {
            let mut w = self.inner.write().unwrap();
            if w.can_patch(ino.0, offset as u64, data.len()) {
                tri!(w.patch(ino.0, offset, data, None), reply);
                drop(w);
                reply.written(data.len() as u32);
                return;
            }
        }
        {
            // The shared lock keeps a save or a reset from removing the upper file meanwhile.
            let inner = self.inner.read().unwrap();
            let file = match self.backing(&inner, fh, ino.0, true) {
                Err(Errno::EAGAIN) => {
                    drop(inner);
                    let mut w = self.inner.write().unwrap();
                    let (path, made, done) = tri!(w.copy_up_write(ino.0, Some((offset, data))), reply);
                    let file = match made {
                        Some(f) => f,
                        None => tri!(OpenOptions::new().read(true).write(true).open(&path).map_err(Errno::from), reply),
                    };
                    let file = self.fds.put(ino.0, file, true);
                    if !done {
                        tri!(file.write_all_at(data, offset).map_err(Errno::from), reply);
                    }
                    w.wrote(ino.0, offset + data.len() as u64);
                    reply.written(data.len() as u32);
                    return;
                }
                r => tri!(r, reply),
            };
            tri!(file.write_all_at(data, offset).map_err(Errno::from), reply);
        }
        self.inner.write().unwrap().wrote(ino.0, offset + data.len() as u64);
        reply.written(data.len() as u32);
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("flush").or_default() += 1;
        if self.cfg.no_open {
            // Every write already reached the server: the kernel stops sending flush.
            reply.error(Errno::ENOSYS);
            return;
        }
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("release").or_default() += 1;
        if let Some(h) = self.open.handles.lock().unwrap().remove(&fh.0) {
            if h.writer {
                let mut inner = self.inner.write().unwrap();
                inner.set_writers(ino.0, -1);
                inner.mark(ino.0);
            }
        }
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("fsync").or_default() += 1;
        let inner = self.inner.read().unwrap();
        // A patched file has no backing file: its bytes are in the journal.
        // A patched or new file without an upper file has its bytes in the journal; any other
        // file must sync its own data.
        let journaled = fh.0 == 0
            && inner.n(_ino.0).is_ok_and(|n| n.kind == Kind::File && n.upper.is_none() && (!n.patches.is_empty() || n.blob.is_none()));
        if !journaled {
            let file = tri!(self.backing(&inner, fh, _ino.0, false), reply);
            let r = if datasync { file.sync_data() } else { file.sync_all() };
            tri!(r.map_err(Errno::from), reply);
        }
        tri!(inner.sync(), reply);
        reply.ok();
    }

    fn forget(&self, _req: &Request, ino: INodeNo, _nlookup: u64) {
        // The kernel sends one forget when it drops a node: nothing holds it any more.
        if self.inner.write().unwrap().forget(ino.0) {
            self.fds.drop_ino(ino.0);
        }
    }

    fn opendir(&self, req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        check!(self, req, reply);
        // ENOSYS: the kernel opens folders itself from now on (FUSE_NO_OPENDIR_SUPPORT) and keeps
        // their listings (its defaults are FOPEN_KEEP_CACHE and FOPEN_CACHE_DIR), so a walk
        // costs no opendir and no releasedir round trip. `readdir` gets file handle 0.
        reply.error(Errno::ENOSYS);
    }

    fn readdir(&self, req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        check!(self, req, reply);
        self.counters.readdir.fetch_add(1, Ordering::Relaxed);
        self.trace.note(|t| {
            t.listed.insert(ino.0);
        });
        let inner = tri!(self.loaded_dir(ino.0), reply);
        let n = tri!(inner.n(ino.0), reply);
        let parent = if n.parent == 0 { ino.0 } else { n.parent };
        let mut list: Vec<(u64, FileType, Vec<u8>)> =
            vec![(ino.0, FileType::Directory, b".".to_vec()), (parent, FileType::Directory, b"..".to_vec())];
        for (name, c) in n.children.as_ref().unwrap() {
            let kind = match inner.nodes[*c as usize].kind {
                Kind::File => FileType::RegularFile,
                Kind::Dir => FileType::Directory,
                Kind::Symlink => FileType::Symlink,
            };
            list.push((*c, kind, name.clone()));
        }
        for (i, (c, kind, name)) in list.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(c), (i + 1) as u64, kind, OsStr::from_bytes(&name)) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        check!(self, req, reply);
        self.counters.readdir.fetch_add(1, Ordering::Relaxed);
        self.trace.note(|t| {
            t.listed.insert(ino.0);
        });
        let inner = tri!(self.loaded_dir(ino.0), reply);
        let n = tri!(inner.n(ino.0), reply);
        let parent = if n.parent == 0 { ino.0 } else { n.parent };
        let mut list: Vec<(u64, Vec<u8>)> = vec![(ino.0, b".".to_vec()), (parent, b"..".to_vec())];
        for (name, c) in n.children.as_ref().unwrap() {
            list.push((*c, name.clone()));
        }
        for (i, (c, name)) in list.into_iter().enumerate().skip(offset as usize) {
            let a = tri!(self.attr(&inner, c), reply);
            if reply.add(INodeNo(c), (i + 1) as u64, OsStr::from_bytes(&name), &self.ttl(), &a, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        // The entries of every folder of the view are lines of the journal.
        tri!(self.inner.read().unwrap().sync(), reply);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("statfs").or_default() += 1;
        let line = self.inner.read().unwrap().line.clone();
        let s = tri!(rustix::fs::statvfs(&line).map_err(|e| Errno::from_i32(e.raw_os_error())), reply);
        reply.statfs(s.f_blocks, s.f_bfree, s.f_bavail, s.f_files, s.f_ffree, s.f_bsize as u32, 255, s.f_frsize as u32);
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("getxattr").or_default() += 1;
        check!(self, req, reply);
        let inner = self.inner.read().unwrap();
        if inner.has_xattrs == Some(false) {
            // No file of the line has extended attributes yet: no lookup. Not ENOSYS, which would
            // make the kernel stop asking for good, also after a `setxattr`.
            reply.error(Errno::NO_XATTR);
            return;
        }
        let n = tri!(inner.n(ino.0), reply);
        match n.xattrs.iter().find(|(k, _)| k.as_slice() == name.as_bytes()) {
            None => reply.error(Errno::NO_XATTR),
            Some((_, v)) if size == 0 => reply.size(v.len() as u32),
            Some((_, v)) if v.len() > size as usize => reply.error(Errno::ERANGE),
            Some((_, v)) => reply.data(v),
        }
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("listxattr").or_default() += 1;
        check!(self, req, reply);
        let inner = self.inner.read().unwrap();
        let n = tri!(inner.n(ino.0), reply);
        let mut out = Vec::new();
        for (k, _) in &n.xattrs {
            out.extend_from_slice(k);
            out.push(0);
        }
        if size == 0 {
            reply.size(out.len() as u32);
        } else if out.len() > size as usize {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(&out);
        }
    }

    fn setxattr(
        &self,
        req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("setxattr").or_default() += 1;
        check!(self, req, reply);
        let mut inner = self.inner.write().unwrap();
        tri!(inner.set_xattr(ino.0, name.as_bytes(), Some(value), flags), reply);
        reply.ok();
    }

    fn removexattr(&self, req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("removexattr").or_default() += 1;
        check!(self, req, reply);
        let mut inner = self.inner.write().unwrap();
        tri!(inner.set_xattr(ino.0, name.as_bytes(), None, 0), reply);
        reply.ok();
    }

    fn access(&self, req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        self.counters.other.fetch_add(1, Ordering::Relaxed);
        *self.counters.ops.lock().unwrap().entry("access").or_default() += 1;
        check!(self, req, reply);
        reply.ok();
    }

    fn create(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, _flags: i32, reply: ReplyCreate) {
        check!(self, req, reply);
        let mut inner = self.inner.write().unwrap();
        let ino = tri!(inner.create(parent.0, name.as_bytes(), mode & !umask, None), reply);
        self.counters.create.fetch_add(1, Ordering::Relaxed);
        self.trace.note(|t| {
            t.wrote.insert(ino);
        });
        if self.cfg.no_open && inner.n(ino).is_ok_and(|n| n.upper.is_none()) {
            let a = tri!(self.attr(&inner, ino), reply);
            drop(inner);
            reply.created(&self.ttl(), &a, Generation(0), FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
            return;
        }
        let path = tri!(inner.read_path(ino), reply);
        let file = tri!(OpenOptions::new().read(true).write(true).open(&path).map_err(Errno::from), reply);
        if self.cfg.no_open {
            self.fds.put(ino, file, true);
            let a = tri!(self.attr(&inner, ino), reply);
            drop(inner);
            reply.created(&self.ttl(), &a, Generation(0), FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
            return;
        }
        inner.set_writers(ino, 1);
        let a = tri!(self.attr(&inner, ino), reply);
        drop(inner);
        let fh = self.open_handle(file, ino, true);
        reply.created(&self.ttl(), &a, Generation(0), FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layr_objects::Keying;

    /// A store with the state `s0`: `d/a` ("a0") and `d/b` ("b0"); the folder of a line.
    fn fixture() -> Result<(PathBuf, PathBuf)> {
        let tmp = std::env::temp_dir().join(format!("layr-fs-view-{}", crate::ids::new_id()));
        let objects = tmp.join("objects");
        let store = Store::open(&objects, Keying::Plain)?;
        let file = |bytes: &[u8], name: &[u8]| -> Result<Entry> {
            let (blob, size) = store.put_hot_bytes(bytes)?;
            Ok(Entry { name: name.to_vec(), mode: 0o644, node: TreeNode::File { size, blob }, xattrs: Vec::new() })
        };
        let d = store.put_tree(&Tree { entries: vec![file(b"a0", b"a")?, file(b"b0", b"b")?], links: Vec::new() })?;
        let dt = store.put_times(&Times { own: 1, records: vec![Record::Time(1), Record::Time(1)] })?;
        let root = Entry { name: b"d".to_vec(), mode: 0o755, node: TreeNode::Dir { tree: d }, xattrs: Vec::new() };
        let tree = store.put_tree(&Tree { entries: vec![root], links: Vec::new() })?;
        let times = store.put_times(&Times { own: 1, records: vec![Record::Folder(dt)] })?;
        store.set_state("s0", &StateRef { tree, times })?;
        Ok((tmp, objects))
    }

    /// The pool of upper files can still make a file while the folder goes: best effort.
    fn cleanup(tmp: &Path) {
        for _ in 0..20 {
            if fs::remove_dir_all(tmp).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn open(objects: &Path, line: &Path) -> Result<Inner> {
        Inner::open(Store::open(objects, Keying::Plain)?, line, "s0", Vec::new())
    }

    fn saved(inner: &mut Inner, name: &str) -> Result<StateRef> {
        let out: serde_json::Value = serde_json::from_str(&inner.snapshot()?)?;
        let r = StateRef { tree: Id::parse(out["tree"].as_str().unwrap())?, times: Id::parse(out["times"].as_str().unwrap())? };
        inner.store.set_state(name, &r)?;
        Ok(r)
    }

    fn file_of(store: &Store, r: &StateRef, name: &[u8]) -> Result<Vec<u8>> {
        let root = store.get_tree(&r.tree)?;
        let TreeNode::Dir { tree } = &root.entries[0].node else { unreachable!() };
        let d = store.get_tree(tree)?;
        let e = d.entries.iter().find(|e| e.name == name).unwrap();
        let TreeNode::File { blob, .. } = &e.node else { unreachable!() };
        store.read_blob(blob)
    }

    #[test]
    fn a_save_after_adopt_keeps_the_saved_content_of_a_sibling() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let mut inner = open(&objects, &tmp.join("line"))?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let (a, b) = (inner.child(d, b"a").unwrap().unwrap(), inner.child(d, b"b").unwrap().unwrap());
        inner.patch(a, 0, b"A1", None).unwrap();
        saved(&mut inner, "s1")?;
        assert!(inner.adopt("s1")?);
        inner.patch(b, 0, b"B1", None).unwrap();
        let r = saved(&mut inner, "s2")?;
        assert_eq!(file_of(&inner.store, &r, b"a")?, b"A1");
        assert_eq!(file_of(&inner.store, &r, b"b")?, b"B1");
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_truncate_after_adopt_reaches_the_next_save() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let mut inner = open(&objects, &tmp.join("line"))?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        inner.copy_up(a).unwrap();
        saved(&mut inner, "s1")?;
        assert!(inner.adopt("s1")?);
        inner.truncate(a, 0).unwrap();
        let r = saved(&mut inner, "s2")?;
        assert_eq!(file_of(&inner.store, &r, b"a")?, b"");
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn an_old_patch_far_past_the_end_replays_as_a_sparse_file() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let line = tmp.join("line");
        std::fs::create_dir_all(&line)?;
        // 1 GiB: far past the end of the file, and below the file size limit of a check (its memory
        // limit, 8 GiB): at 1 TiB the kernel stopped the whole test process (SIGXFSZ, 2026-11-22).
        let far = 1u64 << 30;
        std::fs::write(line.join("journal"), format!("B s0\nW {} {far} 5 {}\n", hex(b"d/a"), hex(b"Z")))?;
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        let n = inner.n(a).unwrap();
        assert!(n.upper.is_some() && n.patches.is_empty());
        assert_eq!(n.size, far + 1);
        let up = inner.read_path(a).unwrap();
        let mut b = [0u8; 2];
        File::open(&up)?.read_exact_at(&mut b[..1], far)?;
        File::open(&up)?.read_exact_at(&mut b[1..], 0)?;
        assert_eq!(&b, b"Za");
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_save_does_not_replace_an_existing_state() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let mut inner = open(&objects, &tmp.join("line"))?;
        let before = inner.store.state_ref("s0")?;
        assert!(inner.save("s0").is_err());
        assert_eq!(inner.store.state_ref("s0")?, before);
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_patch_ends_inside_a_small_file() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let mut inner = open(&objects, &tmp.join("line"))?;
        inner.lazy = true;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        assert!(inner.can_patch(a, 0, 2));
        assert!(!inner.can_patch(a, SMALL, 1), "a small write far past the end");
        assert!(!inner.can_patch(a, u64::MAX, 1));
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_torn_journal_line_goes_and_a_short_one_is_an_error() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let line = tmp.join("line");
        std::fs::create_dir_all(&line)?;
        let path = hex(b"d/a");
        let whole = format!("B s0\nW {path} 0 5 {}\n", hex(b"A1"));
        std::fs::write(line.join("journal"), format!("{whole}W {path} 0"))?;
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        assert_eq!(inner.content(a).unwrap(), b"A1");
        drop(inner);
        assert_eq!(std::fs::read_to_string(line.join("journal"))?.lines().take(2).collect::<Vec<_>>().join("\n") + "\n", whole);
        std::fs::write(line.join("journal"), format!("B s0\nW {path} 0\n"))?;
        assert!(open(&objects, &line).is_err());
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_write_after_a_replayed_far_patch_survives_the_next_replay() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let line = tmp.join("line");
        std::fs::create_dir_all(&line)?;
        // 1 GiB: far past the end of the file, and below the file size limit of a check (its memory
        // limit, 8 GiB): at 1 TiB the kernel stopped the whole test process (SIGXFSZ, 2026-11-22).
        let far = 1u64 << 30;
        std::fs::write(line.join("journal"), format!("B s0\nW {} {far} 5 {}\n", hex(b"d/a"), hex(b"Z")))?;
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        // A write through an open upper file reaches only the file, as in `write`.
        OpenOptions::new().write(true).open(inner.read_path(a).unwrap())?.write_all_at(b"Q", far)?;
        drop(inner);
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        let mut b = [0u8; 1];
        File::open(inner.read_path(a).unwrap())?.read_exact_at(&mut b, far)?;
        assert_eq!(&b, b"Q");
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_removed_file_keeps_its_upper_file_until_the_kernel_forgets_it() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let mut inner = open(&objects, &tmp.join("line"))?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let f = inner.create(d, b"new", 0o644, None).unwrap();
        let up = inner.read_path(f).unwrap();
        inner.remove(d, b"new", false).unwrap();
        assert!(up.exists(), "an open descriptor still reads it by its path");
        assert!(!inner.forget(d), "a node in the tree stays");
        assert!(inner.forget(f));
        assert!(!up.exists());
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_replay_keeps_the_time_of_a_later_write_to_an_upper_file() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let line = tmp.join("line");
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let f = inner.create(d, b"new", 0o644, None).unwrap();
        inner.set_mtime(f, 1_000_000_000).unwrap();
        let up = inner.read_path(f).unwrap();
        OpenOptions::new().write(true).open(&up)?.write_all_at(b"x", 0)?;
        let written = layr_objects::times::ns_of(&fs::metadata(&up)?);
        assert!(written > 1_000_000_000);
        drop(inner);
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let f = inner.child(d, b"new").unwrap().unwrap();
        assert_eq!(inner.n(f).unwrap().mtime, written);
        assert_eq!(layr_objects::times::ns_of(&fs::metadata(inner.read_path(f).unwrap())?), written);
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_write_after_adopt_and_save_survives_a_replay() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let line = tmp.join("line");
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        let up = inner.copy_up(a).unwrap();
        fs::write(&up, b"A1")?;
        saved(&mut inner, "s1")?;
        assert!(inner.adopt("s1")?);
        inner.save("s2")?;
        // The clean node keeps its upper file: the next write goes there.
        let up = inner.read_path(a).unwrap();
        OpenOptions::new().write(true).open(&up)?.write_all_at(b"A2", 0)?;
        drop(inner);
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        assert_eq!(inner.content(a).unwrap(), b"A2");
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_failed_journal_write_changes_nothing() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let line = tmp.join("line");
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        // A journal that takes no write.
        inner.journal = Some(File::open(line.join("journal"))?);
        assert!(inner.remove(d, b"a", false).is_err());
        assert_eq!(inner.child(d, b"a").unwrap(), Some(a));
        assert!(inner.rename(d, b"a", d, b"c", false).is_err());
        assert_eq!(inner.child(d, b"a").unwrap(), Some(a));
        assert_eq!(inner.child(d, b"c").unwrap(), None);
        assert!(inner.create(d, b"n", 0o644, None).is_err());
        assert!(inner.mkdir(d, b"m", 0o755).is_err());
        assert_eq!(inner.child(d, b"n").unwrap(), None);
        assert_eq!(inner.child(d, b"m").unwrap(), None);
        assert!(inner.copy_up(a).is_err());
        assert!(inner.n(a).unwrap().upper.is_none());
        assert!(inner.chmod(a, 0o600).is_err());
        assert_eq!(inner.n(a).unwrap().mode, 0o644);
        assert!(inner.set_xattr(a, b"user.t", Some(b"v"), 0).is_err());
        assert!(inner.n(a).unwrap().xattrs.is_empty());
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_rebase_keeps_a_folder_attribute_of_the_other_side() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let mut theirs = open(&objects, &tmp.join("theirs"))?;
        let d = theirs.child(ROOT, b"d").unwrap().unwrap();
        theirs.set_xattr(d, b"user.t", Some(b"v"), 0).unwrap();
        theirs.save("t1")?;
        drop(theirs);
        let mut inner = open(&objects, &tmp.join("line"))?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        inner.patch(a, 0, b"A1", None).unwrap();
        inner.rebase("t1", "m1")?;
        let r = inner.store.state_ref("m1")?.unwrap();
        assert_eq!(file_of(&inner.store, &r, b"a")?, b"A1");
        let root = inner.store.get_tree(&r.tree)?;
        assert_eq!(root.entries[0].xattrs, vec![(b"user.t".to_vec(), b"v".to_vec())]);
        // A rebase does not replace an existing state.
        assert!(inner.rebase("t1", "s0").is_err());
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_second_rebase_reports_the_same_conflict() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let mut theirs = open(&objects, &tmp.join("theirs"))?;
        let d = theirs.child(ROOT, b"d").unwrap().unwrap();
        let a = theirs.child(d, b"a").unwrap().unwrap();
        theirs.patch(a, 0, b"T1", None).unwrap();
        theirs.save("t1")?;
        drop(theirs);
        let mut inner = open(&objects, &tmp.join("line"))?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        inner.patch(a, 0, b"O1", None).unwrap();
        assert!(inner.rebase("t1", "r1").is_err());
        assert!(inner.rebase("t1", "r2").is_err(), "the saved changes are not the base");
        assert!(inner.store.state_ref("r2")?.is_none());
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_write_after_adopt_and_goto_survives_a_replay() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let line = tmp.join("line");
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        let up = inner.copy_up(a).unwrap();
        fs::write(&up, b"A1")?;
        saved(&mut inner, "s1")?;
        assert!(inner.adopt("s1")?);
        inner.goto("s1")?;
        let up = inner.read_path(a).unwrap();
        OpenOptions::new().write(true).open(&up)?.write_all_at(b"A2", 0)?;
        drop(inner);
        let mut inner = open(&objects, &line)?;
        let d = inner.child(ROOT, b"d").unwrap().unwrap();
        let a = inner.child(d, b"a").unwrap().unwrap();
        assert_eq!(inner.content(a).unwrap(), b"A2");
        drop(inner);
        cleanup(&tmp);
        Ok(())
    }

    #[test]
    fn a_new_state_name_is_taken_once() -> Result<()> {
        let (tmp, objects) = fixture()?;
        let store = Store::open(&objects, Keying::Plain)?;
        let r = store.state_ref("s0")?.unwrap();
        store.create_state("s9", &r)?;
        assert!(store.create_state("s9", &r).is_err());
        assert!(store.create_state("s0", &r).is_err());
        assert_eq!(store.state_ref("s9")?, Some(r));
        cleanup(&tmp);
        Ok(())
    }
}
