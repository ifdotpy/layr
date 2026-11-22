//! git on v2: git reads file contents from a v2 store through FUSE.
//!
//! The store keeps file contents only in v2 (zstd). Commits and git trees stay real git objects
//! (about 2% of a git pack). A FUSE folder shows all files of the store as one git pack
//! (`pack/pack-<id>.pack` and its `.idx`). The index is made at mount from the map. A pack entry is
//! made when git reads its bytes: a blob as zlib "stored" blocks (no compression). The
//! `.git/objects/info/alternates` of a line names the FUSE folder.
//!
//! The first version showed each file as a loose object; its results are in the README.
//!
//! usage:
//!   git-on-v2 map <v2 store> <states> <out map>    git ids of every file of states s0..s{n-1}
//!   git-on-v2 mount <v2 store> <map> <folder>       serve the pack (runs until unmount)

use anyhow::{bail, ensure, Context, Result};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, KernelConfig, LockOwner, MountOption,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use layr_objects::{Id, Keying, Node, Store};
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

enum Src {
    Blob(Id),
    Link(Vec<u8>),
}

struct Ent {
    sha: [u8; 20],
    size: u64,
    /// CRC-32 of the whole pack entry (the pack index keeps it; `pack-objects` checks it on reuse).
    crc: u32,
    src: Src,
}

// ---------------------------------------------------------------- pack entry bytes

const BLOCK: usize = 65535;

/// The object header of a pack entry: type 3 (blob) and the size, 7 bits per byte after 4.
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

/// zlib "stored" blocks of `n` bytes: the length without making them.
fn zlen(n: u64) -> u64 {
    2 + n + 5 * n.div_ceil(BLOCK as u64).max(1) + 4
}

fn entry_len(size: u64) -> u64 {
    entry_header(size).len() as u64 + zlen(size)
}

/// A pack entry: header, then the content as zlib "stored" blocks.
fn entry(size: u64, data: &[u8]) -> Vec<u8> {
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
    debug_assert_eq!(out.len() as u64, entry_len(size));
    out
}

// ---------------------------------------------------------------- map

/// The files of a state in path order, each first seen one once: the pack order. A newest state
/// first puts the versions git reads most next to each other, as v2 packs keep them.
fn walk(store: &Store, tree: &Id, seen: &mut HashSet<Id>, found: &mut HashSet<Vec<u8>>, order: &mut Vec<(Src, u64)>) -> Result<()> {
    if !seen.insert(*tree) {
        return Ok(());
    }
    for e in store.get_tree(tree)?.entries {
        match e.node {
            Node::File { size, blob } => {
                if found.insert(blob.0.to_vec()) {
                    order.push((Src::Blob(blob), size));
                }
            }
            Node::Dir { tree } => walk(store, &tree, seen, found, order)?,
            Node::Symlink { target } => {
                if found.insert(target.clone()) {
                    order.push((Src::Link(target.clone()), target.len() as u64));
                }
            }
        }
    }
    Ok(())
}

fn ent(data: &[u8], src: Src) -> Ent {
    let size = data.len() as u64;
    let mut h = Sha1::new();
    h.update(format!("blob {size}\0"));
    h.update(data);
    Ent { sha: h.finalize().into(), size, crc: crc32fast::hash(&entry(size, data)), src }
}

