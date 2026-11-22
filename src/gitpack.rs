//! Git on the object store (docs/git-on-objects.md): the files of the project's git history live
//! once, in the object store (v2). The git store (`<project>/git`) keeps commits, trees, tags and
//! the few blobs whose git bytes are not the bytes of any file (line end filters, for example).
//!
//! - The map (`meta/gitpack/segments/*.map`) names the git blob id of each file content of the
//!   object store: id, size, the CRC-32 of its pack entry, and the object store blob (or the
//!   target of a symbolic link). `update` adds the contents of new states, newest state first and
//!   in path order inside a state, so a pack read touches neighbors that git reads next.
//! - On Linux a FUSE folder (`<project>/git/layr-v2`, `serve`) shows each map segment as one git
//!   pack (`pack/pack-<id>.pack` and its `.idx`). The index is made at mount; an entry is made
//!   when git reads its bytes: the blob as zlib "stored" blocks. The git store names the folder
//!   in `objects/info/alternates`, so the store and every line's `.git` folder read the files.
//! - `drop_git_blobs` then removes from the git store the blobs that the map serves.
//!
//! An entry is not compressed: a pack that git makes for a push must not reuse it (`push` makes a
//! compressed pack first).

// Without LayrFS only the map, `uses_folder` and the stubs are used.
#![cfg_attr(not(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos")), allow(dead_code))]

use crate::store::Project;
use anyhow::{bail, ensure, Result};
use layr_objects::{Id, Node, Store};
use std::collections::HashSet;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// The folder of the map of a project.
pub fn dir(p: &Project) -> PathBuf {
    p.meta_dir().join("gitpack")
}

fn segments_dir(root: &Path) -> PathBuf {
    root.join("segments")
}

/// The FUSE folder of a git store: the git store names it in `objects/info/alternates`.
pub fn mount_dir(store: &Path) -> PathBuf {
    store.join("layr-v2")
}

/// Whether the project has a map with at least one segment.
pub fn has_map(p: &Project) -> bool {
    std::fs::read_dir(segments_dir(&dir(p))).is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".map"))))
}

// ---------------------------------------------------------------- map entries

