//! The object store on disk (docs/objects.md, "Layout").
//!
//! ```text
//! <root>/config.json         format version and id hash
//! <root>/blobs/ab/cdef...    hot blobs: the exact file bytes, so a checkout clones them
//! <root>/objects/ab/cdef...  encoded objects: trees, cold blobs (zstd, delta, chunk list), chunks
//! <root>/states/<state>      the tree id of an imported state
//! <root>/tmp/                files being written
//! ```
//!
//! A write never replaces a file: it writes a temporary file and links it to its final name, and an
//! existing name means the object is already there. So writes are idempotent, concurrent writers of
//! the same object agree, and a reader never sees a partial object.

use crate::fsx;
use crate::id::{Id, Keying, Kind};
use crate::tree::{varint_into, Reader, Tree};
use anyhow::{bail, ensure, Context, Result};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 4] = b"LOB1";
const WHOLE: u8 = b'z';
const DELTA: u8 = b'd';
const CHUNKED: u8 = b'c';
const CONFIG_VERSION: u64 = 1;

/// Size rules of cold writes.
#[derive(Clone, Debug)]
pub struct Options {
    /// A cold file of this size or more is split into content-defined chunks (unless a delta fits).
    pub chunk_min_file: u64,
    /// A delta needs the new and the previous version in memory: both must be at most this size.
    pub delta_max: u64,
    /// The longest delta chain. A version past it is stored whole (a key frame), so a read decodes
    /// at most this many deltas.
    pub max_depth: u8,
}

impl Default for Options {
    fn default() -> Self {
        Options { chunk_min_file: 1 << 20, delta_max: 64 << 20, max_depth: 16 }
    }
}

/// How the store keeps a new blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Temperature {
    /// Whole and uncompressed, as a clone of the source when the file system can: a checkout
    /// clones it back. For the current bases of lines.
    Hot,
    /// Compressed: a delta against `previous` when there is one, chunks for large files, else one
    /// zstd frame. For history.
    Cold,
}

/// The counters of one store, for `layr objects stat` and for reports.
#[derive(Clone, Debug, Default)]
pub struct Stat {
    pub hot_blobs: u64,
    pub hot_bytes: u64,
    pub trees: u64,
    pub tree_bytes: u64,
    pub whole_blobs: u64,
    pub whole_bytes: u64,
    pub delta_blobs: u64,
    pub delta_bytes: u64,
    pub chunked_blobs: u64,
    pub chunked_bytes: u64,
    pub chunks: u64,
    pub chunk_bytes: u64,
    pub states: u64,
}

impl Stat {
    pub fn total_bytes(&self) -> u64 {
        self.hot_bytes + self.tree_bytes + self.whole_bytes + self.delta_bytes + self.chunked_bytes + self.chunk_bytes
    }
}

/// What writes added (new objects only: an object that was there counts nowhere).
#[derive(Debug, Default)]
pub struct Added {
    pub objects: AtomicU64,
    pub bytes: AtomicU64,
    pub clones: AtomicU64,
}

/// A step for each new clone of a source file, before the store links it (layr clears the
/// immutable flag that a sealed APFS state gives its files, and a clone copies; the flag would
/// refuse the rename). Kept outside this crate: it needs a system call without a safe wrapper.
pub type CloneHook = fn(std::os::fd::BorrowedFd<'_>) -> io::Result<()>;

pub struct Store {
    root: PathBuf,
    keying: Keying,
    pub options: Options,
    pub added: Added,
    pub clone_hook: Option<CloneHook>,
}

/// One stored form of a blob, read from the header of its object file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    Hot,
    Whole,
    Delta { base: Id, depth: u8 },
    Chunked,
}