fn map(a: &[String]) -> Result<()> {
    let (root, n, out) = (PathBuf::from(&a[2]), a[3].parse::<usize>()?, PathBuf::from(&a[4]));
    let store = Store::open_existing(&root, Keying::Plain)?;
    let t = Instant::now();
    let (mut seen, mut found, mut order) = (HashSet::new(), HashSet::new(), Vec::new());
    for k in (0..n).rev() {
        let st = store.state_ref(&format!("s{k}"))?.with_context(|| format!("no state s{k}"))?;
        walk(&store, &st.tree, &mut seen, &mut found, &mut order)?;
    }
    let walk_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let ents: Vec<Ent> = order
        .into_par_iter()
        .map(|(src, size)| -> Result<Ent> {
            let data = match &src {
                Src::Blob(id) => store.read_blob(id)?,
                Src::Link(l) => l.clone(),
            };
            ensure!(data.len() as u64 == size, "an object has the wrong size");
            Ok(ent(&data, src))
        })
        .collect::<Result<_>>()?;
    let hash_ms = t.elapsed().as_secs_f64() * 1e3;
    // A file and a symbolic link with the same bytes are one git object: keep the first.
    let mut ids = HashSet::new();
    let ents: Vec<Ent> = ents.into_iter().filter(|e| ids.insert(e.sha)).collect();
    let mut w = BufWriter::new(std::fs::File::create(&out)?);
    let mut shas = BufWriter::new(std::fs::File::create(out.with_extension("shas"))?);
    w.write_all(&(ents.len() as u64).to_le_bytes())?;
    let mut raw = 0u64;
    for e in &ents {
        raw += e.size;
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
        writeln!(shas, "{}", hex(&e.sha))?;
    }
    w.flush()?;
    shas.flush()?;
    let bytes = std::fs::metadata(&out)?.len();
    println!(
        "{{\"objects\":{},\"trees\":{},\"raw_bytes\":{raw},\"map_bytes\":{bytes},\"walk_ms\":{walk_ms:.0},\"hash_ms\":{hash_ms:.0}}}",
        ents.len(),
        seen.len()
    );
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn load(path: &Path) -> Result<Vec<Ent>> {
    let mut r = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut u8s = [0u8; 8];
    let mut u4s = [0u8; 4];
    r.read_exact(&mut u8s)?;
    let n = u64::from_le_bytes(u8s) as usize;
    let mut ents = Vec::with_capacity(n);
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
                let mut t = vec![0u8; u32::from_le_bytes(u4s) as usize];
                r.read_exact(&mut t)?;
                Src::Link(t)
            }
            t => bail!("bad tag {t}"),
        };
        ents.push(Ent { sha, size, crc, src });
    }
    Ok(ents)
}

// ---------------------------------------------------------------- the pack and its index

/// The pack index (version 2) of `ents` at `offsets`, and the pack checksum both files name.
/// `ents` and `offsets` are in pack order; the index lists them by id.
fn index(ents: &[Ent], offsets: &[u64]) -> (Vec<u8>, [u8; 20]) {
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
    // git checks only that the pack and the index name the same checksum, not the pack bytes.
    let pack_sum: [u8; 20] = Sha1::digest(&out).into();
    out.extend_from_slice(&pack_sum);
    let idx_sum: [u8; 20] = Sha1::digest(&out).into();
    out.extend_from_slice(&idx_sum);
    (out, pack_sum)
}

// ---------------------------------------------------------------- FUSE

const TTL: Duration = Duration::from_secs(3600);
const ROOT: u64 = 1;
const PACKDIR: u64 = 2;
const IDX: u64 = 3;
const PACK: u64 = 4;
const CACHE_BYTES: u64 = 1 << 30;

struct Fs {
    ents: Vec<Ent>,
    /// The pack offset of each entry; one more for the trailer.
    offsets: Vec<u64>,
    idx: Vec<u8>,
    head: Vec<u8>,
    trailer: [u8; 20],
    name: String,
    store: Store,
    uid: u32,
    gid: u32,
    /// Entries made before, kept until they pass `CACHE_BYTES` (then the cache starts again).
    made: Mutex<(HashMap<usize, Arc<Vec<u8>>>, u64)>,
    stats: Arc<[AtomicU64; 4]>, // entries made, bytes made, ns making, pack reads
}

impl Fs {
    fn pack_len(&self) -> u64 {
        self.offsets[self.ents.len()] + 20
    }

    fn attr(&self, ino: u64) -> FileAttr {
        let (kind, size) = match ino {
            IDX => (FileType::RegularFile, self.idx.len() as u64),
            PACK => (FileType::RegularFile, self.pack_len()),
            _ => (FileType::Directory, 0),
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm: if kind == FileType::Directory { 0o555 } else { 0o444 },
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
            st_ino: 0,
        }
    }