/// Where the bytes of a map entry come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Src {
    Blob(Id),
    Link(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Ent {
    pub sha: [u8; 20],
    pub size: u64,
    /// CRC-32 of the whole pack entry: the pack index keeps it, and `pack-objects` checks it
    /// before it reuses the entry.
    pub crc: u32,
    pub src: Src,
}

const SEGMENT_MAGIC: &[u8; 8] = b"LGPSEG01";
const BLOCK: usize = 65535;

/// The object header of a pack entry: type 3 (blob) and the size.
fn entry_header(size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    let mut c = (3u8 << 4) | (size & 15) as u8;
    let mut s = size >> 4;
    while s != 0 {
        out.push(c | 0x80);
        c = (s & 0x7f) as u8;
        s >>= 7;
    }
    out.push(c);
    out
}

/// The length of `n` bytes as zlib "stored" blocks.
fn zlen(n: u64) -> u64 {
    2 + n + 5 * n.div_ceil(BLOCK as u64).max(1) + 4
}

pub fn entry_len(size: u64) -> u64 {
    entry_header(size).len() as u64 + zlen(size)
}

/// A pack entry: the header, then the content as zlib "stored" blocks (no compression).
pub fn entry(data: &[u8]) -> Vec<u8> {
    let size = data.len() as u64;
    let mut out = entry_header(size);
    out.reserve(zlen(size) as usize);
    out.extend_from_slice(&[0x78, 0x01]);
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let mut chunks = data.chunks(BLOCK).peekable();
    while let Some(c) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = c.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for c in data.chunks(5552) {
        for &x in c {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn ent(data: &[u8], src: Src) -> Ent {
    Ent { sha: layr_hashers::git_blob_id(data), size: data.len() as u64, crc: crc32fast::hash(&entry(data)), src }
}

fn write_segment(path: &Path, ents: &[Ent]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", crate::ids::new_id()));
    let r = (|| -> Result<()> {
        let mut w = BufWriter::new(std::fs::File::create(&tmp)?);
        w.write_all(SEGMENT_MAGIC)?;
        w.write_all(&(ents.len() as u64).to_le_bytes())?;
        for e in ents {
            w.write_all(&e.sha)?;
            w.write_all(&e.size.to_le_bytes())?;
            w.write_all(&e.crc.to_le_bytes())?;
            match &e.src {
                Src::Blob(id) => {
                    w.write_all(&[0])?;
                    w.write_all(&id.0)?;
                }
                Src::Link(l) => {
                    w.write_all(&[1])?;
                    w.write_all(&(l.len() as u32).to_le_bytes())?;
                    w.write_all(l)?;
                }
            }
        }
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

pub fn read_segment(path: &Path) -> Result<Vec<Ent>> {
    let mut r = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    ensure!(&magic == SEGMENT_MAGIC, "{} is not a git map segment", path.display());
    let (mut u8s, mut u4s) = ([0u8; 8], [0u8; 4]);
    r.read_exact(&mut u8s)?;
    let n = u64::from_le_bytes(u8s) as usize;
    let mut ents = Vec::with_capacity(n.min(1 << 24));
    for _ in 0..n {
        let mut sha = [0u8; 20];
        r.read_exact(&mut sha)?;
        r.read_exact(&mut u8s)?;
        let size = u64::from_le_bytes(u8s);
        r.read_exact(&mut u4s)?;
        let crc = u32::from_le_bytes(u4s);
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        let src = match tag[0] {
            0 => {
                let mut id = [0u8; 32];
                r.read_exact(&mut id)?;
                Src::Blob(Id(id))
            }
            1 => {
                r.read_exact(&mut u4s)?;
                let len = u32::from_le_bytes(u4s) as usize;
                ensure!(len < 1 << 20, "a link target in {} is too long", path.display());
                let mut t = vec![0u8; len];
                r.read_exact(&mut t)?;
                Src::Link(t)
            }
            t => bail!("{}: unknown entry kind {t}", path.display()),
        };
        ents.push(Ent { sha, size, crc, src });
    }
    Ok(ents)
}

/// The segment files of a map folder, oldest first.
pub fn segment_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(segments_dir(root)) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "map")).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    v.sort();
    Ok(v)
}

/// Every entry of the map.
pub fn load_map(root: &Path) -> Result<Vec<Ent>> {
    let mut all = Vec::new();
    for p in segment_paths(root)? {
        all.extend(read_segment(&p)?);
    }
    Ok(all)
}

/// The object store blobs that the map names: they stay through a prune of the object store,
/// because the git store may hold no other copy.
pub fn mapped_blobs(p: &Project) -> Result<HashSet<Id>> {
    Ok(load_map(&dir(p))?
        .into_iter()
        .filter_map(|e| match e.src {
            Src::Blob(id) => Some(id),
            Src::Link(_) => None,
        })
        .collect())
}

// ---------------------------------------------------------------- update

fn walk(
    store: &Store,
    tree: &Id,
    walked: &mut HashSet<Id>,
    known: &mut HashSet<Vec<u8>>,
    order: &mut Vec<(Src, u64)>,
    new_trees: &mut Vec<Id>,
) -> Result<()> {
    if !walked.insert(*tree) {
        return Ok(());
    }
    new_trees.push(*tree);
    for e in store.get_tree(tree)?.entries {
        match e.node {
            Node::File { size, blob } => {
                if known.insert(blob.0.to_vec()) {
                    order.push((Src::Blob(blob), size));
                }
            }
            Node::Dir { tree } => walk(store, &tree, walked, known, order, new_trees)?,
            Node::Symlink { target } => {
                let mut key = b"link:".to_vec();
                key.extend_from_slice(&target);
                if known.insert(key) {
                    order.push((Src::Link(target.clone()), target.len() as u64));
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct UpdateReport {
    pub objects: usize,
    pub bytes: u64,
}

/// Add the file contents of the states of the object store that the map does not name yet: one
/// new segment. The trees walked once are not walked again (`meta/gitpack/trees`).
pub fn update(p: &Project) -> Result<UpdateReport> {
    if !crate::objstore::exists(p) {
        return Ok(UpdateReport::default());
    }
    let root = dir(p);
    std::fs::create_dir_all(segments_dir(&root))?;
    let store = crate::objstore::open_existing(p)?;
    let trees_file = root.join("trees");
    let mut walked: HashSet<Id> = std::fs::read(&trees_file)
        .unwrap_or_default()
        .chunks_exact(32)
        .map(|c| Id(c.try_into().expect("32 bytes")))
        .collect();
    let map = load_map(&root)?;
    let mut shas: HashSet<[u8; 20]> = map.iter().map(|e| e.sha).collect();
    let mut known: HashSet<Vec<u8>> = map
        .iter()
        .map(|e| match &e.src {
            Src::Blob(id) => id.0.to_vec(),
            Src::Link(t) => [b"link:".as_slice(), t].concat(),
        })
        .collect();
    drop(map);
    // Newest state first: its files are the ones git reads most.
    let mut states: Vec<(i64, String, layr_objects::StateRef)> = Vec::new();
    for id in p.db.kept_state_ids()? {
        let Some(r) = store.state_ref(&id)? else { continue };
        if !store.has(&r.tree) {
            continue;
        }
        let time = p.db.state(&id)?.map(|s| s.time).unwrap_or(0);
        states.push((time, id, r));
    }
    states.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let (mut order, mut new_trees) = (Vec::new(), Vec::new());
    for (_, _, r) in &states {
        walk(&store, &r.tree, &mut walked, &mut known, &mut order, &mut new_trees)?;
    }
    use rayon::prelude::*;
    let ents: Vec<Ent> = order
        .into_par_iter()
        .map(|(src, size)| -> Result<Ent> {
            let data = match &src {
                Src::Blob(id) => store.read_blob(id)?,
                Src::Link(l) => l.clone(),
            };
            ensure!(data.len() as u64 == size, "an object store file has the wrong size");
            Ok(ent(&data, src))
        })
        .collect::<Result<_>>()?;
    // The same bytes as a file and as a link target are one git object: the first one stays.
    let ents: Vec<Ent> = ents.into_iter().filter(|e| shas.insert(e.sha)).collect();
    let report = UpdateReport { objects: ents.len(), bytes: ents.iter().map(|e| e.size).sum() };
    if !ents.is_empty() {
        let name = format!("seg-{:016x}-{}.map", crate::ids::now_ms(), &crate::ids::new_id()[..8]);
        write_segment(&segments_dir(&root).join(name), &ents)?;
    }
    if !new_trees.is_empty() {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&trees_file)?;
        let mut buf = Vec::with_capacity(new_trees.len() * 32);
        for t in &new_trees {
            buf.extend_from_slice(&t.0);
        }
        f.write_all(&buf)?;
    }
    Ok(report)
}

// ---------------------------------------------------------------- the pack of a segment

/// One map segment as a git pack: the offset of each entry (pack order), its index, and its name.
pub struct Pack {
    pub ents: Vec<Ent>,
    /// The pack offset of each entry; one more for the trailer.
    pub offsets: Vec<u64>,
    pub idx: Vec<u8>,
    pub head: [u8; 12],
    pub trailer: [u8; 20],
    pub name: String,
}

impl Pack {
    pub fn new(ents: Vec<Ent>) -> Pack {
        let mut offsets = Vec::with_capacity(ents.len() + 1);
        let mut at = 12u64;
        for e in &ents {
            offsets.push(at);
            at += entry_len(e.size);
        }
        offsets.push(at);
        let (idx, trailer) = index(&ents, &offsets[..ents.len()]);
        let mut head = [0u8; 12];
        head[..4].copy_from_slice(b"PACK");
        head[4..8].copy_from_slice(&2u32.to_be_bytes());
        head[8..].copy_from_slice(&(ents.len() as u32).to_be_bytes());
        Pack { ents, offsets, idx, head, trailer, name: format!("pack-{}", hex::encode(trailer)) }
    }

    pub fn len(&self) -> u64 {
        self.offsets[self.ents.len()] + 20
    }

    /// Bytes `[at, at + len)` of the pack; `entry_bytes` gives the bytes of entry `i`.
    pub fn read(&self, at: u64, len: u64, entry_bytes: &dyn Fn(usize) -> Result<std::sync::Arc<Vec<u8>>>) -> Result<Vec<u8>> {
        let end = (at.saturating_add(len)).min(self.len());
        let mut out = Vec::with_capacity(end.saturating_sub(at) as usize);
        let mut pos = at;
        let tail = self.offsets[self.ents.len()];
        while pos < end {
            if pos < 12 {
                let e = 12.min(end);
                out.extend_from_slice(&self.head[pos as usize..e as usize]);
                pos = e;
                continue;
            }
            if pos >= tail {
                out.extend_from_slice(&self.trailer[(pos - tail) as usize..(end - tail) as usize]);
                break;
            }
            let i = self.offsets.partition_point(|&o| o <= pos) - 1;
            let b = entry_bytes(i)?;
            ensure!(b.len() as u64 == self.offsets[i + 1] - self.offsets[i], "pack entry {i} has the wrong length");
            let s = (pos - self.offsets[i]) as usize;
            let e = ((end - self.offsets[i]) as usize).min(b.len());
            out.extend_from_slice(&b[s..e]);
            pos = self.offsets[i] + e as u64;
        }
        Ok(out)
    }
}

/// The pack index (version 2) of `ents` at `offsets` (pack order), listed by id, and the pack
/// checksum that both files name. git checks only that the two name the same checksum.
pub fn index(ents: &[Ent], offsets: &[u64]) -> (Vec<u8>, [u8; 20]) {
    use layr_hashers::Digest;
    let mut by_id: Vec<usize> = (0..ents.len()).collect();
    by_id.sort_by(|&x, &y| ents[x].sha.cmp(&ents[y].sha));
    let mut out = vec![0xff, 0x74, 0x4f, 0x63, 0, 0, 0, 2];
    for b in 0..256usize {
        let n = by_id.partition_point(|&i| (ents[i].sha[0] as usize) <= b);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
    for &i in &by_id {
        out.extend_from_slice(&ents[i].sha);
    }
    for &i in &by_id {
        out.extend_from_slice(&ents[i].crc.to_be_bytes());
    }
    let mut large = Vec::new();
    for o in by_id.iter().map(|&i| offsets[i]) {
        if o < 0x8000_0000 {
            out.extend_from_slice(&(o as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&(0x8000_0000u32 | large.len() as u32).to_be_bytes());
            large.push(o);
        }
    }
    for o in large {
        out.extend_from_slice(&o.to_be_bytes());
    }
    let sum = |b: &[u8]| -> [u8; 20] {
        let mut h = layr_hashers::GitCompatible::new();
        h.update(b);
        h.finalize().into()
    };
    let pack_sum = sum(&out);
    out.extend_from_slice(&pack_sum);
    let idx_sum = sum(&out);
    out.extend_from_slice(&idx_sum);
    (out, pack_sum)
}

/// The bytes of a map entry from the object store.
pub fn entry_of(store: &Store, e: &Ent) -> Result<Vec<u8>> {
    let data = match &e.src {
        Src::Link(l) => l.clone(),
        Src::Blob(id) => store.read_blob(id)?,
    };
    ensure!(data.len() as u64 == e.size, "map entry {} has the wrong size", hex::encode(e.sha));
    Ok(entry(&data))
}

// ---------------------------------------------------------------- the mount (Linux)

#[cfg(all(target_os = "linux", feature = "layrfs-fuse"))]
pub use mount::{ensure_mounted, is_mounted, run, unmount};
#[cfg(target_os = "macos")]
pub use fsk::{ensure_mounted, is_mounted, run, unmount};
#[cfg(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos"))]
pub use ops::maintain;

#[cfg(not(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos")))]
pub fn is_mounted(_store: &Path) -> bool {
    false
}

#[cfg(not(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos")))]
pub fn unmount(_store: &Path) {}

/// Without a folder that serves the blobs the git store keeps every blob: nothing to do.
#[cfg(not(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos")))]
pub fn maintain(_ctx: &crate::ctx::Ctx, _repo: &crate::repo::Repo) -> Result<Option<String>> {
    Ok(None)
}

/// The git store `s` reads blobs from the FUSE folder: mount it when it does not answer (once per
/// thread and store; a failure shows in the git command that needs a blob).
pub fn serve_if_used(p: &Project, s: &Path) {
    thread_local! {
        static CHECKED: std::cell::RefCell<HashSet<PathBuf>> = std::cell::RefCell::new(HashSet::new());
    }
    if !CHECKED.with(|c| c.borrow_mut().insert(s.to_path_buf())) || !uses_folder(s) || is_mounted(s) {
        return;
    }
    #[cfg(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos"))]
    if let Err(e) = ensure_mounted(p) {
        eprintln!("warning: the git blobs of the object store are not mounted: {e:#}");
    }
    #[cfg(not(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos")))]
    let _ = p;
}

/// Whether the git store `s` reads blobs from the FUSE folder (its alternates name it).
pub fn uses_folder(s: &Path) -> bool {
    let want = mount_dir(s);
    std::fs::read_to_string(s.join("objects/info/alternates")).is_ok_and(|t| t.lines().any(|l| Path::new(l.trim()) == want))
}

#[cfg(all(target_os = "linux", feature = "layrfs-fuse"))]
mod mount {
    use super::*;
    use anyhow::Context;
    use fuser::{
        Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, KernelConfig, LockOwner, MountOption,
        OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request, SessionACL, TimeOrNow,
    };
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const USAGE: &str = "usage: layr fs gitpack serve <project folder>   (the git blobs of a project from its object store)";

    /// `layr fs gitpack ...`.
    pub fn run(args: &[String]) -> Result<i32> {
        match args {
            [cmd, project] if cmd == "serve" => {
                serve(Path::new(project))?;
                Ok(0)
            }
            _ => bail!("{USAGE}"),
        }
    }

    const TTL: Duration = Duration::from_secs(3600);
    const SHORT: Duration = Duration::from_secs(1);
    const ROOT: u64 = 1;
    const PACKDIR: u64 = 2;
    /// Segment `k` has the index at `FIRST + 2k` and the pack at `FIRST + 2k + 1`.
    const FIRST: u64 = 16;
    const CACHE_BYTES: u64 = 512 << 20;
    const FUSE_MAGIC: i64 = 0x6573_5546;

    struct Packs {
        /// Every pack loaded since the start: an open file of a removed segment still reads.
        all: Vec<Arc<Pack>>,
        /// The segment file of each pack in `all`, and whether it is still in the folder.
        files: Vec<(PathBuf, bool)>,
        scanned: Option<Instant>,
    }

    struct Fs {
        map_root: PathBuf,
        store: Store,
        packs: RwLock<Packs>,
        made: Mutex<(HashMap<(usize, usize), Arc<Vec<u8>>>, u64)>,
        uid: u32,
        gid: u32,
        start: SystemTime,
    }

    impl Fs {
        fn rescan(&self, force: bool) {
            {
                let p = self.packs.read().unwrap();
                if !force && p.scanned.is_some_and(|t| t.elapsed() < SHORT) {
                    return;
                }
            }
            let Ok(paths) = segment_paths(&self.map_root) else { return };
            let mut p = self.packs.write().unwrap();
            for f in p.files.iter_mut() {
                f.1 = paths.contains(&f.0);
            }
            for path in paths {
                if p.files.iter().any(|f| f.0 == path) {
                    continue;
                }
                match read_segment(&path) {
                    Ok(ents) => {
                        p.all.push(Arc::new(Pack::new(ents)));
                        p.files.push((path, true));
                    }
                    Err(e) => eprintln!("layr gitpack: {}: {e:#}", path.display()),
                }
            }
            p.scanned = Some(Instant::now());
        }

        fn pack(&self, k: usize) -> Option<Arc<Pack>> {
            self.packs.read().unwrap().all.get(k).cloned()
        }

        fn attr(&self, ino: u64) -> Option<FileAttr> {
            let (kind, size) = match ino {
                ROOT | PACKDIR => (FileType::Directory, 0),
                _ if ino >= FIRST => {
                    let k = ((ino - FIRST) / 2) as usize;
                    let p = self.pack(k)?;
                    (FileType::RegularFile, if (ino - FIRST) % 2 == 0 { p.idx.len() as u64 } else { p.len() })
                }
                _ => return None,
            };
            Some(FileAttr {
                ino: INodeNo(ino),
                size,
                blocks: size.div_ceil(512),
                atime: self.start,
                mtime: self.start,
                ctime: self.start,
                crtime: UNIX_EPOCH,
                kind,
                // Files are writable for the time change of git's "freshen" (`setattr`) only: the
                // view refuses every write.
                perm: if kind == FileType::Directory { 0o555 } else { 0o666 },
                nlink: if kind == FileType::Directory { 2 } else { 1 },
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                flags: 0,
                blksize: 4096,
                st_ino: 0,
            })
        }

        fn entry_bytes(&self, k: usize, pack: &Pack, i: usize) -> Result<Arc<Vec<u8>>> {
            if let Some(b) = self.made.lock().unwrap().0.get(&(k, i)) {
                return Ok(b.clone());
            }
            let b = Arc::new(entry_of(&self.store, &pack.ents[i])?);
            let mut m = self.made.lock().unwrap();
            if m.1 > CACHE_BYTES {
                *m = (HashMap::new(), 0);
            }
            m.1 += b.len() as u64;
            m.0.insert((k, i), b.clone());
            Ok(b)
        }
    }

    impl Filesystem for Fs {
        fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
            // Each pack holds its entries in path order: the bytes after a read are likely next.
            let _ = config.set_max_readahead(128 << 10);
            Ok(())
        }

        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            let name = name.to_str().unwrap_or("");
            let found = match (u64::from(parent), name) {
                (ROOT, "pack") => Some(PACKDIR),
                (PACKDIR, n) => {
                    self.rescan(false);
                    let p = self.packs.read().unwrap();
                    p.all.iter().zip(&p.files).enumerate().find_map(|(k, (pk, f))| {
                        if !f.1 {
                            return None;
                        }
                        if n == format!("{}.idx", pk.name) {
                            Some(FIRST + 2 * k as u64)
                        } else if n == format!("{}.pack", pk.name) {
                            Some(FIRST + 2 * k as u64 + 1)
                        } else {
                            None
                        }
                    })
                }
                _ => None,
            };
            match found.and_then(|ino| self.attr(ino)) {
                Some(a) => reply.entry(if a.kind == FileType::Directory { &SHORT } else { &TTL }, &a, fuser::Generation(0)),
                None => reply.error(Errno::ENOENT),
            }
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
            match self.attr(u64::from(ino)) {
                Some(a) => reply.attr(&TTL, &a),
                None => reply.error(Errno::ENOENT),
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn setattr(
            &self,
            _req: &Request,
            ino: INodeNo,
            mode: Option<u32>,
            uid: Option<u32>,
            gid: Option<u32>,
            size: Option<u64>,
            _atime: Option<TimeOrNow>,
            _mtime: Option<TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<FileHandle>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<fuser::BsdFileFlags>,
            reply: ReplyAttr,
        ) {
            // git touches a pack before it skips the write of an object that the pack holds; a
            // refused touch makes it write a loose copy. The times stay as they are.
            if mode.is_some() || uid.is_some() || gid.is_some() || size.is_some() {
                return reply.error(Errno::EPERM);
            }
            match self.attr(u64::from(ino)) {
                Some(a) => reply.attr(&TTL, &a),
                None => reply.error(Errno::ENOENT),
            }
        }

        fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
            if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
                return reply.error(Errno::EROFS);
            }
            match self.attr(u64::from(ino)) {
                Some(a) if a.kind == FileType::RegularFile => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
                Some(_) => reply.error(Errno::EISDIR),
                None => reply.error(Errno::ENOENT),
            }
        }

        fn read(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            offset: u64,
            size: u32,
            _flags: OpenFlags,
            _lock: Option<LockOwner>,
            reply: ReplyData,
        ) {
            let ino = u64::from(ino);
            if ino < FIRST {
                return reply.error(Errno::EISDIR);
            }
            let k = ((ino - FIRST) / 2) as usize;
            let Some(pack) = self.pack(k) else { return reply.error(Errno::ENOENT) };
            if (ino - FIRST) % 2 == 0 {
                let s = (offset as usize).min(pack.idx.len());
                let e = (s + size as usize).min(pack.idx.len());
                return reply.data(&pack.idx[s..e]);
            }
            match pack.read(offset, size as u64, &|i| self.entry_bytes(k, &pack, i)) {
                Ok(b) => reply.data(&b),
                Err(e) => {
                    eprintln!("layr gitpack: read {} at {offset}: {e:#}", pack.name);
                    reply.error(Errno::EIO)
                }
            }
        }

        fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
            reply.opened(FileHandle(0), FopenFlags::empty())
        }

        fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
            let p = u64::from(ino);
            let mut list: Vec<(u64, FileType, String)> = vec![(p, FileType::Directory, ".".into()), (ROOT, FileType::Directory, "..".into())];
            match p {
                ROOT => list.push((PACKDIR, FileType::Directory, "pack".into())),
                PACKDIR => {
                    self.rescan(offset == 0);
                    let packs = self.packs.read().unwrap();
                    for (k, (pk, f)) in packs.all.iter().zip(&packs.files).enumerate() {
                        if f.1 {
                            list.push((FIRST + 2 * k as u64, FileType::RegularFile, format!("{}.idx", pk.name)));
                            list.push((FIRST + 2 * k as u64 + 1, FileType::RegularFile, format!("{}.pack", pk.name)));
                        }
                    }
                }
                _ => return reply.error(Errno::ENOTDIR),
            }
            for (n, (ino, kind, name)) in list.into_iter().enumerate().skip(offset as usize) {
                if reply.add(INodeNo(ino), (n + 1) as u64, kind, name) {
                    break;
                }
            }
            reply.ok()
        }
    }

    /// Serve the FUSE folder of the project at `project` until it is unmounted.
    fn serve(project: &Path) -> Result<()> {
        let store_dir = project.join("git");
        let mnt = mount_dir(&store_dir);
        clear_dead(&mnt);
        std::fs::create_dir_all(&mnt)?;
        let m = std::fs::metadata(&store_dir)?;
        use std::os::unix::fs::MetadataExt;
        let fs = Fs {
            map_root: project.join("meta/gitpack"),
            // A long-lived reader without the prune lease, as the server of a fast line: a
            // prune keeps every blob of the map (`objstore::maintain`).
            store: Store::open_existing_unleased(&project.join("meta/objects"), layr_objects::Keying::Plain)?,
            packs: RwLock::new(Packs { all: Vec::new(), files: Vec::new(), scanned: None }),
            made: Mutex::new((HashMap::new(), 0)),
            uid: m.uid(),
            gid: m.gid(),
            start: SystemTime::now(),
        };
        fs.rescan(true);
        let mut cfg = Config::default();
        cfg.mount_options.extend([MountOption::FSName("layr-gitpack".into()), MountOption::DefaultPermissions]);
        // Every reader of the git store (an access list decides who enters it) reads the folder.
        cfg.acl = SessionACL::All;
        cfg.n_threads = Some(8);
        cfg.clone_fd = true;
        fuser::mount(fs, &mnt, &cfg)?;
        Ok(())
    }

    /// True when the FUSE folder of the git store `s` answers.
    pub fn is_mounted(s: &Path) -> bool {
        rustix::fs::statfs(&mount_dir(s)).is_ok_and(|st| st.f_type as i64 == FUSE_MAGIC)
    }

    /// A server that died leaves its mount behind (`statfs` gives ENOTCONN): remove it.
    fn clear_dead(mnt: &Path) {
        if let Err(e) = rustix::fs::statfs(mnt) {
            if e == rustix::io::Errno::NOTCONN {
                let _ = Command::new("fusermount3").arg("-u").arg("-z").arg(mnt).stderr(Stdio::null()).status();
                let _ = Command::new("umount").arg("-l").arg(mnt).stderr(Stdio::null()).status();
            }
        }
    }

    /// Unmount the FUSE folder of the git store `s` (before the store moves).
    pub fn unmount(s: &Path) {
        let mnt = mount_dir(s);
        if is_mounted(s) {
            let ok = Command::new("fusermount3").arg("-u").arg(&mnt).stderr(Stdio::null()).status().is_ok_and(|x| x.success());
            if !ok {
                let _ = Command::new("umount").arg("-l").arg(&mnt).stderr(Stdio::null()).status();
            }
        }
        clear_dead(&mnt);
        // The server ends when its mount goes: wait for it, so that nothing of the project stays open.
        let Some(project) = s.parent() else { return };
        let mut want = b"fs\0gitpack\0serve\0".to_vec();
        want.extend_from_slice(project.as_os_str().as_encoded_bytes());
        want.push(0);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let running = std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
                std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c.windows(want.len()).any(|w| w == want))
            });
            if !running {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Mount the FUSE folder of the project when it has a map and the folder does not answer.
    /// True when it answers.
    pub fn ensure_mounted(p: &Project) -> Result<bool> {
        let s = p.git_dir();
        if is_mounted(&s) {
            return Ok(true);
        }
        if !has_map(p) || !s.join("objects").is_dir() {
            return Ok(false);
        }
        clear_dead(&mount_dir(&s));
        let exe = std::env::current_exe()?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(dir(p).join("server.log"))?;
        let mut c = Command::new(exe);
        c.args(["fs", "gitpack", "serve"]).arg(&p.dir).current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
        crate::sys::new_session(&mut c);
        let mut child = c.spawn().context("start the git pack server")?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if is_mounted(&s) {
                // The server runs on alone; the service reaps it.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(true);
            }
            if let Ok(Some(status)) = child.try_wait() {
                bail!("the git pack server stopped ({status}); see {}", dir(p).join("server.log").display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        bail!("the git pack server did not mount {}", mount_dir(&s).display())
    }

}

/// macOS: the LayrFS extension (FSKit) shows the packs; it reads their bytes from a pack server,
/// `layr fs gitpack serve`. The sandbox of the extension refuses Unix sockets, so each connection
/// is a pair of named pipes (tools/layrfs/LayrFS/GitPackVolume.swift has the requests): the
/// extension makes `<id>.req` and `<id>.resp` in the folder of the server and writes `<id>` to its
/// pipe `ctl`; the server opens the pair, answers "K", then serves requests until the pair closes.
#[cfg(target_os = "macos")]
// `mkfifo` (rustix has no `mkfifoat` on macOS).
#[allow(unsafe_code)]
mod fsk {
    use super::*;
    use anyhow::Context;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, BufWriter, Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant};

    const USAGE: &str = "usage: layr fs gitpack serve <project folder>   (the git blobs of a project from its object store)";
    const CACHE_BYTES: u64 = 512 << 20;
    /// The most bytes one read request gets.
    const MAX_READ: u64 = 8 << 20;

    /// `layr fs gitpack ...`.
    pub fn run(args: &[String]) -> Result<i32> {
        match args {
            [cmd, project] if cmd == "serve" => {
                serve(Path::new(project))?;
                Ok(0)
            }
            _ => bail!("{USAGE}"),
        }
    }

    /// The folder of the pipes of the pack server of a project.
    fn pipes(project: &Path) -> PathBuf {
        project.join("meta/gitpack/pipes")
    }

    /// The folder that the extension mounts: it holds only the settings file.
    fn src_dir(project: &Path) -> PathBuf {
        project.join("meta/gitpack/fsk")
    }

    struct Server {
        map_root: PathBuf,
        store: Store,
        /// Every pack loaded since the start, by name; the segment file of each loaded pack.
        packs: RwLock<(HashMap<String, Arc<Pack>>, Vec<(PathBuf, String)>)>,
        made: Mutex<(HashMap<(String, usize), Arc<Vec<u8>>>, u64)>,
    }

    impl Server {
        /// The packs of the segments in the folder now, oldest first.
        fn current(&self) -> Result<Vec<Arc<Pack>>> {
            let paths = segment_paths(&self.map_root)?;
            let mut out = Vec::new();
            for path in paths {
                let known = self.packs.read().unwrap().1.iter().find(|(p, _)| *p == path).map(|(_, n)| n.clone());
                let name = match known {
                    Some(n) => n,
                    None => {
                        let pack = Arc::new(Pack::new(read_segment(&path)?));
                        let name = pack.name.clone();
                        let mut w = self.packs.write().unwrap();
                        w.0.insert(name.clone(), pack);
                        w.1.push((path, name.clone()));
                        name
                    }
                };
                if let Some(p) = self.packs.read().unwrap().0.get(&name) {
                    out.push(p.clone());
                }
            }
            Ok(out)
        }

        fn entry_bytes(&self, pack: &Pack, i: usize) -> Result<Arc<Vec<u8>>> {
            let key = (pack.name.clone(), i);
            if let Some(b) = self.made.lock().unwrap().0.get(&key) {
                return Ok(b.clone());
            }
            let b = Arc::new(entry_of(&self.store, &pack.ents[i])?);
            let mut m = self.made.lock().unwrap();
            if m.1 > CACHE_BYTES {
                *m = (HashMap::new(), 0);
            }
            m.1 += b.len() as u64;
            m.0.insert(key, b.clone());
            Ok(b)
        }

        /// Answers the requests of one connection until it closes.
        fn answer(&self, r: impl Read, w: impl Write) -> Result<()> {
            let mut r = BufReader::new(r);
            let mut w = BufWriter::new(w);
            w.write_all(b"K")?;
            w.flush()?;
            loop {
                let mut op = [0u8; 1];
                if r.read_exact(&mut op).is_err() {
                    return Ok(());
                }
                match op[0] {
                    b'L' => {
                        let packs = self.current()?;
                        w.write_all(&(packs.len() as u32).to_le_bytes())?;
                        for p in packs {
                            w.write_all(&(p.name.len() as u16).to_le_bytes())?;
                            w.write_all(p.name.as_bytes())?;
                            w.write_all(&(p.idx.len() as u64).to_le_bytes())?;
                            w.write_all(&p.len().to_le_bytes())?;
                        }
                    }
                    b'R' => {
                        let mut h = [0u8; 3];
                        r.read_exact(&mut h)?;
                        let mut name = vec![0u8; u16::from_le_bytes([h[1], h[2]]) as usize];
                        r.read_exact(&mut name)?;
                        let mut tail = [0u8; 12];
                        r.read_exact(&mut tail)?;
                        let offset = u64::from_le_bytes(tail[..8].try_into().unwrap());
                        let len = (u32::from_le_bytes(tail[8..].try_into().unwrap()) as u64).min(MAX_READ);
                        let name = String::from_utf8_lossy(&name).into_owned();
                        let pack = self.packs.read().unwrap().0.get(&name).cloned();
                        let got: Result<Vec<u8>, i32> = match pack {
                            None => Err(libc::ENOENT),
                            Some(p) if h[0] == 0 => {
                                let s = (offset as usize).min(p.idx.len());
                                let e = (s + len as usize).min(p.idx.len());
                                Ok(p.idx[s..e].to_vec())
                            }
                            Some(p) => p.read(offset, len, &|i| self.entry_bytes(&p, i)).map_err(|e| {
                                eprintln!("layr gitpack: read {name} at {offset}: {e:#}");
                                libc::EIO
                            }),
                        };
                        match got {
                            Ok(b) => {
                                w.write_all(&[0])?;
                                w.write_all(&(b.len() as u32).to_le_bytes())?;
                                w.write_all(&b)?;
                            }
                            Err(code) => {
                                w.write_all(&[code as u8])?;
                                w.write_all(&0u32.to_le_bytes())?;
                            }
                        }
                    }
                    _ => return Ok(()),
                }
                w.flush()?;
            }
        }
    }

    fn mkfifo(path: &Path) -> Result<()> {
        let _ = std::fs::remove_file(path);
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `c` is a valid C string.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("make the pipe {}", path.display()));
        }
        Ok(())
    }

    /// Whether a pack server holds the lock of the folder `d`.
    fn running(d: &Path) -> bool {
        let Ok(f) = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(d.join("server.lock")) else {
            return false;
        };
        rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err()
    }

    /// Serve the packs of the project at `project` until a signal ends it.
    fn serve(project: &Path) -> Result<()> {
        let d = pipes(project);
        std::fs::create_dir_all(&d)?;
        let lock = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(d.join("server.lock"))?;
        if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
            bail!("a pack server runs for {} already", project.display());
        }
        // Pipes of connections that a server before this one did not finish.
        for e in std::fs::read_dir(&d)?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".req") || n.ends_with(".resp") {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let server = Arc::new(Server {
            map_root: project.join("meta/gitpack"),
            // A long-lived reader without the prune lease, as the server of a fast line: a
            // prune keeps every blob of the map (`objstore::maintain`).
            store: Store::open_existing_unleased(&project.join("meta/objects"), layr_objects::Keying::Plain)?,
            packs: RwLock::new((HashMap::new(), Vec::new())),
            made: Mutex::new((HashMap::new(), 0)),
        });
        server.current()?;
        let ctl = d.join("ctl");
        mkfifo(&ctl)?;
        std::fs::write(d.join("server.pid"), format!("{}\n", std::process::id()))?;
        // Read and write: the pipe never reaches its end while no client writes.
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&ctl)?;
        for line in BufReader::new(f).lines() {
            let id = line?;
            if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                continue;
            }
            let (server, d) = (server.clone(), d.clone());
            std::thread::spawn(move || {
                let (req, resp) = (d.join(format!("{id}.req")), d.join(format!("{id}.resp")));
                let r = (|| -> Result<()> {
                    let r = std::fs::File::open(&req)?;
                    let w = std::fs::OpenOptions::new().write(true).open(&resp)?;
                    let _ = std::fs::remove_file(&req);
                    let _ = std::fs::remove_file(&resp);
                    server.answer(r, w)
                })();
                if let Err(e) = r {
                    eprintln!("layr gitpack: connection {id}: {e:#}");
                }
                let _ = std::fs::remove_file(&req);
                let _ = std::fs::remove_file(&resp);
            });
        }
        drop(lock);
        Ok(())
    }

    /// True when the extension serves the folder of the git store `s`.
    pub fn is_mounted(s: &Path) -> bool {
        crate::sys::fs_type_name(&mount_dir(s)).is_ok_and(|t| t == "layrfs")
    }

    /// Unmount the folder of the git store `s` and stop its pack server.
    pub fn unmount(s: &Path) {
        let mnt = mount_dir(s);
        for _ in 0..20 {
            if !is_mounted(s) {
                break;
            }
            let _ = Command::new("/sbin/umount").arg(&mnt).stderr(Stdio::null()).status();
            if is_mounted(s) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let Some(project) = s.parent() else { return };
        let d = pipes(project);
        let pid = std::fs::read_to_string(d.join("server.pid")).ok().and_then(|t| t.trim().parse::<i32>().ok());
        if let Some(pid) = pid.and_then(rustix::process::Pid::from_raw) {
            if running(&d) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
        }
        let start = Instant::now();
        while running(&d) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Start the pack server and mount the folder when the project has a map and the folder does
    /// not answer. True when it answers.
    pub fn ensure_mounted(p: &Project) -> Result<bool> {
        let s = p.git_dir();
        if is_mounted(&s) {
            return Ok(true);
        }
        if !has_map(p) || !s.join("objects").is_dir() {
            return Ok(false);
        }
        let d = pipes(&p.dir);
        std::fs::create_dir_all(&d)?;
        if !running(&d) {
            let log = std::fs::OpenOptions::new().create(true).append(true).open(dir(p).join("server.log"))?;
            let mut c = Command::new(std::env::current_exe()?);
            c.args(["fs", "gitpack", "serve"]).arg(&p.dir).current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
            crate::sys::new_session(&mut c);
            let mut child = c.spawn().context("start the git pack server")?;
            let start = Instant::now();
            while !(running(&d) && d.join("server.pid").is_file() && d.join("ctl").exists()) {
                if let Ok(Some(status)) = child.try_wait() {
                    bail!("the git pack server stopped ({status}); see {}", dir(p).join("server.log").display());
                }
                if start.elapsed() > Duration::from_secs(20) {
                    let _ = child.kill();
                    bail!("the git pack server did not start; see {}", dir(p).join("server.log").display());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        let src = src_dir(&p.dir);
        std::fs::create_dir_all(&src)?;
        crate::fsutil::atomic_write(&src.join("layrfs-gitpack.conf"), format!("pipes={}\n", d.display()).as_bytes(), 0o644)?;
        let mnt = mount_dir(&s);
        std::fs::create_dir_all(&mnt)?;
        let out = Command::new("/sbin/mount").args(["-F", "-t", "layrfs", "-o", "nobrowse"]).arg(&src).arg(&mnt).output().context("run mount")?;
        let start = Instant::now();
        while !is_mounted(&s) {
            if start.elapsed() > Duration::from_secs(20) {
                bail!(
                    "cannot mount the git pack folder {} (is the LayrFS extension on?): {}",
                    mnt.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(true)
    }
}

/// The steps that do not depend on how the folder is served (Linux FUSE, macOS FSKit).
#[cfg(any(all(target_os = "linux", feature = "layrfs-fuse"), target_os = "macos"))]
mod ops {
    use super::*;
    use anyhow::Context;
    use crate::repo::Repo;
    use std::process::{Command, Stdio};

    /// Name the FUSE folder in the alternates of the git store.
    fn add_alternate(s: &Path) -> Result<()> {
        if uses_folder(s) {
            return Ok(());
        }
        let path = s.join("objects/info/alternates");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("{}\n", mount_dir(s).display()));
        std::fs::create_dir_all(path.parent().unwrap())?;
        crate::fsutil::atomic_write(&path, text.as_bytes(), 0o644)
    }

    fn git(s: &Path) -> Command {
        let mut c = Command::new("git");
        c.arg("-c").arg("safe.directory=*").env("GIT_DIR", s).env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE");
        c
    }

    /// The ids of the objects in the store's own packs (not its alternates) and loose files.
    fn local_objects(s: &Path) -> Result<(Vec<PathBuf>, Vec<(String, PathBuf)>, HashSet<[u8; 20]>)> {
        let objects = s.join("objects");
        let mut packs = Vec::new();
        let mut ids = HashSet::new();
        for e in std::fs::read_dir(objects.join("pack")).into_iter().flatten().flatten() {
            let path = e.path();
            if path.extension().is_some_and(|x| x == "idx") {
                let data = std::fs::read(&path)?;
                ensure!(data.len() >= 8 + 1024 && data[..8] == [0xff, 0x74, 0x4f, 0x63, 0, 0, 0, 2], "{} is not a pack index v2", path.display());
                let n = u32::from_be_bytes(data[8 + 1020..8 + 1024].try_into().unwrap()) as usize;
                ensure!(data.len() >= 1032 + n * 20, "{} is cut short", path.display());
                for i in 0..n {
                    ids.insert(data[1032 + i * 20..1032 + (i + 1) * 20].try_into().unwrap());
                }
                packs.push(path);
            }
        }
        let mut loose = Vec::new();
        for e in std::fs::read_dir(&objects)?.flatten() {
            let fan = e.file_name().to_string_lossy().into_owned();
            if fan.len() != 2 || !fan.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            for f in std::fs::read_dir(e.path())?.flatten() {
                let rest = f.file_name().to_string_lossy().into_owned();
                if rest.len() == 38 {
                    if let Ok(b) = hex::decode(format!("{fan}{rest}")) {
                        ids.insert(b.try_into().unwrap());
                        loose.push((format!("{fan}{rest}"), f.path()));
                    }
                }
            }
        }
        Ok((packs, loose, ids))
    }

    /// Remove from the git store the blobs that the FUSE folder serves: one new pack of every
    /// other object, then the old packs and loose objects. The caller checked that the folder
    /// answers. Returns the names of the removed packs (`pack-<id>`) and the objects removed.
    pub fn drop_git_blobs(p: &Project) -> Result<(Vec<String>, usize)> {
        let s = p.git_dir();
        ensure!(is_mounted(&s) && uses_folder(&s), "the git pack folder of the store does not answer");
        let mapped: HashSet<[u8; 20]> = load_map(&dir(p))?.into_iter().map(|e| e.sha).collect();
        let (packs, loose, local) = local_objects(&s)?;
        let gone = local.iter().filter(|id| mapped.contains(*id)).count();
        if gone == 0 {
            return Ok((Vec::new(), 0));
        }
        // Every git command of the folder reads the blobs before any copy goes: check a few.
        for id in local.iter().filter(|id| mapped.contains(*id)).take(16) {
            let ok = git(&s).args(["cat-file", "-e", &hex::encode(id)]).stderr(Stdio::null()).status()?.success();
            ensure!(ok, "the git pack folder does not serve blob {}", hex::encode(id));
        }
        let keep: Vec<String> = local.iter().filter(|id| !mapped.contains(*id)).map(hex::encode).collect();
        let base = s.join("objects/pack/pack");
        let mut c = git(&s);
        c.args(["pack-objects", "-q", "--local"]).arg(&base).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().context("start git pack-objects")?;
        let mut input = child.stdin.take().context("git pack-objects has no input")?;
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            let mut w = std::io::BufWriter::new(&mut input);
            for id in keep {
                writeln!(w, "{id}")?;
            }
            w.flush()
        });
        let out = child.wait_with_output()?;
        writer.join().map_err(|_| anyhow::anyhow!("git pack-objects writer failed"))??;
        ensure!(out.status.success(), "git pack-objects failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        let new = format!("pack-{}", String::from_utf8_lossy(&out.stdout).trim());
        let mut removed = Vec::new();
        for idx in packs {
            let Some(stem) = idx.file_stem().map(|x| x.to_string_lossy().into_owned()) else { continue };
            if stem == new {
                continue;
            }
            // The index first: git finds a pack by its index.
            let _ = std::fs::remove_file(&idx);
            for ext in ["pack", "rev", "bitmap", "keep", "promisor", "mtimes"] {
                let _ = std::fs::remove_file(idx.with_extension(ext));
            }
            removed.push(stem);
        }
        for (_, path) in loose {
            let _ = std::fs::remove_file(path);
        }
        Ok((removed, gone))
    }

    /// The step of `layr gc` (and of an import): objects for imported states, the map, the
    /// FUSE folder, then the blobs of the git store that the folder serves go. A summary line,
    /// or None when the project has no git store.
    pub fn maintain(ctx: &crate::ctx::Ctx, repo: &Repo) -> Result<Option<String>> {
        let p = &repo.p;
        let s = p.git_dir();
        if !s.join("objects").is_dir() || !crate::objstore::exists(p) {
            return Ok(None);
        }
        if p.config_value("git.objects")?.and_then(|v| v.as_str().map(String::from)).as_deref() == Some("git") {
            return Ok(None);
        }
        let built = crate::gitbridge::build_import_objects(ctx, repo)?;
        let u = update(p)?;
        if !has_map(p) || !ensure_mounted(p)? {
            return Ok(Some(format!("git objects: {built} imported states got objects, {} files mapped", u.objects)));
        }
        add_alternate(&s)?;
        let _refs = crate::gitbridge::refs_lock(repo)?;
        let (removed, gone) = drop_git_blobs(p)?;
        if !removed.is_empty() {
            crate::gitbridge::refresh_line_packs(repo, &removed);
        }
        Ok(Some(format!(
            "git objects: {built} imported states got objects, {} new files mapped ({} MB), {gone} git blobs now read from the object store",
            u.objects,
            u.bytes / 1_000_000
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_have_the_computed_length() {
        for n in [0usize, 1, 15, 16, 65520, 65535, 65536, 200_000] {
            assert_eq!(entry(&vec![7u8; n]).len() as u64, entry_len(n as u64));
        }
    }

    #[test]
    fn segments_round_trip() {
        let dir = std::env::temp_dir().join(format!("layr-gitpack-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ents = vec![ent(b"a\n", Src::Link(b"a\n".to_vec())), ent(b"bc", Src::Blob(Id([3; 32])))];
        write_segment(&dir.join("s.map"), &ents).unwrap();
        let back = read_segment(&dir.join("s.map")).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!((back[0].sha, back[0].crc, &back[0].src), (ents[0].sha, ents[0].crc, &ents[0].src));
        assert_eq!((back[1].sha, back[1].size, &back[1].src), (ents[1].sha, 2, &ents[1].src));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A pack made from map entries passes `git index-pack`, which gives the same index, and git
    /// reads every blob through it.
    #[test]
    fn git_reads_the_pack() {
        let dir = std::env::temp_dir().join(format!("layr-gitpack-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let datas: Vec<Vec<u8>> = vec![b"hello\n".to_vec(), vec![1u8; 70_000], b"x".to_vec(), Vec::new()];
        let mut ents: Vec<Ent> = datas.iter().map(|d| ent(d, Src::Link(d.clone()))).collect();
        ents.sort_by(|x, y| y.sha.cmp(&x.sha));
        let pack = Pack::new(ents.clone());
        let bytes = pack
            .read(0, pack.len(), &|i| {
                let Src::Link(d) = &pack.ents[i].src else { unreachable!() };
                Ok(std::sync::Arc::new(entry(d)))
            })
            .unwrap();
        assert_eq!(bytes.len() as u64, pack.len());
        // index-pack checks the real checksum at the end: give it one.
        let mut real = bytes[..bytes.len() - 20].to_vec();
        let sum: [u8; 20] = {
            use layr_hashers::Digest;
            let mut h = layr_hashers::GitCompatible::new();
            h.update(&real);
            h.finalize().into()
        };
        real.extend_from_slice(&sum);
        std::fs::write(dir.join("p.pack"), &real).unwrap();
        let ok = std::process::Command::new("git").args(["index-pack", "-o"]).arg(dir.join("p.idx")).arg(dir.join("p.pack")).status().unwrap();
        assert!(ok.success());
        let git_idx = std::fs::read(dir.join("p.idx")).unwrap();
        assert_eq!(&git_idx[..git_idx.len() - 40], &pack.idx[..pack.idx.len() - 40]);
        // A short read in the middle of an entry gives the same bytes as the whole.
        let part = pack
            .read(20, 100, &|i| {
                let Src::Link(d) = &pack.ents[i].src else { unreachable!() };
                Ok(std::sync::Arc::new(entry(d)))
            })
            .unwrap();
        assert_eq!(part, bytes[20..120]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