impl Store {
    /// Open the store at `root`, made if absent. The id hash of an existing store must match.
    pub fn open(root: &Path, keying: Keying) -> Result<Store> {
        for dir in ["blobs", "objects", "states", "tmp"] {
            fs::create_dir_all(root.join(dir)).with_context(|| format!("cannot make {}", root.join(dir).display()))?;
        }
        let config = root.join("config.json");
        let want = format!("{{\"ids\":\"{}\",\"v\":{CONFIG_VERSION}}}\n", keying.name());
        match fs::read_to_string(&config) {
            Ok(text) => ensure!(text == want, "the object store at {} has another format: {}", root.display(), text.trim()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let tmp = root.join("tmp").join(format!("config-{}", std::process::id()));
                fs::write(&tmp, &want)?;
                if let Err(e) = fs::hard_link(&tmp, &config) {
                    let _ = fs::remove_file(&tmp);
                    if e.kind() != io::ErrorKind::AlreadyExists {
                        return Err(e.into());
                    }
                    ensure!(fs::read_to_string(&config)? == want, "the object store has another format");
                }
                let _ = fs::remove_file(&tmp);
            }
            Err(e) => return Err(e.into()),
        }
        remove_orphans(&root.join("tmp"));
        Ok(Store { root: root.to_path_buf(), keying, options: Options::default(), added: Added::default(), clone_hook: None })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn keying(&self) -> &Keying {
        &self.keying
    }

    fn fan(&self, area: &str, id: &Id) -> PathBuf {
        let hex = id.hex();
        self.root.join(area).join(&hex[..2]).join(&hex[2..])
    }

    /// The whole file of a hot blob, if the store has one.
    pub fn hot_path(&self, id: &Id) -> Option<PathBuf> {
        let p = self.fan("blobs", id);
        p.is_file().then_some(p)
    }

    fn object_path(&self, id: &Id) -> PathBuf {
        self.fan("objects", id)
    }

    pub fn has(&self, id: &Id) -> bool {
        self.fan("blobs", id).exists() || self.object_path(id).exists()
    }

    /// A unique temporary path in `tmp/`.
    fn temp(&self) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        self.root.join("tmp").join(format!("{}-{n}", std::process::id()))
    }

    /// Give the finished temporary file `tmp` its final name `dst`, unless `dst` is there. True
    /// when this call added it.
    fn publish(&self, tmp: &Path, dst: &Path) -> Result<bool> {
        let parent = dst.parent().expect("a fan-out path has a parent");
        let mut moved = move_new(tmp, dst);
        if matches!(&moved, Err(e) if e.kind() == io::ErrorKind::NotFound) {
            fs::create_dir_all(parent)?;
            moved = move_new(tmp, dst);
        }
        match moved {
            Ok(()) => Ok(true),
            Err(e) => {
                let _ = fs::remove_file(tmp);
                if e.kind() == io::ErrorKind::AlreadyExists {
                    Ok(false)
                } else {
                    Err(e).with_context(|| format!("cannot store {}", dst.display()))
                }
            }
        }
    }

    fn write_object(&self, id: &Id, kind: Kind, form: u8, body: &[&[u8]]) -> Result<bool> {
        let dst = self.object_path(id);
        if dst.exists() {
            return Ok(false);
        }
        let tmp = self.temp();
        let written = (|| -> Result<u64> {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o444).open(&tmp)?;
            f.write_all(MAGIC)?;
            f.write_all(&[kind.byte(), form])?;
            let mut n = 6;
            for part in body {
                f.write_all(part)?;
                n += part.len() as u64;
            }
            Ok(n)
        })();
        let n = match written {
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        let added = self.publish(&tmp, &dst)?;
        if added {
            self.added.objects.fetch_add(1, Ordering::Relaxed);
            self.added.bytes.fetch_add(n, Ordering::Relaxed);
        }
        Ok(added)
    }

    fn read_object(&self, id: &Id) -> Result<(Kind, u8, Vec<u8>)> {
        let path = self.object_path(id);
        let data = fs::read(&path).with_context(|| format!("object {id} is missing"))?;
        ensure!(data.len() >= 6 && &data[..4] == MAGIC, "object {id} has no valid header");
        Ok((Kind::from_byte(data[4])?, data[5], data[6..].to_vec()))
    }