    fn entry_bytes(&self, i: usize) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.made.lock().unwrap().0.get(&i) {
            return Ok(b.clone());
        }
        let t = Instant::now();
        let e = &self.ents[i];
        let data = match &e.src {
            Src::Link(l) => l.clone(),
            Src::Blob(id) => self.store.read_blob(id)?,
        };
        let b = Arc::new(entry(e.size, &data));
        self.stats[0].fetch_add(1, Relaxed);
        self.stats[1].fetch_add(b.len() as u64, Relaxed);
        self.stats[2].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        let mut m = self.made.lock().unwrap();
        if m.1 > CACHE_BYTES {
            *m = (HashMap::new(), 0);
        }
        m.1 += b.len() as u64;
        m.0.insert(i, b.clone());
        Ok(b)
    }

    /// Bytes `[at, at + len)` of the pack.
    fn pack_read(&self, at: u64, len: u64) -> Result<Vec<u8>> {
        let end = (at + len).min(self.pack_len());
        let mut out = Vec::with_capacity(end.saturating_sub(at) as usize);
        let mut pos = at;
        while pos < end {
            if pos < 12 {
                out.extend_from_slice(&self.head[pos as usize..12.min(end) as usize]);
                pos = 12.min(end);
                continue;
            }
            let tail = self.offsets[self.ents.len()];
            if pos >= tail {
                out.extend_from_slice(&self.trailer[(pos - tail) as usize..(end - tail) as usize]);
                break;
            }
            let i = self.offsets.partition_point(|&o| o <= pos) - 1;
            let b = self.entry_bytes(i)?;
            let s = (pos - self.offsets[i]) as usize;
            let e = ((end - self.offsets[i]) as usize).min(b.len());
            out.extend_from_slice(&b[s..e]);
            pos = self.offsets[i] + e as u64;
        }
        Ok(out)
    }
}

impl Filesystem for Fs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // The pack is in path order (newest first), so the bytes after a read are likely next.
        // GIT_ON_V2_READAHEAD: bytes to read ahead (default 1 MiB).
        let ra = std::env::var("GIT_ON_V2_READAHEAD").ok().and_then(|v| v.parse().ok()).unwrap_or(1 << 20);
        let got = config.set_max_readahead(ra);
        eprintln!("readahead {ra}: {got:?}");
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_str().unwrap_or("");
        let ino = match (u64::from(parent), name) {
            (ROOT, "pack") => PACKDIR,
            (PACKDIR, n) if n == format!("{}.idx", self.name) => IDX,
            (PACKDIR, n) if n == format!("{}.pack", self.name) => PACK,
            _ => return reply.error(Errno::ENOENT),
        };
        reply.entry(&TTL, &self.attr(ino), fuser::Generation(0))
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match u64::from(ino) {
            i @ ROOT..=PACK => reply.attr(&TTL, &self.attr(i)),
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match u64::from(ino) {
            IDX | PACK => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            _ => reply.error(Errno::EISDIR),
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
        match u64::from(ino) {
            IDX => {
                let s = (offset as usize).min(self.idx.len());
                let e = (s + size as usize).min(self.idx.len());
                reply.data(&self.idx[s..e])
            }
            PACK => {
                self.stats[3].fetch_add(1, Relaxed);
                match self.pack_read(offset, size as u64) {
                    Ok(b) => reply.data(&b),
                    Err(e) => {
                        eprintln!("read {offset}+{size}: {e:#}");
                        reply.error(Errno::EIO)
                    }
                }
            }
            _ => reply.error(Errno::EISDIR),
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::FOPEN_CACHE_DIR | FopenFlags::FOPEN_KEEP_CACHE)
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let p = u64::from(ino);
        let mut list: Vec<(u64, FileType, String)> = vec![(p, FileType::Directory, ".".into()), (ROOT, FileType::Directory, "..".into())];
        match p {
            ROOT => list.push((PACKDIR, FileType::Directory, "pack".into())),
            PACKDIR => {
                list.push((IDX, FileType::RegularFile, format!("{}.idx", self.name)));
                list.push((PACK, FileType::RegularFile, format!("{}.pack", self.name)));
            }
            _ => return reply.error(Errno::ENOTDIR),
        }
        for (k, (ino, kind, name)) in list.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (k + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok()
    }
}

fn mount(a: &[String]) -> Result<()> {
    let (root, mapf, mnt) = (PathBuf::from(&a[2]), PathBuf::from(&a[3]), PathBuf::from(&a[4]));
    let t = Instant::now();
    let ents = load(&mapf)?;
    let mut offsets = Vec::with_capacity(ents.len() + 1);
    let mut at = 12u64;
    for e in &ents {
        offsets.push(at);
        at += entry_len(e.size);
    }
    offsets.push(at);
    let (idx, sum) = index(&ents, &offsets[..ents.len()]);
    let mut head = b"PACK".to_vec();
    head.extend_from_slice(&2u32.to_be_bytes());
    head.extend_from_slice(&(ents.len() as u32).to_be_bytes());
    eprintln!("{} objects, pack {} bytes, index {} bytes, made in {:.0} ms", ents.len(), at + 20, idx.len(), t.elapsed().as_secs_f64() * 1e3);
    let m = std::fs::metadata(&mnt)?;
    let fs = Fs {
        ents,
        offsets,
        idx,
        head,
        trailer: sum,
        name: format!("pack-{}", hex(&sum)),
        store: Store::open_existing(&root, Keying::Plain)?,
        uid: m.uid(),
        gid: m.gid(),
        made: Mutex::new((HashMap::new(), 0)),
        stats: Arc::new(Default::default()),
    };
    // Counters for the benchmark: a file next to the map, rewritten each second.
    let st = fs.stats.clone();
    let stats = mapf.with_extension("stats");
    std::thread::spawn(move || loop {
        let s = format!(
            "{{\"opened\":{},\"made_bytes\":{},\"made_ms\":{:.1},\"reads\":{}}}\n",
            st[0].load(Relaxed),
            st[1].load(Relaxed),
            st[2].load(Relaxed) as f64 / 1e6,
            st[3].load(Relaxed)
        );
        let _ = std::fs::write(&stats, s);
        std::thread::sleep(Duration::from_secs(1));
    });
    let mut cfg = Config::default();
    cfg.mount_options.extend([MountOption::RO, MountOption::FSName("git-on-v2".into())]);
    cfg.n_threads = Some(8);
    cfg.clone_fd = true;
    fuser::mount(fs, &mnt, &cfg)?;
    Ok(())
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("map") if a.len() == 5 => map(&a),
        Some("mount") if a.len() == 5 => mount(&a),
        _ => bail!("usage: git-on-v2 map <store> <states> <out> | mount <store> <map> <folder>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_have_the_computed_length() {
        for n in [0usize, 1, 15, 16, 65520, 65535, 65536, 200_000] {
            let data = vec![7u8; n];
            assert_eq!(entry(n as u64, &data).len() as u64, entry_len(n as u64));
        }
    }

    #[test]
    fn git_reads_the_pack() {
        // A pack of two blobs made here must pass `git index-pack` and give the same index.
        let dir = std::env::temp_dir().join(format!("git-on-v2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Not in id order: the index must sort them.
        let mut ents: Vec<Ent> = [b"hello\n".to_vec(), vec![1u8; 70_000], b"x".to_vec()].into_iter().map(|d| ent(&d.clone(), Src::Link(d))).collect();
        ents.sort_by(|x, y| y.sha.cmp(&x.sha));
        let mut offsets = vec![];
        let mut at = 12u64;
        let mut pack = b"PACK".to_vec();
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(ents.len() as u32).to_be_bytes());
        for e in &ents {
            offsets.push(at);
            let Src::Link(d) = &e.src else { unreachable!() };
            let b = entry(e.size, d);
            at += b.len() as u64;
            pack.extend_from_slice(&b);
        }
        let real: [u8; 20] = Sha1::digest(&pack).into();
        pack.extend_from_slice(&real);
        std::fs::write(dir.join("p.pack"), &pack).unwrap();
        let ok = std::process::Command::new("git").args(["index-pack", "-o"]).arg(dir.join("p.idx")).arg(dir.join("p.pack")).status().unwrap();
        assert!(ok.success());
        let git_idx = std::fs::read(dir.join("p.idx")).unwrap();
        let (ours, _) = index(&ents, &offsets);
        // Same fan-out, ids, CRCs and offsets; only the checksums at the end differ.
        assert_eq!(&git_idx[..git_idx.len() - 40], &ours[..ours.len() - 40]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