    pub fn put_tree(&self, tree: &Tree) -> Result<Id> {
        let bytes = tree.encode();
        let id = self.keying.id(Kind::Tree, &bytes);
        if !self.object_path(&id).exists() {
            let frame = layr_store::zst::compress(&bytes, None)?;
            self.write_object(&id, Kind::Tree, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    pub fn get_tree(&self, id: &Id) -> Result<Tree> {
        let (kind, form, body) = self.read_object(id)?;
        ensure!(kind == Kind::Tree && form == WHOLE, "object {id} is not a tree");
        let bytes = layr_store::zst::decompress(&body, None)?;
        ensure!(self.keying.id(Kind::Tree, &bytes) == *id, "tree {id} does not match its id");
        Tree::decode(&bytes)
    }

    /// The id of the file at `path`, read once.
    pub fn hash_file(&self, path: &Path) -> Result<(Id, u64)> {
        let mut f = File::open(path)?;
        let len = f.metadata()?.len();
        let mut h = self.keying.hasher(Kind::Blob, len);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok((h.finish().with_context(|| path.display().to_string())?, len))
    }

    /// Store the file at `path` and return its id. `previous` is the blob of the same path in the
    /// previous tree (a delta base for a cold write).
    pub fn put_file(&self, path: &Path, temperature: Temperature, previous: Option<Id>) -> Result<(Id, u64)> {
        let (id, len) = self.hash_file(path)?;
        match temperature {
            Temperature::Hot if self.fan("blobs", &id).exists() => Ok((id, len)),
            Temperature::Hot => self.put_hot(path),
            Temperature::Cold if self.has(&id) => Ok((id, len)),
            Temperature::Cold => {
                let data = if len < self.options.chunk_min_file || (previous.is_some() && len <= self.options.delta_max) {
                    Some(fs::read(path)?)
                } else {
                    None
                };
                match data {
                    Some(data) => self.put_cold_bytes(&data, previous),
                    None => self.put_chunked_file(path),
                }
            }
        }
    }

    /// A hot write: clone (or copy) the file into `tmp/`, hash the copy, and publish it. The id is
    /// the hash of the stored copy, so a file that changes during the write cannot give a wrong id.
    fn put_hot(&self, path: &Path) -> Result<(Id, u64)> {
        let tmp = self.temp();
        let result = (|| -> Result<(Id, u64, bool)> {
            let cloned = fsx::clone_or_copy(path, &tmp)?;
            if let (true, Some(hook)) = (cloned, self.clone_hook) {
                use std::os::fd::AsFd;
                hook(File::open(&tmp)?.as_fd())?;
            }
            // A clone takes the attributes of its source; the tree keeps them per entry.
            fsx::clear_xattrs(&tmp)?;
            let (id, len) = self.hash_file(&tmp)?;
            Ok((id, len, cloned))
        })();
        let (id, len, cloned) = match result {
            Ok(x) => x,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        if self.publish(&tmp, &self.fan("blobs", &id))? {
            self.added.objects.fetch_add(1, Ordering::Relaxed);
            self.added.bytes.fetch_add(len, Ordering::Relaxed);
            if cloned {
                self.added.clones.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok((id, len))
    }

    /// The size of a blob where the store knows it without a decode: a hot blob, a chunk list.
    /// The other forms are smaller than `delta_max` by the write rules.
    fn known_size(&self, id: &Id) -> Option<u64> {
        if let Some(p) = self.hot_path(id) {
            return fs::metadata(p).ok().map(|m| m.len());
        }
        let (kind, form, body) = self.read_object(id).ok()?;
        if kind != Kind::Blob || form != CHUNKED {
            return None;
        }
        let mut r = Reader::new(&body);
        let n = r.varint().ok()?;
        let mut total = 0u64;
        for _ in 0..n {
            r.id().ok()?;
            total = total.saturating_add(r.varint().ok()?);
        }
        Some(total)
    }

    /// The delta depth of a stored blob: 0 for any form that decodes alone.
    fn depth(&self, id: &Id) -> Result<u8> {
        Ok(match self.form(id)? {
            Form::Delta { depth, .. } => depth,
            _ => 0,
        })
    }

    /// Store `data` cold: a delta against `previous` when it is stored, within the size and depth
    /// limits, and smaller than a whole frame would likely be; else one zstd frame.
    pub fn put_cold_bytes(&self, data: &[u8], previous: Option<Id>) -> Result<(Id, u64)> {
        let id = self.keying.id(Kind::Blob, data);
        let len = data.len() as u64;
        if self.has(&id) {
            return Ok((id, len));
        }
        let base = match previous.filter(|p| *p != id && self.has(p)) {
            Some(p) if len <= self.options.delta_max => {
                let depth = self.depth(&p)?;
                (depth < self.options.max_depth).then_some((p, depth))
            }
            _ => None,
        };
        if let Some((base_id, depth)) = base.filter(|(p, _)| self.known_size(p).is_none_or(|n| n <= self.options.delta_max)) {
            let base = self.read_blob(&base_id)?;
            if base.len() as u64 <= self.options.delta_max {
                let frame = layr_store::zst::compress(data, Some(&base))?;
                self.write_object(&id, Kind::Blob, DELTA, &[&[depth + 1], &base_id.0, &frame])?;
                return Ok((id, len));
            }
        }
        if len >= self.options.chunk_min_file {
            self.put_chunked_bytes(id, data)?;
        } else {
            let frame = layr_store::zst::compress(data, None)?;
            self.write_object(&id, Kind::Blob, WHOLE, &[&frame])?;
        }
        Ok((id, len))
    }

    fn put_chunk(&self, chunk: &[u8]) -> Result<Id> {
        let id = self.keying.id(Kind::Chunk, chunk);
        if !self.object_path(&id).exists() {
            let frame = layr_store::zst::compress(chunk, None)?;
            self.write_object(&id, Kind::Chunk, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    fn put_chunked_bytes(&self, id: Id, data: &[u8]) -> Result<()> {
        let mut list = Vec::new();
        let chunks = layr_store::chunks::split(data);
        varint_into(&mut list, chunks.len() as u64);
        for chunk in chunks {
            list.extend_from_slice(&self.put_chunk(chunk)?.0);
            varint_into(&mut list, chunk.len() as u64);
        }
        self.write_object(&id, Kind::Blob, CHUNKED, &[&list])?;
        Ok(())
    }

    /// Chunk a large file as a stream: the cut points are those of `chunks::split` on the whole
    /// file, because a cut depends only on the bytes from the chunk start up to `chunks::MAX`.
    fn put_chunked_file(&self, path: &Path) -> Result<(Id, u64)> {
        use layr_store::chunks::MAX;
        let mut f = File::open(path)?;
        let len = f.metadata()?.len();
        let mut h = self.keying.hasher(Kind::Blob, len);
        let mut ids: Vec<(Id, u64)> = Vec::new();
        let mut buf: Vec<u8> = Vec::with_capacity(8 * MAX);
        let mut eof = false;
        loop {
            while !eof && buf.len() < 4 * MAX {
                let start = buf.len();
                buf.resize(start + 4 * MAX, 0);
                let n = f.read(&mut buf[start..])?;
                buf.truncate(start + n);
                eof = n == 0;
            }
            if buf.is_empty() {
                break;
            }
            let mut used = 0;
            for chunk in layr_store::chunks::split(&buf) {
                // A chunk that starts in the last MAX bytes may end at the buffer end, not at its cut.
                if !eof && buf.len() - used < MAX {
                    break;
                }
                h.update(chunk);
                ids.push((self.put_chunk(chunk)?, chunk.len() as u64));
                used += chunk.len();
            }
            buf.drain(..used);
        }
        let id = h.finish().with_context(|| path.display().to_string())?;
        if !self.has(&id) {
            let mut list = Vec::new();
            varint_into(&mut list, ids.len() as u64);
            for (chunk, n) in &ids {
                list.extend_from_slice(&chunk.0);
                varint_into(&mut list, *n);
            }
            self.write_object(&id, Kind::Blob, CHUNKED, &[&list])?;
        }
        Ok((id, len))
    }

    /// How a blob is stored (the hot form first).
    pub fn form(&self, id: &Id) -> Result<Form> {
        if self.fan("blobs", id).exists() {
            return Ok(Form::Hot);
        }
        let mut head = [0u8; 6 + 1 + 32];
        let mut f = File::open(self.object_path(id)).with_context(|| format!("object {id} is missing"))?;
        let n = read_up_to(&mut f, &mut head)?;
        ensure!(n >= 6 && &head[..4] == MAGIC, "object {id} has no valid header");
        Ok(match head[5] {
            WHOLE => Form::Whole,
            CHUNKED => Form::Chunked,
            DELTA => {
                ensure!(n == head.len(), "delta object {id} is cut short");
                Form::Delta { depth: head[6], base: Id(head[7..39].try_into().expect("32 bytes")) }
            }
            other => bail!("object {id} has unknown form {other:#x}"),
        })
    }

    /// The content of a blob, checked against its id.
    pub fn read_blob(&self, id: &Id) -> Result<Vec<u8>> {
        let data = self.read_blob_unchecked(id, 0)?;
        ensure!(self.keying.id(Kind::Blob, &data) == *id, "blob {id} does not match its id");
        Ok(data)
    }

    fn read_blob_unchecked(&self, id: &Id, depth: u32) -> Result<Vec<u8>> {
        ensure!(depth <= 255, "delta chain of {id} is too long");
        if let Some(p) = self.hot_path(id) {
            return Ok(fs::read(p)?);
        }
        let (kind, form, body) = self.read_object(id)?;
        ensure!(kind == Kind::Blob, "object {id} is a {}, not a blob", kind.name());
        match form {
            WHOLE => layr_store::zst::decompress(&body, None),
            DELTA => {
                ensure!(body.len() > 33, "delta object {id} is cut short");
                let base_id = Id(body[1..33].try_into().expect("32 bytes"));
                let base = self.read_blob_unchecked(&base_id, depth + 1)?;
                layr_store::zst::decompress(&body[33..], Some(&base))
            }
            CHUNKED => {
                let mut r = Reader::new(&body);
                let n = r.varint()?;
                let mut out = Vec::new();
                for _ in 0..n {
                    let chunk_id = r.id()?;
                    let len = r.varint()?;
                    let (kind, form, frame) = self.read_object(&chunk_id)?;
                    ensure!(kind == Kind::Chunk && form == WHOLE, "object {chunk_id} is not a chunk");
                    let chunk = layr_store::zst::decompress(&frame, None)?;
                    ensure!(chunk.len() as u64 == len, "chunk {chunk_id} has the wrong length");
                    ensure!(self.keying.id(Kind::Chunk, &chunk) == chunk_id, "chunk {chunk_id} does not match its id");
                    out.extend_from_slice(&chunk);
                }
                ensure!(r.done(), "bytes after the chunk list of {id}");
                Ok(out)
            }
            other => bail!("object {id} has unknown form {other:#x}"),
        }
    }

    /// Record the tree of an imported state.
    pub fn set_state(&self, state: &str, tree: &Id) -> Result<()> {
        ensure!(!state.is_empty() && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid state id {state}");
        let tmp = self.temp();
        fs::write(&tmp, format!("{tree}\n"))?;
        fs::rename(&tmp, self.root.join("states").join(state))?;
        Ok(())
    }

    pub fn state_tree(&self, state: &str) -> Result<Option<Id>> {
        if state.is_empty() || !state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Ok(None);
        }
        match fs::read_to_string(self.root.join("states").join(state)) {
            Ok(text) => Ok(Some(Id::parse(text.trim())?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The imported states whose tree is `tree`.
    pub fn states_of(&self, tree: &Id) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("states"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if self.state_tree(&name)?.as_ref() == Some(tree) {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Count the objects by form and add their file sizes (the fan-out folders in parallel).
    pub fn stat(&self) -> Result<Stat> {
        use rayon::prelude::*;
        let parts = |area: &str| -> Result<Vec<Stat>> {
            fan_folders(&self.root.join(area))?.par_iter().map(|dir| self.stat_folder(area == "blobs", dir)).collect()
        };
        let mut s = Stat::default();
        for p in parts("blobs")?.into_iter().chain(parts("objects")?) {
            s.hot_blobs += p.hot_blobs;
            s.hot_bytes += p.hot_bytes;
            s.trees += p.trees;
            s.tree_bytes += p.tree_bytes;
            s.whole_blobs += p.whole_blobs;
            s.whole_bytes += p.whole_bytes;
            s.delta_blobs += p.delta_blobs;
            s.delta_bytes += p.delta_bytes;
            s.chunked_blobs += p.chunked_blobs;
            s.chunked_bytes += p.chunked_bytes;
            s.chunks += p.chunks;
            s.chunk_bytes += p.chunk_bytes;
        }
        s.states = fs::read_dir(self.root.join("states"))?.count() as u64;
        Ok(s)
    }

    fn stat_folder(&self, hot: bool, dir: &Path) -> Result<Stat> {
        let mut s = Stat::default();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let len = fs::symlink_metadata(&path)?.len();
            if hot {
                s.hot_blobs += 1;
                s.hot_bytes += len;
                continue;
            }
            let mut head = [0u8; 6];
            let n = read_up_to(&mut File::open(&path)?, &mut head)?;
            ensure!(n == 6 && &head[..4] == MAGIC, "{} has no valid header", path.display());
            let (count, bytes) = match (Kind::from_byte(head[4])?, head[5]) {
                (Kind::Tree, _) => (&mut s.trees, &mut s.tree_bytes),
                (Kind::Chunk, _) => (&mut s.chunks, &mut s.chunk_bytes),
                (Kind::Blob, WHOLE) => (&mut s.whole_blobs, &mut s.whole_bytes),
                (Kind::Blob, DELTA) => (&mut s.delta_blobs, &mut s.delta_bytes),
                (Kind::Blob, _) => (&mut s.chunked_blobs, &mut s.chunked_bytes),
            };
            *count += 1;
            *bytes += len;
        }
        Ok(s)
    }
}

/// Remove the temporary files of writers that are gone (a crash or a kill). A name starts with the
/// writer's process id.
fn remove_orphans(tmp: &Path) {
    let Ok(entries) = fs::read_dir(tmp) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let pid = name.trim_start_matches("config-").split('-').next().and_then(|p| p.parse::<i32>().ok());
        let alive = match pid.and_then(rustix::process::Pid::from_raw) {
            Some(pid) => match rustix::process::test_kill_process(pid) {
                Ok(()) => true,
                Err(e) => e != rustix::io::Errno::SRCH,
            },
            None => true,
        };
        if !alive {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Rename `tmp` to `dst` unless `dst` exists (one system call); where the file system has no
/// such rename, link and unlink (two). `AlreadyExists` when `dst` is there.
fn move_new(tmp: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use rustix::fs::{renameat_with, RenameFlags, CWD};
        match renameat_with(CWD, tmp, CWD, dst, RenameFlags::NOREPLACE) {
            Err(e) if e == rustix::io::Errno::INVAL || e == rustix::io::Errno::NOTSUP => {}
            other => return other.map_err(io::Error::from),
        }
    }
    fs::hard_link(tmp, dst)?;
    let _ = fs::remove_file(tmp);
    Ok(())
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let got = f.read(&mut buf[n..])?;
        if got == 0 {
            break;
        }
        n += got;
    }
    Ok(n)
}

/// The fan-out folders of an area.
fn fan_folders(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for fan in fs::read_dir(dir)? {
        let fan = fan?;
        if fan.file_type()?.is_dir() {
            out.push(fan.path());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{noise, scratch};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_are_idempotent() {
        let dir = scratch("idem");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let file = dir.join("f");
        fs::write(&file, b"same content").unwrap();
        let (a, _) = store.put_file(&file, Temperature::Hot, None).unwrap();
        let first = store.added.objects.load(Ordering::Relaxed);
        let (b, _) = store.put_file(&file, Temperature::Hot, None).unwrap();
        assert_eq!(a, b);
        assert_eq!(store.added.objects.load(Ordering::Relaxed), first, "a second write adds nothing");
        assert_eq!(a, Keying::Plain.id(Kind::Blob, b"same content"));
        assert_eq!(store.read_blob(&a).unwrap(), b"same content");
        let (c, _) = store.put_cold_bytes(b"same content", None).unwrap();
        assert_eq!(a, c, "a cold write of a hot blob adds nothing");
        assert_eq!(store.added.objects.load(Ordering::Relaxed), first);
        assert!(fs::read_dir(dir.join("store/tmp")).unwrap().next().is_none(), "no temporary file is left");
        // A writer that is gone left a temporary file: the next open removes it.
        fs::write(dir.join("store/tmp/999999999-0"), b"partial").unwrap();
        fs::write(dir.join(format!("store/tmp/{}-77", std::process::id())), b"in progress").unwrap();
        Store::open(&dir.join("store"), Keying::Plain).unwrap();
        assert!(!dir.join("store/tmp/999999999-0").exists());
        assert!(dir.join(format!("store/tmp/{}-77", std::process::id())).exists());
        // The format is fixed for a store.
        assert!(Store::open(&dir.join("store"), Keying::Keyed([7; 32])).is_err());
    }

    #[test]
    fn a_delta_round_trips_and_is_small() {
        let dir = scratch("delta");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let v1 = noise(200_000, 1);
        let mut v2 = v1.clone();
        v2.splice(100_000..100_010, b"a small edit".iter().copied());
        let (a, _) = store.put_cold_bytes(&v1, None).unwrap();
        let (b, _) = store.put_cold_bytes(&v2, Some(a)).unwrap();
        assert_eq!(store.form(&b).unwrap(), Form::Delta { base: a, depth: 1 });
        assert_eq!(store.read_blob(&b).unwrap(), v2);
        let s = store.stat().unwrap();
        assert_eq!(s.delta_blobs, 1);
        assert!(s.delta_bytes < 1000, "delta of a small edit: {} bytes", s.delta_bytes);
    }

    #[test]
    fn delta_chains_stop_at_the_depth_limit() {
        let dir = scratch("depth");
        let mut store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        store.options.max_depth = 3;
        let mut data = noise(10_000, 3);
        let (mut prev, _) = store.put_cold_bytes(&data, None).unwrap();
        let mut depths = Vec::new();
        for i in 0..6u8 {
            data[i as usize * 100] ^= 0xff;
            let (id, _) = store.put_cold_bytes(&data, Some(prev)).unwrap();
            depths.push(match store.form(&id).unwrap() {
                Form::Delta { depth, .. } => depth,
                _ => 0,
            });
            assert_eq!(store.read_blob(&id).unwrap(), data);
            prev = id;
        }
        assert_eq!(depths, vec![1, 2, 3, 0, 1, 2]);
    }

    #[test]
    fn large_files_are_chunked_as_a_stream() {
        let dir = scratch("chunks");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let data = noise(5 << 20, 9);
        let file = dir.join("big");
        fs::write(&file, &data).unwrap();
        let (id, len) = store.put_file(&file, Temperature::Cold, None).unwrap();
        assert_eq!(len, data.len() as u64);
        assert_eq!(store.form(&id).unwrap(), Form::Chunked);
        assert_eq!(store.read_blob(&id).unwrap(), data);
        let s = store.stat().unwrap();
        // The stream cut the file where `split` cuts the whole file.
        assert_eq!(s.chunks, layr_store::chunks::split(&data).len() as u64);
        // The in-memory path gives the same chunks: an edit far from the start shares most of them.
        let mut edited = data.clone();
        edited[4 << 20] ^= 1;
        let before = s.chunks;
        let (other, _) = store.put_cold_bytes(&edited, None).unwrap();
        assert_eq!(store.read_blob(&other).unwrap(), edited);
        assert!(store.stat().unwrap().chunks - before <= 2);
    }

    #[test]
    fn corruption_is_found_on_read() {
        let dir = scratch("corrupt");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (id, _) = store.put_cold_bytes(b"original", None).unwrap();
        let path = store.object_path(&id);
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap();
        let other = layr_store::zst::compress(b"changed!", None).unwrap();
        let mut bytes = b"LOB1bz".to_vec();
        bytes.extend_from_slice(&other);
        fs::write(&path, bytes).unwrap();
        assert!(store.read_blob(&id).is_err());
    }
}
