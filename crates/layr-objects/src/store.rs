//! The object store on disk (docs/objects.md, "Layout").
//!
//! ```text
//! <root>/config.json         format version and id hash
//! <root>/blobs/ab/cdef...    hot blobs: the exact file bytes, so a checkout clones them
//! <root>/objects/ab/cdef...  encoded objects: trees, cold blobs (zstd, delta, chunk list), chunks
//! <root>/packs/<id>.pack     packed objects in zstd blocks, and <id>.idx (`crate::pack`)
//! <root>/states/<state>      the tree id of an imported state
//! <root>/tmp/                files being written
//! ```
//!
//! A write never replaces a file: it writes a temporary file and links it to its final name, and an
//! existing name means the object is already there. So writes are idempotent, concurrent writers of
//! the same object agree, and a reader never sees a partial object.

use crate::fsx;
use crate::id::{Id, Keying, Kind};
<<<<<<< head
use crate::pack::{self, Builder, Index, LazyIndex};
use crate::tree::{varint_into, Reader, Tree};
use anyhow::{bail, ensure, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

const MAGIC: &[u8; 4] = b"LOB1";
const WHOLE: u8 = b'z';
const DELTA: u8 = b'd';
const CHUNKED: u8 = b'c';
/// A whole small blob compressed with the store dictionary: dict-id:32, then the frame.
const WHOLE_DICT: u8 = b'y';
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
    /// A pack block holds about this many plain bytes: a read decodes one block.
    pub block_size: usize,
    /// A pack holds about this many plain bytes (so a compressed pack stays under 64 MiB, the
    /// largest single upload of Layr Cloud).
    pub pack_plain_max: usize,
    /// The zstd level of pack blocks: packs are written once and read many times.
    pub pack_level: i32,
    /// Long-distance matching in pack blocks (on for a block above the level-19 window of 8 MiB).
    pub pack_long: bool,
    /// A repack trains the store dictionary for small loose blobs when the store has none.
    pub train_dict: bool,
}

/// Whole blobs up to this size are dictionary samples in measurements (`small_blobs`).
pub const DICT_SAMPLE_MAX: usize = 16 << 10;

impl Default for Options {
    fn default() -> Self {
        Options {
            chunk_min_file: 1 << 20,
            delta_max: 64 << 20,
            max_depth: 16,
            // 4 MiB: a read decodes one block, 13 times faster than one stream per pack, for 4.2%
            // more bytes (docs/objects.md section 15).
            block_size: 4 << 20,
            pack_plain_max: 56 << 20,
            pack_level: 19,
            pack_long: true,
            train_dict: true,
        }
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
    pub times: u64,
    pub times_bytes: u64,
    pub states: u64,
    /// Packs: their count, objects, plain payload bytes, and file bytes (data and index).
    pub packs: u64,
    pub packed_objects: u64,
    pub packed_plain_bytes: u64,
    pub pack_bytes: u64,
    /// The store dictionary (`dicts/`), if a repack trained one.
    pub dict_bytes: u64,
}

impl Stat {
    /// The bytes of loose objects (counts and bytes above count loose files only).
    pub fn loose_bytes(&self) -> u64 {
        self.tree_bytes + self.whole_bytes + self.delta_bytes + self.chunked_bytes + self.chunk_bytes + self.times_bytes
    }
    pub fn total_bytes(&self) -> u64 {
        self.hot_bytes + self.loose_bytes() + self.pack_bytes + self.dict_bytes
    }
}

/// What an imported state names: its tree and its times table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateRef {
    pub tree: Id,
    pub times: Id,
}

/// What `Store::evict_hot` did.
#[derive(Debug, Default)]
pub struct EvictReport {
    pub blobs: u64,
    pub bytes: u64,
}

/// What `Store::pack` did.
#[derive(Debug, Default)]
pub struct PackReport {
    pub packs: u64,
    pub objects: u64,
    /// Loose files removed (packed now, or copies of packed objects).
    pub removed: u64,
    pub loose_bytes: u64,
    pub pack_bytes: u64,
}

/// An object as the store reads it, from a loose file or from a pack.
struct Payload {
    kind: Kind,
    /// `pack::RAW` (decoded content), `pack::DELTA` (delta body) or `pack::CHUNKED` (chunk list).
    form: u8,
    bytes: Vec<u8>,
}

struct LoadedPack {
    id: Id,
    data: File,
    /// Read at the first lookup that reaches this pack.
    index: OnceLock<LazyIndex>,
    index_path: PathBuf,
}

impl LoadedPack {
    fn index(&self) -> Result<&LazyIndex> {
        if let Some(i) = self.index.get() {
            return Ok(i);
        }
        let parsed = LazyIndex::parse(fs::read(&self.index_path)?, &self.id, self.data.metadata()?.len())
            .with_context(|| format!("pack {}", self.id))?;
        Ok(self.index.get_or_init(|| parsed))
    }
}

/// The packs of a store, read once and again when a lookup misses (another process packed).
#[derive(Default)]
struct PackSet {
    packs: Vec<Arc<LoadedPack>>,
}

impl PackSet {
    /// The pack that holds `id` and its entry. `last` is the pack of the previous lookup: objects
    /// read in path order are neighbors there, so most lookups search one index.
    fn find(&self, id: &Id, last: &AtomicU32) -> Result<Option<(u32, pack::Entry)>> {
        let hint = last.load(Ordering::Relaxed) as usize;
        if let Some(p) = self.packs.get(hint) {
            if let Some(e) = p.index()?.find(id)? {
                return Ok(Some((hint as u32, e)));
            }
        }
        for (n, p) in self.packs.iter().enumerate() {
            if n == hint {
                continue;
            }
            if let Some(e) = p.index()?.find(id)? {
                last.store(n as u32, Ordering::Relaxed);
                return Ok(Some((n as u32, e)));
            }
        }
        Ok(None)
    }
}

/// Recently decoded blocks: a checkout reads objects in path order, so neighbors share blocks.
#[derive(Default)]
struct BlockCache {
    blocks: HashMap<(Id, u32), Arc<Vec<u8>>>,
    order: VecDeque<(Id, u32)>,
    bytes: usize,
}

/// The decoded blocks a store keeps.
const CACHED_BYTES: usize = 512 << 20;

/// Whole blobs up to this size use the store dictionary in their loose frame.
pub const DICT_FILE_MAX: usize = 64 << 10;
/// The size of the store dictionary and the sample bytes it learns from.
pub const DICT_SIZE: usize = 112 << 10;
const DICT_SAMPLES_BYTES: usize = 16 << 20;

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
    packs: RwLock<Arc<PackSet>>,
    /// The pack of the last lookup (`PackSet::find`).
    last_pack: AtomicU32,
    cache: Mutex<BlockCache>,
    /// The store dictionary for small whole blobs (`dicts/<id>`), once a repack trained one.
    dict: RwLock<Option<(Id, Arc<Vec<u8>>)>>,
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
        for dir in ["blobs", "dicts", "objects", "packs", "states", "tmp"] {
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
        let store = Store {
            root: root.to_path_buf(),
            keying,
            options: Options::default(),
            added: Added::default(),
            clone_hook: None,
            packs: RwLock::new(Arc::new(PackSet::default())),
            last_pack: AtomicU32::new(0),
            cache: Mutex::new(BlockCache::default()),
            dict: RwLock::new(None),
        };
        store.reload_packs()?;
        store.load_dict()?;
        Ok(store)
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
        self.fan("blobs", id).exists() || self.has_object(id)
    }

    /// A loose or packed object (not a hot blob).
    fn has_object(&self, id: &Id) -> bool {
        self.object_path(id).exists() || matches!(self.pack_set().find(id, &self.last_pack), Ok(Some(_)))
    }

    fn pack_set(&self) -> Arc<PackSet> {
        self.packs.read().expect("no panic holds the lock").clone()
    }

    /// List the packs again; packs already open stay open. An index is read when a lookup first
    /// reaches its pack.
    fn reload_packs(&self) -> Result<Arc<PackSet>> {
        let old = self.pack_set();
        let mut set = PackSet::default();
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(self.root.join("packs"))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".idx") {
                names.push(stem.to_string());
            }
        }
        names.sort();
        for name in names {
            let id = Id::parse(&name)?;
            let pack = match old.packs.iter().find(|p| p.id == id) {
                Some(p) => p.clone(),
                None => {
                    let data_path = self.root.join("packs").join(format!("{name}.pack"));
                    let data = File::open(&data_path).with_context(|| format!("pack {name} has an index but no data"))?;
                    let index_path = self.root.join("packs").join(format!("{name}.idx"));
                    Arc::new(LoadedPack { id, data, index: OnceLock::new(), index_path })
                }
            };
            set.packs.push(pack);
        }
        let set = Arc::new(set);
        *self.packs.write().expect("no panic holds the lock") = set.clone();
        Ok(set)
    }

    /// The plain bytes of one block of a pack, checked against the index sizes.
    fn block(&self, set: &PackSet, pack: u32, block: u32) -> Result<Arc<Vec<u8>>> {
        let p = &set.packs[pack as usize];
        let key = (p.id, block);
        if let Some(b) = self.cache.lock().expect("no panic holds the lock").blocks.get(&key) {
            return Ok(b.clone());
        }
        let b = p.index()?.blocks[block as usize];
        let mut frame = vec![0u8; b.compressed as usize];
        p.data.read_exact_at(&mut frame, b.offset)?;
        let plain = crate::zstdx::decompress(&frame, None, b.plain as usize)
            .with_context(|| format!("block {block} of pack {}", p.id))?;
        let plain = Arc::new(plain);
        let mut cache = self.cache.lock().expect("no panic holds the lock");
        if cache.blocks.insert(key, plain.clone()).is_none() {
            cache.order.push_back(key);
            cache.bytes += plain.len();
            while cache.bytes > CACHED_BYTES && cache.order.len() > 1 {
                let Some(old) = cache.order.pop_front() else { break };
                if let Some(b) = cache.blocks.remove(&old) {
                    cache.bytes -= b.len();
                }
            }
        }
        Ok(plain)
    }

    /// An object from its loose file, else from a pack (the packs are read again once on a miss).
    fn payload(&self, id: &Id) -> Result<Payload> {
        match fs::read(self.object_path(id)) {
            Ok(data) => {
                ensure!(data.len() >= 6 && &data[..4] == MAGIC, "object {id} has no valid header");
                let kind = Kind::from_byte(data[4])?;
                return Ok(match data[5] {
                    WHOLE => Payload { kind, form: pack::RAW, bytes: layr_store::zst::decompress(&data[6..], None)? },
                    WHOLE_DICT => {
                        ensure!(data.len() > 6 + 32, "object {id} is cut short");
                        let dict_id = Id(data[6..38].try_into().expect("32 bytes"));
                        let dict = self.dict_bytes(&dict_id)?;
                        let frame = &data[38..];
                        let size = zstd_safe::get_frame_content_size(frame)
                            .ok()
                            .flatten()
                            .ok_or_else(|| anyhow::anyhow!("object {id} has a frame without its size"))?;
                        ensure!(size <= DICT_FILE_MAX as u64, "object {id} is too large for a dictionary frame");
                        Payload { kind, form: pack::RAW, bytes: crate::zstdx::decompress(frame, Some(&dict), size as usize)? }
                    }
                    DELTA => Payload { kind, form: pack::DELTA, bytes: data[6..].to_vec() },
                    CHUNKED => Payload { kind, form: pack::CHUNKED, bytes: data[6..].to_vec() },
                    other => bail!("object {id} has unknown form {other:#x}"),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot read object {id}")),
        }
        let mut set = self.pack_set();
        let mut found = set.find(id, &self.last_pack)?;
        if found.is_none() {
            set = self.reload_packs()?;
            found = set.find(id, &self.last_pack)?;
        }
        let Some((pack, e)) = found else { bail!("object {id} is missing") };
        let block = self.block(&set, pack, e.block)?;
        let bytes = block[e.offset as usize..(e.offset + e.length) as usize].to_vec();
        Ok(Payload { kind: e.kind, form: e.form, bytes })
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
        if self.has_object(id) {
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

    pub fn put_tree(&self, tree: &Tree) -> Result<Id> {
        let bytes = tree.encode();
        let id = self.keying.id(Kind::Tree, &bytes);
        if !self.has_object(&id) {
            let frame = layr_store::zst::compress(&bytes, None)?;
            self.write_object(&id, Kind::Tree, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    pub fn put_times(&self, times: &crate::times::Times) -> Result<Id> {
        let bytes = times.encode();
        let id = self.keying.id(Kind::Times, &bytes);
        if !self.has_object(&id) {
            let frame = layr_store::zst::compress(&bytes, None)?;
            self.write_object(&id, Kind::Times, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    pub fn get_times(&self, id: &Id) -> Result<crate::times::Times> {
        let p = self.payload(id)?;
        ensure!(p.kind == Kind::Times && p.form == pack::RAW, "object {id} is not a times table");
        ensure!(self.keying.id(Kind::Times, &p.bytes) == *id, "times {id} do not match their id");
        crate::times::Times::decode(&p.bytes)
    }

    pub fn get_tree(&self, id: &Id) -> Result<Tree> {
        let p = self.payload(id)?;
        ensure!(p.kind == Kind::Tree && p.form == pack::RAW, "object {id} is not a tree");
        ensure!(self.keying.id(Kind::Tree, &p.bytes) == *id, "tree {id} does not match its id");
        Tree::decode(&p.bytes)
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
        self.put_file_with(path, temperature, previous.as_slice(), false)
    }

    /// `put_file` with delta base candidates (`put_cold_with`).
    pub fn put_file_with(&self, path: &Path, temperature: Temperature, bases: &[Id], judge: bool) -> Result<(Id, u64)> {
        let (id, len) = self.hash_file(path)?;
        match temperature {
            Temperature::Hot if self.fan("blobs", &id).exists() => Ok((id, len)),
            Temperature::Hot => self.put_hot(path),
            Temperature::Cold if self.has(&id) => Ok((id, len)),
            Temperature::Cold => {
                if len < self.options.chunk_min_file || (!bases.is_empty() && len <= self.options.delta_max) {
                    self.put_cold_with(&fs::read(path)?, bases, judge)
                } else {
                    self.put_chunked_file(path)
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
        let p = self.payload(id).ok()?;
        if p.kind != Kind::Blob || p.form != pack::CHUNKED {
            return None;
        }
        let mut r = Reader::new(&p.bytes);
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
    /// limits; else one zstd frame (chunks for a large one).
    pub fn put_cold_bytes(&self, data: &[u8], previous: Option<Id>) -> Result<(Id, u64)> {
        self.put_cold_with(data, previous.as_slice(), false)
    }

    /// Store `data` cold against the best of `bases`: each usable base (stored, within the size
    /// and depth limits) gives a delta and the smallest wins. With `judge`, a whole frame competes
    /// too (bases found by similarity may not be similar).
    pub fn put_cold_with(&self, data: &[u8], bases: &[Id], judge: bool) -> Result<(Id, u64)> {
        let id = self.keying.id(Kind::Blob, data);
        let len = data.len() as u64;
        if self.has(&id) {
            return Ok((id, len));
        }
        let mut best: Option<(Vec<u8>, Id, u8)> = None;
        if len <= self.options.delta_max {
            for base_id in bases.iter().filter(|p| **p != id && self.has(p)) {
                if self.known_size(base_id).is_some_and(|n| n > self.options.delta_max) {
                    continue;
                }
                let depth = self.depth(base_id)?;
                if depth >= self.options.max_depth {
                    continue;
                }
                let base = self.read_blob(base_id)?;
                if base.len() as u64 > self.options.delta_max {
                    continue;
                }
                let frame = layr_store::zst::compress(data, Some(&base))?;
                if best.as_ref().is_none_or(|(b, _, _)| frame.len() < b.len()) {
                    best = Some((frame, *base_id, depth));
                }
            }
        }
        let whole = (best.is_none() || judge).then(|| self.whole_frame(data)).transpose()?;
        match (best, whole) {
            (Some((frame, base_id, depth)), w)
                if w.as_ref().is_none_or(|(_, prefix, w)| frame.len() < prefix.len() + w.len()) =>
            {
                self.write_object(&id, Kind::Blob, DELTA, &[&[depth + 1], &base_id.0, &frame])?;
            }
            _ if len >= self.options.chunk_min_file => self.put_chunked_bytes(id, data)?,
            (_, Some((form, prefix, w))) => {
                self.write_object(&id, Kind::Blob, form, &[&prefix, &w])?;
            }
            (_, None) => unreachable!("a whole frame exists when no delta wins"),
        }
        Ok((id, len))
    }

    /// The loose whole frame of a blob: with the store dictionary for a small blob once a repack
    /// trained one (form `y`, the dictionary id first), else plain (form `z`).
    fn whole_frame(&self, data: &[u8]) -> Result<(u8, Vec<u8>, Vec<u8>)> {
        if data.len() <= DICT_FILE_MAX {
            if let Some((dict_id, dict)) = self.dict.read().expect("no panic holds the lock").clone() {
                let codec = crate::zstdx::Codec { level: layr_store::zst::LEVEL, long: false, dict: Some(dict.to_vec()) };
                return Ok((WHOLE_DICT, dict_id.0.to_vec(), crate::zstdx::compress(data, &codec)?));
            }
        }
        Ok((WHOLE, Vec::new(), layr_store::zst::compress(data, None)?))
    }

    fn load_dict(&self) -> Result<()> {
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(self.root.join("dicts"))? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        // One dictionary per store; the first one written stays.
        if let Some(name) = names.first() {
            let id = Id::parse(name)?;
            let bytes = self.dict_bytes(&id)?;
            *self.dict.write().expect("no panic holds the lock") = Some((id, bytes));
        }
        Ok(())
    }

    fn dict_bytes(&self, id: &Id) -> Result<Arc<Vec<u8>>> {
        if let Some((have, bytes)) = self.dict.read().expect("no panic holds the lock").as_ref() {
            if have == id {
                return Ok(bytes.clone());
            }
        }
        let bytes = fs::read(self.root.join("dicts").join(id.hex())).with_context(|| format!("dictionary {id} is missing"))?;
        ensure!(self.keying.id(Kind::Opaque, &bytes) == *id, "dictionary {id} does not match its id");
        Ok(Arc::new(bytes))
    }

    /// Train the store dictionary on small whole blobs of `order`, unless the store has one.
    fn train_dict(&self, order: &[Id]) -> Result<Option<Id>> {
        if let Some((id, _)) = self.dict.read().expect("no panic holds the lock").as_ref() {
            return Ok(Some(*id));
        }
        let mut samples = Vec::new();
        let mut total = 0;
        for id in order {
            let p = self.payload(id)?;
            if p.kind == Kind::Blob && p.form == pack::RAW && !p.bytes.is_empty() && p.bytes.len() <= DICT_FILE_MAX {
                total += p.bytes.len();
                samples.push(p.bytes);
                if total >= DICT_SAMPLES_BYTES {
                    break;
                }
            }
        }
        if samples.len() < 64 {
            return Ok(None);
        }
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dict = crate::zstdx::train(&refs, DICT_SIZE)?;
        let id = self.keying.id(Kind::Opaque, &dict);
        let tmp = self.temp();
        fs::write(&tmp, &dict)?;
        self.publish(&tmp, &self.root.join("dicts").join(id.hex()))?;
        self.load_dict()?;
        Ok(Some(id))
    }

    fn put_chunk(&self, chunk: &[u8]) -> Result<Id> {
        let id = self.keying.id(Kind::Chunk, chunk);
        if !self.has_object(&id) {
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
        if !self.has_object(&id) {
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
        let p = self.payload(id)?;
        ensure!(p.kind == Kind::Blob, "object {id} is a {}, not a blob", p.kind.name());
        Ok(match p.form {
            pack::RAW => Form::Whole,
            pack::CHUNKED => Form::Chunked,
            _ => {
                ensure!(p.bytes.len() > 33, "delta object {id} is cut short");
                Form::Delta { depth: p.bytes[0], base: Id(p.bytes[1..33].try_into().expect("32 bytes")) }
            }
        })
    }

    /// The content of a blob, checked against its id.
    pub fn read_blob(&self, id: &Id) -> Result<Vec<u8>> {
        let data = self.read_blob_unchecked(id, 0, true)?;
        ensure!(self.keying.id(Kind::Blob, &data) == *id, "blob {id} does not match its id");
        Ok(data)
    }

    fn read_blob_unchecked(&self, id: &Id, depth: u32, hot: bool) -> Result<Vec<u8>> {
        ensure!(depth <= 255, "delta chain of {id} is too long");
        if let Some(p) = self.hot_path(id).filter(|_| hot) {
            return Ok(fs::read(p)?);
        }
        let Payload { kind, form, bytes: body } = self.payload(id)?;
        ensure!(kind == Kind::Blob, "object {id} is a {}, not a blob", kind.name());
        match form {
            pack::RAW => Ok(body),
            pack::DELTA => {
                ensure!(body.len() > 33, "delta object {id} is cut short");
                let base_id = Id(body[1..33].try_into().expect("32 bytes"));
                let base = self.read_blob_unchecked(&base_id, depth + 1, true)?;
                layr_store::zst::decompress(&body[33..], Some(&base))
            }
            _ => {
                let mut r = Reader::new(&body);
                let n = r.varint()?;
                let mut out = Vec::new();
                for _ in 0..n {
                    let chunk_id = r.id()?;
                    let len = r.varint()?;
                    let p = self.payload(&chunk_id)?;
                    ensure!(p.kind == Kind::Chunk && p.form == pack::RAW, "object {chunk_id} is not a chunk");
                    let chunk = p.bytes;
                    ensure!(chunk.len() as u64 == len, "chunk {chunk_id} has the wrong length");
                    ensure!(self.keying.id(Kind::Chunk, &chunk) == chunk_id, "chunk {chunk_id} does not match its id");
                    out.extend_from_slice(&chunk);
                }
                ensure!(r.done(), "bytes after the chunk list of {id}");
                Ok(out)
            }
        }
    }

    /// Turn the hot blobs that no tree of `keep` names into cold objects (a zstd frame, or chunks
    /// for a large one) and remove their files. A hot file goes only after its cold form reads
    /// back to the same id.
    pub fn evict_hot(&self, keep: &[Id]) -> Result<EvictReport> {
        use rayon::prelude::*;
        let mut hot = HashSet::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<Id> = keep.to_vec();
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            for e in self.get_tree(&t)?.entries {
                match e.node {
                    crate::tree::Node::Dir { tree } => stack.push(tree),
                    crate::tree::Node::File { blob, .. } => {
                        hot.insert(blob);
                    }
                    crate::tree::Node::Symlink { .. } => {}
                }
            }
        }
        let mut files = Vec::new();
        for dir in fan_folders(&self.root.join("blobs"))? {
            let fan = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if let Ok(id) = Id::parse(&format!("{fan}{}", entry.file_name().to_string_lossy())) {
                    if !hot.contains(&id) {
                        files.push((id, entry.path()));
                    }
                }
            }
        }
        let done = files
            .par_iter()
            .map(|(id, path)| -> Result<u64> {
                let len = fs::metadata(path)?.len();
                if !self.has_object(id) {
                    if len >= self.options.chunk_min_file {
                        let (got, _) = self.put_chunked_file(path)?;
                        ensure!(got == *id, "hot blob {id} does not match its id");
                    } else {
                        let data = fs::read(path)?;
                        ensure!(self.keying.id(Kind::Blob, &data) == *id, "hot blob {id} does not match its id");
                        let (form, prefix, frame) = self.whole_frame(&data)?;
                        self.write_object(id, Kind::Blob, form, &[&prefix, &frame])?;
                    }
                }
                let cold = self.read_blob_unchecked(id, 0, false)?;
                ensure!(self.keying.id(Kind::Blob, &cold) == *id, "the cold form of {id} does not match its id");
                fs::remove_file(path)?;
                Ok(len)
            })
            .collect::<Result<Vec<u64>>>()?;
        Ok(EvictReport { blobs: done.len() as u64, bytes: done.iter().sum() })
    }

    /// Record the tree and the times table of an imported state:
    /// `{"times":"<id>","tree":"<id>"}` (docs/objects.md, "States").
    pub fn set_state(&self, state: &str, r: &StateRef) -> Result<()> {
        ensure!(!state.is_empty() && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid state id {state}");
        let tmp = self.temp();
        fs::write(&tmp, format!("{{\"times\":\"{}\",\"tree\":\"{}\"}}\n", r.times, r.tree))?;
        fs::rename(&tmp, self.root.join("states").join(state))?;
        Ok(())
    }

    pub fn state_ref(&self, state: &str) -> Result<Option<StateRef>> {
        if state.is_empty() || !state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Ok(None);
        }
        let text = match fs::read_to_string(self.root.join("states").join(state)) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let field = |name: &str| -> Result<Id> {
            let key = format!("\"{name}\":\"");
            let at = text.find(&key).ok_or_else(|| anyhow::anyhow!("state {state} has no {name}"))? + key.len();
            Id::parse(text.get(at..at + 64).unwrap_or(""))
        };
        let r = StateRef { tree: field("tree")?, times: field("times")? };
        ensure!(text == format!("{{\"times\":\"{}\",\"tree\":\"{}\"}}\n", r.times, r.tree), "state {state} is not canonical");
        Ok(Some(r))
    }

    pub fn state_tree(&self, state: &str) -> Result<Option<Id>> {
        Ok(self.state_ref(state)?.map(|r| r.tree))
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
            s.times += p.times;
            s.times_bytes += p.times_bytes;
        }
        s.states = fs::read_dir(self.root.join("states"))?.count() as u64;
        for entry in fs::read_dir(self.root.join("dicts"))? {
            s.dict_bytes += entry?.metadata()?.len();
        }
        for p in &self.reload_packs()?.packs {
            s.packs += 1;
            let index = p.index()?;
            s.packed_objects += index.count as u64;
            for i in 0..index.count {
                s.packed_plain_bytes += u64::from(index.entry(i)?.length);
            }
            s.pack_bytes += p.data.metadata()?.len();
            s.pack_bytes += fs::metadata(&p.index_path)?.len();
        }
        Ok(s)
    }

    /// Move the loose objects into packs; hot blobs stay files for clones. A loose copy of an
    /// object that a pack holds goes. A run with no loose object changes nothing. A pack is
    /// complete on disk (its data checked against its id, its index written) before any loose
    /// file of it goes, so a crash leaves at most a loose copy too many.
    pub fn pack(&self) -> Result<PackReport> {
        use rayon::prelude::*;
        let set = self.reload_packs()?;
        let mut report = PackReport::default();
        let mut loose: HashMap<Id, u64> = HashMap::new();
        for dir in fan_folders(&self.root.join("objects"))? {
            let fan = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let Ok(id) = Id::parse(&format!("{fan}{}", entry.file_name().to_string_lossy())) else { continue };
                if set.find(&id, &self.last_pack)?.is_some() {
                    fs::remove_file(entry.path())?;
                    report.removed += 1;
                    continue;
                }
                loose.insert(id, entry.metadata()?.len());
            }
        }
        report.loose_bytes = loose.values().sum();
        let order = self.pack_order(&loose)?;
        if self.options.train_dict {
            self.train_dict(&order)?;
        }
        let codec = crate::zstdx::Codec { level: self.options.pack_level, long: self.options.pack_long, dict: None };
        // Full packs wait until there is one for each thread, then compress side by side: a pack of
        // one stream compresses on one thread.
        let mut builder = Builder::new(self.options.block_size);
        let mut full: Vec<Builder> = Vec::new();
        let flush = |full: &mut Vec<Builder>, report: &mut PackReport| -> Result<()> {
            let done =
                std::mem::take(full).into_par_iter().map(|b| self.write_pack(b, &codec)).collect::<Result<Vec<PackReport>>>()?;
            for d in done {
                report.packs += d.packs;
                report.objects += d.objects;
                report.removed += d.removed;
                report.pack_bytes += d.pack_bytes;
            }
            Ok(())
        };
        for batch in order.chunks(4096) {
            let payloads = batch.par_iter().map(|id| self.payload(id)).collect::<Result<Vec<Payload>>>()?;
            for (id, p) in batch.iter().zip(payloads) {
                builder.add(*id, p.kind, p.form, &p.bytes);
                if builder.plain_len() >= self.options.pack_plain_max {
                    full.push(std::mem::replace(&mut builder, Builder::new(self.options.block_size)));
                    if full.len() >= rayon::current_num_threads() {
                        flush(&mut full, &mut report)?;
                    }
                }
            }
        }
        if !builder.entries.is_empty() {
            full.push(builder);
        }
        flush(&mut full, &mut report)?;
        // Packs written side by side may have read the pack folder before each other's packs.
        self.reload_packs()?;
        Ok(report)
    }

    /// Whole blobs of at most `DICT_SAMPLE_MAX` bytes from `order`, up to `budget` bytes.
    #[doc(hidden)]
    pub fn small_blobs(&self, order: &[Id], budget: usize) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut total = 0;
        for id in order {
            let p = self.payload(id)?;
            if p.kind == Kind::Blob && p.form == pack::RAW && !p.bytes.is_empty() && p.bytes.len() <= DICT_SAMPLE_MAX {
                total += p.bytes.len();
                out.push(p.bytes);
                if total >= budget {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The loose objects in pack order and their payloads in plain blocks of `block_size`, as a
    /// repack would make them (for measurements).
    #[doc(hidden)]
    pub fn plain_blocks(&self, block_size: usize) -> Result<(Vec<Id>, Vec<Vec<u8>>)> {
        use rayon::prelude::*;
        let mut loose: HashMap<Id, u64> = HashMap::new();
        for dir in fan_folders(&self.root.join("objects"))? {
            let fan = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if let Ok(id) = Id::parse(&format!("{fan}{}", entry.file_name().to_string_lossy())) {
                    loose.insert(id, entry.metadata()?.len());
                }
            }
        }
        let order = self.pack_order(&loose)?;
        let mut builder = Builder::new(block_size);
        for batch in order.chunks(4096) {
            let payloads = batch.par_iter().map(|id| self.payload(id)).collect::<Result<Vec<Payload>>>()?;
            for (id, p) in batch.iter().zip(payloads) {
                builder.add(*id, p.kind, p.form, &p.bytes);
            }
        }
        Ok((order, builder.plain))
    }

    /// Pack order: the trees of the imported states first (they hold ids, which do not compress),
    /// then their blobs and chunks in path order, so files of one folder share blocks; then the
    /// loose objects that no imported state names.
    fn pack_order(&self, loose: &HashMap<Id, u64>) -> Result<Vec<Id>> {
        let mut states: Vec<String> = fs::read_dir(self.root.join("states"))?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_>>()?;
        states.sort();
        let mut seen = HashSet::new();
        let (mut trees, mut others) = (Vec::new(), Vec::new());
        for state in states {
            if let Some(r) = self.state_ref(&state)? {
                self.order_tree(&r.tree, loose, &mut seen, &mut trees, &mut others)?;
                self.order_times(&r.times, loose, &mut seen, &mut trees)?;
            }
        }
        let mut rest: Vec<Id> = loose.keys().filter(|id| !seen.contains(*id)).copied().collect();
        rest.sort();
        trees.extend(others);
        trees.extend(rest);
        Ok(trees)
    }

    /// A times table and the tables below it. A table already in a pack ends the walk: a table
    /// is packed with or after the tables it names.
    fn order_times(&self, id: &Id, loose: &HashMap<Id, u64>, seen: &mut HashSet<Id>, out: &mut Vec<Id>) -> Result<()> {
        if !loose.contains_key(id) || !seen.insert(*id) {
            return Ok(());
        }
        out.push(*id);
        for r in self.get_times(id)?.records {
            if let crate::times::Record::Folder(child) = r {
                self.order_times(&child, loose, seen, out)?;
            }
        }
        Ok(())
    }

    /// The times table `root` and every table below it (what a transfer of a state must send).
    pub fn times_ids(&self, root: &Id) -> Result<Vec<Id>> {
        let mut out = vec![*root];
        let mut next = 0;
        let mut seen: HashSet<Id> = HashSet::from([*root]);
        while next < out.len() {
            for r in self.get_times(&out[next])?.records {
                if let crate::times::Record::Folder(child) = r {
                    if seen.insert(child) {
                        out.push(child);
                    }
                }
            }
            next += 1;
        }
        Ok(out)
||||||| base
=======
use crate::pack::{self, Builder, Index};
use crate::tree::{varint_into, Reader, Tree};
use anyhow::{bail, ensure, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const MAGIC: &[u8; 4] = b"LOB1";
const WHOLE: u8 = b'z';
const DELTA: u8 = b'd';
const CHUNKED: u8 = b'c';
/// A whole small blob compressed with the store dictionary: dict-id:32, then the frame.
const WHOLE_DICT: u8 = b'y';
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
    /// A pack block holds about this many plain bytes: a read decodes one block.
    pub block_size: usize,
    /// A pack holds about this many plain bytes (so a compressed pack stays under 64 MiB, the
    /// largest single upload of Layr Cloud).
    pub pack_plain_max: usize,
    /// The zstd level of pack blocks: packs are written once and read many times.
    pub pack_level: i32,
    /// Long-distance matching in pack blocks (on for a block above the level-19 window of 8 MiB).
    pub pack_long: bool,
    /// A repack trains the store dictionary for small loose blobs when the store has none.
    pub train_dict: bool,
}

/// Whole blobs up to this size are dictionary samples in measurements (`small_blobs`).
pub const DICT_SAMPLE_MAX: usize = 16 << 10;

impl Default for Options {
    fn default() -> Self {
        Options {
            chunk_min_file: 1 << 20,
            delta_max: 64 << 20,
            max_depth: 16,
            // One zstd stream per pack (docs/objects.md section 15).
            block_size: 56 << 20,
            pack_plain_max: 56 << 20,
            pack_level: 19,
            pack_long: true,
            train_dict: true,
        }
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
    pub times: u64,
    pub times_bytes: u64,
    pub states: u64,
    /// Packs: their count, objects, plain payload bytes, and file bytes (data and index).
    pub packs: u64,
    pub packed_objects: u64,
    pub packed_plain_bytes: u64,
    pub pack_bytes: u64,
    /// The store dictionary (`dicts/`), if a repack trained one.
    pub dict_bytes: u64,
}

impl Stat {
    /// The bytes of loose objects (counts and bytes above count loose files only).
    pub fn loose_bytes(&self) -> u64 {
        self.tree_bytes + self.whole_bytes + self.delta_bytes + self.chunked_bytes + self.chunk_bytes + self.times_bytes
    }
    pub fn total_bytes(&self) -> u64 {
        self.hot_bytes + self.loose_bytes() + self.pack_bytes + self.dict_bytes
    }
}

/// What an imported state names: its tree and its times table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateRef {
    pub tree: Id,
    pub times: Id,
}

/// What `Store::evict_hot` did.
#[derive(Debug, Default)]
pub struct EvictReport {
    pub blobs: u64,
    pub bytes: u64,
}

/// What `Store::pack` did.
#[derive(Debug, Default)]
pub struct PackReport {
    pub packs: u64,
    pub objects: u64,
    /// Loose files removed (packed now, or copies of packed objects).
    pub removed: u64,
    pub loose_bytes: u64,
    pub pack_bytes: u64,
}

/// An object as the store reads it, from a loose file or from a pack.
struct Payload {
    kind: Kind,
    /// `pack::RAW` (decoded content), `pack::DELTA` (delta body) or `pack::CHUNKED` (chunk list).
    form: u8,
    bytes: Vec<u8>,
}

struct LoadedPack {
    index: Index,
    data: File,
}

/// The packs of a store, read once and again when a lookup misses (another process packed).
#[derive(Default)]
struct PackSet {
    packs: Vec<LoadedPack>,
    names: HashSet<Id>,
    /// Object id to (pack, entry).
    map: HashMap<Id, (u32, u32)>,
}

/// Recently decoded blocks: a checkout reads objects in path order, so neighbors share blocks.
#[derive(Default)]
struct BlockCache {
    blocks: HashMap<(Id, u32), Arc<Vec<u8>>>,
    order: VecDeque<(Id, u32)>,
    bytes: usize,
}

/// The decoded blocks a store keeps (a pack of one stream is one block of up to 56 MiB).
const CACHED_BYTES: usize = 512 << 20;

/// Whole blobs up to this size use the store dictionary in their loose frame.
pub const DICT_FILE_MAX: usize = 64 << 10;
/// The size of the store dictionary and the sample bytes it learns from.
pub const DICT_SIZE: usize = 112 << 10;
const DICT_SAMPLES_BYTES: usize = 16 << 20;

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
    packs: RwLock<Arc<PackSet>>,
    cache: Mutex<BlockCache>,
    /// The store dictionary for small whole blobs (`dicts/<id>`), once a repack trained one.
    dict: RwLock<Option<(Id, Arc<Vec<u8>>)>>,
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
        for dir in ["blobs", "dicts", "objects", "packs", "states", "tmp"] {
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
        let store = Store {
            root: root.to_path_buf(),
            keying,
            options: Options::default(),
            added: Added::default(),
            clone_hook: None,
            packs: RwLock::new(Arc::new(PackSet::default())),
            cache: Mutex::new(BlockCache::default()),
            dict: RwLock::new(None),
        };
        store.reload_packs()?;
        store.load_dict()?;
        Ok(store)
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
        self.fan("blobs", id).exists() || self.has_object(id)
    }

    /// A loose or packed object (not a hot blob).
    fn has_object(&self, id: &Id) -> bool {
        self.object_path(id).exists() || self.pack_set().map.contains_key(id)
    }

    fn pack_set(&self) -> Arc<PackSet> {
        self.packs.read().expect("no panic holds the lock").clone()
    }

    /// Read the pack indexes again; packs already open stay open.
    fn reload_packs(&self) -> Result<Arc<PackSet>> {
        let old = self.pack_set();
        let mut set = PackSet::default();
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(self.root.join("packs"))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".idx") {
                names.push(stem.to_string());
            }
        }
        names.sort();
        for name in names {
            let id = Id::parse(&name)?;
            let data_path = self.root.join("packs").join(format!("{name}.pack"));
            let data = File::open(&data_path).with_context(|| format!("pack {name} has an index but no data"))?;
            let index = match old.packs.iter().find(|p| p.index.pack == id) {
                Some(p) => p.index.clone(),
                None => {
                    let bytes = fs::read(self.root.join("packs").join(format!("{name}.idx")))?;
                    let index = Index::decode(&bytes, data.metadata()?.len()).with_context(|| format!("pack {name}"))?;
                    ensure!(index.pack == id, "pack index {name} names pack {}", index.pack);
                    index
                }
            };
            let n = set.packs.len() as u32;
            for (i, e) in index.entries.iter().enumerate() {
                set.map.entry(e.id).or_insert((n, i as u32));
            }
            set.names.insert(id);
            set.packs.push(LoadedPack { index, data });
        }
        let set = Arc::new(set);
        *self.packs.write().expect("no panic holds the lock") = set.clone();
        Ok(set)
    }

    /// The plain bytes of one block of a pack, checked against the index sizes.
    fn block(&self, set: &PackSet, pack: u32, block: u32) -> Result<Arc<Vec<u8>>> {
        let p = &set.packs[pack as usize];
        let key = (p.index.pack, block);
        if let Some(b) = self.cache.lock().expect("no panic holds the lock").blocks.get(&key) {
            return Ok(b.clone());
        }
        let b = p.index.blocks[block as usize];
        let mut frame = vec![0u8; b.compressed as usize];
        p.data.read_exact_at(&mut frame, b.offset)?;
        let plain = crate::zstdx::decompress(&frame, None, b.plain as usize)
            .with_context(|| format!("block {block} of pack {}", p.index.pack))?;
        let plain = Arc::new(plain);
        let mut cache = self.cache.lock().expect("no panic holds the lock");
        if cache.blocks.insert(key, plain.clone()).is_none() {
            cache.order.push_back(key);
            cache.bytes += plain.len();
            while cache.bytes > CACHED_BYTES && cache.order.len() > 1 {
                let Some(old) = cache.order.pop_front() else { break };
                if let Some(b) = cache.blocks.remove(&old) {
                    cache.bytes -= b.len();
                }
            }
        }
        Ok(plain)
    }

    /// An object from its loose file, else from a pack (the packs are read again once on a miss).
    fn payload(&self, id: &Id) -> Result<Payload> {
        match fs::read(self.object_path(id)) {
            Ok(data) => {
                ensure!(data.len() >= 6 && &data[..4] == MAGIC, "object {id} has no valid header");
                let kind = Kind::from_byte(data[4])?;
                return Ok(match data[5] {
                    WHOLE => Payload { kind, form: pack::RAW, bytes: layr_store::zst::decompress(&data[6..], None)? },
                    WHOLE_DICT => {
                        ensure!(data.len() > 6 + 32, "object {id} is cut short");
                        let dict_id = Id(data[6..38].try_into().expect("32 bytes"));
                        let dict = self.dict_bytes(&dict_id)?;
                        let frame = &data[38..];
                        let size = zstd_safe::get_frame_content_size(frame)
                            .ok()
                            .flatten()
                            .ok_or_else(|| anyhow::anyhow!("object {id} has a frame without its size"))?;
                        ensure!(size <= DICT_FILE_MAX as u64, "object {id} is too large for a dictionary frame");
                        Payload { kind, form: pack::RAW, bytes: crate::zstdx::decompress(frame, Some(&dict), size as usize)? }
                    }
                    DELTA => Payload { kind, form: pack::DELTA, bytes: data[6..].to_vec() },
                    CHUNKED => Payload { kind, form: pack::CHUNKED, bytes: data[6..].to_vec() },
                    other => bail!("object {id} has unknown form {other:#x}"),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot read object {id}")),
        }
        let mut set = self.pack_set();
        if !set.map.contains_key(id) {
            set = self.reload_packs()?;
        }
        let Some(&(pack, entry)) = set.map.get(id) else { bail!("object {id} is missing") };
        let e = set.packs[pack as usize].index.entries[entry as usize];
        let block = self.block(&set, pack, e.block)?;
        let bytes = block[e.offset as usize..(e.offset + e.length) as usize].to_vec();
        Ok(Payload { kind: e.kind, form: e.form, bytes })
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
        if self.has_object(id) {
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

    pub fn put_tree(&self, tree: &Tree) -> Result<Id> {
        let bytes = tree.encode();
        let id = self.keying.id(Kind::Tree, &bytes);
        if !self.has_object(&id) {
            let frame = layr_store::zst::compress(&bytes, None)?;
            self.write_object(&id, Kind::Tree, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    pub fn put_times(&self, times: &crate::times::Times) -> Result<Id> {
        let bytes = times.encode();
        let id = self.keying.id(Kind::Times, &bytes);
        if !self.has_object(&id) {
            let frame = layr_store::zst::compress(&bytes, None)?;
            self.write_object(&id, Kind::Times, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    pub fn get_times(&self, id: &Id) -> Result<crate::times::Times> {
        let p = self.payload(id)?;
        ensure!(p.kind == Kind::Times && p.form == pack::RAW, "object {id} is not a times table");
        ensure!(self.keying.id(Kind::Times, &p.bytes) == *id, "times {id} do not match their id");
        crate::times::Times::decode(&p.bytes)
    }

    pub fn get_tree(&self, id: &Id) -> Result<Tree> {
        let p = self.payload(id)?;
        ensure!(p.kind == Kind::Tree && p.form == pack::RAW, "object {id} is not a tree");
        ensure!(self.keying.id(Kind::Tree, &p.bytes) == *id, "tree {id} does not match its id");
        Tree::decode(&p.bytes)
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
        self.put_file_with(path, temperature, previous.as_slice(), false)
    }

    /// `put_file` with delta base candidates (`put_cold_with`).
    pub fn put_file_with(&self, path: &Path, temperature: Temperature, bases: &[Id], judge: bool) -> Result<(Id, u64)> {
        let (id, len) = self.hash_file(path)?;
        match temperature {
            Temperature::Hot if self.fan("blobs", &id).exists() => Ok((id, len)),
            Temperature::Hot => self.put_hot(path),
            Temperature::Cold if self.has(&id) => Ok((id, len)),
            Temperature::Cold => {
                if len < self.options.chunk_min_file || (!bases.is_empty() && len <= self.options.delta_max) {
                    self.put_cold_with(&fs::read(path)?, bases, judge)
                } else {
                    self.put_chunked_file(path)
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
        let p = self.payload(id).ok()?;
        if p.kind != Kind::Blob || p.form != pack::CHUNKED {
            return None;
        }
        let mut r = Reader::new(&p.bytes);
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
    /// limits; else one zstd frame (chunks for a large one).
    pub fn put_cold_bytes(&self, data: &[u8], previous: Option<Id>) -> Result<(Id, u64)> {
        self.put_cold_with(data, previous.as_slice(), false)
    }

    /// Store `data` cold against the best of `bases`: each usable base (stored, within the size
    /// and depth limits) gives a delta and the smallest wins. With `judge`, a whole frame competes
    /// too (bases found by similarity may not be similar).
    pub fn put_cold_with(&self, data: &[u8], bases: &[Id], judge: bool) -> Result<(Id, u64)> {
        let id = self.keying.id(Kind::Blob, data);
        let len = data.len() as u64;
        if self.has(&id) {
            return Ok((id, len));
        }
        let mut best: Option<(Vec<u8>, Id, u8)> = None;
        if len <= self.options.delta_max {
            for base_id in bases.iter().filter(|p| **p != id && self.has(p)) {
                if self.known_size(base_id).is_some_and(|n| n > self.options.delta_max) {
                    continue;
                }
                let depth = self.depth(base_id)?;
                if depth >= self.options.max_depth {
                    continue;
                }
                let base = self.read_blob(base_id)?;
                if base.len() as u64 > self.options.delta_max {
                    continue;
                }
                let frame = layr_store::zst::compress(data, Some(&base))?;
                if best.as_ref().is_none_or(|(b, _, _)| frame.len() < b.len()) {
                    best = Some((frame, *base_id, depth));
                }
            }
        }
        let whole = (best.is_none() || judge).then(|| self.whole_frame(data)).transpose()?;
        match (best, whole) {
            (Some((frame, base_id, depth)), w)
                if w.as_ref().is_none_or(|(_, prefix, w)| frame.len() < prefix.len() + w.len()) =>
            {
                self.write_object(&id, Kind::Blob, DELTA, &[&[depth + 1], &base_id.0, &frame])?;
            }
            _ if len >= self.options.chunk_min_file => self.put_chunked_bytes(id, data)?,
            (_, Some((form, prefix, w))) => {
                self.write_object(&id, Kind::Blob, form, &[&prefix, &w])?;
            }
            (_, None) => unreachable!("a whole frame exists when no delta wins"),
        }
        Ok((id, len))
    }

    /// The loose whole frame of a blob: with the store dictionary for a small blob once a repack
    /// trained one (form `y`, the dictionary id first), else plain (form `z`).
    fn whole_frame(&self, data: &[u8]) -> Result<(u8, Vec<u8>, Vec<u8>)> {
        if data.len() <= DICT_FILE_MAX {
            if let Some((dict_id, dict)) = self.dict.read().expect("no panic holds the lock").clone() {
                let codec = crate::zstdx::Codec { level: layr_store::zst::LEVEL, long: false, dict: Some(dict.to_vec()) };
                return Ok((WHOLE_DICT, dict_id.0.to_vec(), crate::zstdx::compress(data, &codec)?));
            }
        }
        Ok((WHOLE, Vec::new(), layr_store::zst::compress(data, None)?))
    }

    fn load_dict(&self) -> Result<()> {
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(self.root.join("dicts"))? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        // One dictionary per store; the first one written stays.
        if let Some(name) = names.first() {
            let id = Id::parse(name)?;
            let bytes = self.dict_bytes(&id)?;
            *self.dict.write().expect("no panic holds the lock") = Some((id, bytes));
        }
        Ok(())
    }

    fn dict_bytes(&self, id: &Id) -> Result<Arc<Vec<u8>>> {
        if let Some((have, bytes)) = self.dict.read().expect("no panic holds the lock").as_ref() {
            if have == id {
                return Ok(bytes.clone());
            }
        }
        let bytes = fs::read(self.root.join("dicts").join(id.hex())).with_context(|| format!("dictionary {id} is missing"))?;
        ensure!(self.keying.id(Kind::Opaque, &bytes) == *id, "dictionary {id} does not match its id");
        Ok(Arc::new(bytes))
    }

    /// Train the store dictionary on small whole blobs of `order`, unless the store has one.
    fn train_dict(&self, order: &[Id]) -> Result<Option<Id>> {
        if let Some((id, _)) = self.dict.read().expect("no panic holds the lock").as_ref() {
            return Ok(Some(*id));
        }
        let mut samples = Vec::new();
        let mut total = 0;
        for id in order {
            let p = self.payload(id)?;
            if p.kind == Kind::Blob && p.form == pack::RAW && !p.bytes.is_empty() && p.bytes.len() <= DICT_FILE_MAX {
                total += p.bytes.len();
                samples.push(p.bytes);
                if total >= DICT_SAMPLES_BYTES {
                    break;
                }
            }
        }
        if samples.len() < 64 {
            return Ok(None);
        }
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dict = crate::zstdx::train(&refs, DICT_SIZE)?;
        let id = self.keying.id(Kind::Opaque, &dict);
        let tmp = self.temp();
        fs::write(&tmp, &dict)?;
        self.publish(&tmp, &self.root.join("dicts").join(id.hex()))?;
        self.load_dict()?;
        Ok(Some(id))
    }

    fn put_chunk(&self, chunk: &[u8]) -> Result<Id> {
        let id = self.keying.id(Kind::Chunk, chunk);
        if !self.has_object(&id) {
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
        if !self.has_object(&id) {
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
        let p = self.payload(id)?;
        ensure!(p.kind == Kind::Blob, "object {id} is a {}, not a blob", p.kind.name());
        Ok(match p.form {
            pack::RAW => Form::Whole,
            pack::CHUNKED => Form::Chunked,
            _ => {
                ensure!(p.bytes.len() > 33, "delta object {id} is cut short");
                Form::Delta { depth: p.bytes[0], base: Id(p.bytes[1..33].try_into().expect("32 bytes")) }
            }
        })
    }

    /// The content of a blob, checked against its id.
    pub fn read_blob(&self, id: &Id) -> Result<Vec<u8>> {
        let data = self.read_blob_unchecked(id, 0, true)?;
        ensure!(self.keying.id(Kind::Blob, &data) == *id, "blob {id} does not match its id");
        Ok(data)
    }

    fn read_blob_unchecked(&self, id: &Id, depth: u32, hot: bool) -> Result<Vec<u8>> {
        ensure!(depth <= 255, "delta chain of {id} is too long");
        if let Some(p) = self.hot_path(id).filter(|_| hot) {
            return Ok(fs::read(p)?);
        }
        let Payload { kind, form, bytes: body } = self.payload(id)?;
        ensure!(kind == Kind::Blob, "object {id} is a {}, not a blob", kind.name());
        match form {
            pack::RAW => Ok(body),
            pack::DELTA => {
                ensure!(body.len() > 33, "delta object {id} is cut short");
                let base_id = Id(body[1..33].try_into().expect("32 bytes"));
                let base = self.read_blob_unchecked(&base_id, depth + 1, true)?;
                layr_store::zst::decompress(&body[33..], Some(&base))
            }
            _ => {
                let mut r = Reader::new(&body);
                let n = r.varint()?;
                let mut out = Vec::new();
                for _ in 0..n {
                    let chunk_id = r.id()?;
                    let len = r.varint()?;
                    let p = self.payload(&chunk_id)?;
                    ensure!(p.kind == Kind::Chunk && p.form == pack::RAW, "object {chunk_id} is not a chunk");
                    let chunk = p.bytes;
                    ensure!(chunk.len() as u64 == len, "chunk {chunk_id} has the wrong length");
                    ensure!(self.keying.id(Kind::Chunk, &chunk) == chunk_id, "chunk {chunk_id} does not match its id");
                    out.extend_from_slice(&chunk);
                }
                ensure!(r.done(), "bytes after the chunk list of {id}");
                Ok(out)
            }
        }
    }

    /// Turn the hot blobs that no tree of `keep` names into cold objects (a zstd frame, or chunks
    /// for a large one) and remove their files. A hot file goes only after its cold form reads
    /// back to the same id.
    pub fn evict_hot(&self, keep: &[Id]) -> Result<EvictReport> {
        use rayon::prelude::*;
        let mut hot = HashSet::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<Id> = keep.to_vec();
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            for e in self.get_tree(&t)?.entries {
                match e.node {
                    crate::tree::Node::Dir { tree } => stack.push(tree),
                    crate::tree::Node::File { blob, .. } => {
                        hot.insert(blob);
                    }
                    crate::tree::Node::Symlink { .. } => {}
                }
            }
        }
        let mut files = Vec::new();
        for dir in fan_folders(&self.root.join("blobs"))? {
            let fan = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if let Ok(id) = Id::parse(&format!("{fan}{}", entry.file_name().to_string_lossy())) {
                    if !hot.contains(&id) {
                        files.push((id, entry.path()));
                    }
                }
            }
        }
        let done = files
            .par_iter()
            .map(|(id, path)| -> Result<u64> {
                let len = fs::metadata(path)?.len();
                if !self.has_object(id) {
                    if len >= self.options.chunk_min_file {
                        let (got, _) = self.put_chunked_file(path)?;
                        ensure!(got == *id, "hot blob {id} does not match its id");
                    } else {
                        let data = fs::read(path)?;
                        ensure!(self.keying.id(Kind::Blob, &data) == *id, "hot blob {id} does not match its id");
                        let (form, prefix, frame) = self.whole_frame(&data)?;
                        self.write_object(id, Kind::Blob, form, &[&prefix, &frame])?;
                    }
                }
                let cold = self.read_blob_unchecked(id, 0, false)?;
                ensure!(self.keying.id(Kind::Blob, &cold) == *id, "the cold form of {id} does not match its id");
                fs::remove_file(path)?;
                Ok(len)
            })
            .collect::<Result<Vec<u64>>>()?;
        Ok(EvictReport { blobs: done.len() as u64, bytes: done.iter().sum() })
    }

    /// Record the tree and the times table of an imported state:
    /// `{"times":"<id>","tree":"<id>"}` (docs/objects.md, "States").
    pub fn set_state(&self, state: &str, r: &StateRef) -> Result<()> {
        ensure!(!state.is_empty() && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid state id {state}");
        let tmp = self.temp();
        fs::write(&tmp, format!("{{\"times\":\"{}\",\"tree\":\"{}\"}}\n", r.times, r.tree))?;
        fs::rename(&tmp, self.root.join("states").join(state))?;
        Ok(())
    }

    pub fn state_ref(&self, state: &str) -> Result<Option<StateRef>> {
        if state.is_empty() || !state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Ok(None);
        }
        let text = match fs::read_to_string(self.root.join("states").join(state)) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let field = |name: &str| -> Result<Id> {
            let key = format!("\"{name}\":\"");
            let at = text.find(&key).ok_or_else(|| anyhow::anyhow!("state {state} has no {name}"))? + key.len();
            Id::parse(text.get(at..at + 64).unwrap_or(""))
        };
        let r = StateRef { tree: field("tree")?, times: field("times")? };
        ensure!(text == format!("{{\"times\":\"{}\",\"tree\":\"{}\"}}\n", r.times, r.tree), "state {state} is not canonical");
        Ok(Some(r))
    }

    pub fn state_tree(&self, state: &str) -> Result<Option<Id>> {
        Ok(self.state_ref(state)?.map(|r| r.tree))
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
            s.times += p.times;
            s.times_bytes += p.times_bytes;
        }
        s.states = fs::read_dir(self.root.join("states"))?.count() as u64;
        for entry in fs::read_dir(self.root.join("dicts"))? {
            s.dict_bytes += entry?.metadata()?.len();
        }
        for p in &self.reload_packs()?.packs {
            s.packs += 1;
            s.packed_objects += p.index.entries.len() as u64;
            s.packed_plain_bytes += p.index.entries.iter().map(|e| u64::from(e.length)).sum::<u64>();
            s.pack_bytes += p.data.metadata()?.len();
            s.pack_bytes += fs::metadata(self.root.join("packs").join(format!("{}.idx", p.index.pack)))?.len();
        }
        Ok(s)
    }

    /// Move the loose objects into packs; hot blobs stay files for clones. A loose copy of an
    /// object that a pack holds goes. A run with no loose object changes nothing. A pack is
    /// complete on disk (its data checked against its id, its index written) before any loose
    /// file of it goes, so a crash leaves at most a loose copy too many.
    pub fn pack(&self) -> Result<PackReport> {
        use rayon::prelude::*;
        let set = self.reload_packs()?;
        let mut report = PackReport::default();
        let mut loose: HashMap<Id, u64> = HashMap::new();
        for dir in fan_folders(&self.root.join("objects"))? {
            let fan = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let Ok(id) = Id::parse(&format!("{fan}{}", entry.file_name().to_string_lossy())) else { continue };
                if set.map.contains_key(&id) {
                    fs::remove_file(entry.path())?;
                    report.removed += 1;
                    continue;
                }
                loose.insert(id, entry.metadata()?.len());
            }
        }
        report.loose_bytes = loose.values().sum();
        let order = self.pack_order(&loose)?;
        if self.options.train_dict {
            self.train_dict(&order)?;
        }
        let codec = crate::zstdx::Codec { level: self.options.pack_level, long: self.options.pack_long, dict: None };
        // Full packs wait until there is one for each thread, then compress side by side: a pack of
        // one stream compresses on one thread.
        let mut builder = Builder::new(self.options.block_size);
        let mut full: Vec<Builder> = Vec::new();
        let flush = |full: &mut Vec<Builder>, report: &mut PackReport| -> Result<()> {
            let done =
                std::mem::take(full).into_par_iter().map(|b| self.write_pack(b, &codec)).collect::<Result<Vec<PackReport>>>()?;
            for d in done {
                report.packs += d.packs;
                report.objects += d.objects;
                report.removed += d.removed;
                report.pack_bytes += d.pack_bytes;
            }
            Ok(())
        };
        for batch in order.chunks(4096) {
            let payloads = batch.par_iter().map(|id| self.payload(id)).collect::<Result<Vec<Payload>>>()?;
            for (id, p) in batch.iter().zip(payloads) {
                builder.add(*id, p.kind, p.form, &p.bytes);
                if builder.plain_len() >= self.options.pack_plain_max {
                    full.push(std::mem::replace(&mut builder, Builder::new(self.options.block_size)));
                    if full.len() >= rayon::current_num_threads() {
                        flush(&mut full, &mut report)?;
                    }
                }
            }
        }
        if !builder.entries.is_empty() {
            full.push(builder);
        }
        flush(&mut full, &mut report)?;
        // Packs written side by side may have read the pack folder before each other's packs.
        self.reload_packs()?;
        Ok(report)
    }

    /// Whole blobs of at most `DICT_SAMPLE_MAX` bytes from `order`, up to `budget` bytes.
    #[doc(hidden)]
    pub fn small_blobs(&self, order: &[Id], budget: usize) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut total = 0;
        for id in order {
            let p = self.payload(id)?;
            if p.kind == Kind::Blob && p.form == pack::RAW && !p.bytes.is_empty() && p.bytes.len() <= DICT_SAMPLE_MAX {
                total += p.bytes.len();
                out.push(p.bytes);
                if total >= budget {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The loose objects in pack order and their payloads in plain blocks of `block_size`, as a
    /// repack would make them (for measurements).
    #[doc(hidden)]
    pub fn plain_blocks(&self, block_size: usize) -> Result<(Vec<Id>, Vec<Vec<u8>>)> {
        use rayon::prelude::*;
        let mut loose: HashMap<Id, u64> = HashMap::new();
        for dir in fan_folders(&self.root.join("objects"))? {
            let fan = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if let Ok(id) = Id::parse(&format!("{fan}{}", entry.file_name().to_string_lossy())) {
                    loose.insert(id, entry.metadata()?.len());
                }
            }
        }
        let order = self.pack_order(&loose)?;
        let mut builder = Builder::new(block_size);
        for batch in order.chunks(4096) {
            let payloads = batch.par_iter().map(|id| self.payload(id)).collect::<Result<Vec<Payload>>>()?;
            for (id, p) in batch.iter().zip(payloads) {
                builder.add(*id, p.kind, p.form, &p.bytes);
            }
        }
        Ok((order, builder.plain))
    }

    /// Pack order: the trees of the imported states first (they hold ids, which do not compress),
    /// then their blobs and chunks in path order, so files of one folder share blocks; then the
    /// loose objects that no imported state names.
    fn pack_order(&self, loose: &HashMap<Id, u64>) -> Result<Vec<Id>> {
        let mut states: Vec<String> = fs::read_dir(self.root.join("states"))?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_>>()?;
        states.sort();
        let mut seen = HashSet::new();
        let (mut trees, mut others) = (Vec::new(), Vec::new());
        for state in states {
            if let Some(tree) = self.state_tree(&state)? {
                self.order_tree(&tree, loose, &mut seen, &mut trees, &mut others)?;
            }
        }
        let mut rest: Vec<Id> = loose.keys().filter(|id| !seen.contains(*id)).copied().collect();
        rest.sort();
        trees.extend(others);
        trees.extend(rest);
        Ok(trees)
>>>>>>> cloud-v1-client
    }

    fn order_tree(
        &self,
        id: &Id,
        loose: &HashMap<Id, u64>,
        seen: &mut HashSet<Id>,
        trees: &mut Vec<Id>,
        others: &mut Vec<Id>,
    ) -> Result<()> {
        if !seen.insert(*id) {
            return Ok(());
        }
        if loose.contains_key(id) {
            trees.push(*id);
        }
        for e in self.get_tree(id)?.entries {
            match e.node {
                crate::tree::Node::Dir { tree } => self.order_tree(&tree, loose, seen, trees, others)?,
                crate::tree::Node::File { blob, .. } if loose.contains_key(&blob) && seen.insert(blob) => {
                    others.push(blob);
                    let p = self.payload(&blob)?;
                    if p.form == pack::CHUNKED {
                        let mut r = Reader::new(&p.bytes);
                        for _ in 0..r.varint()? {
                            let chunk = r.id()?;
                            r.varint()?;
                            if loose.contains_key(&chunk) && seen.insert(chunk) {
                                others.push(chunk);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn write_pack(&self, builder: Builder, codec: &crate::zstdx::Codec) -> Result<PackReport> {
        let mut report = PackReport::default();
        use rayon::prelude::*;
        let Builder { plain, mut entries, .. } = builder;
        let frames = plain.par_iter().map(|b| crate::zstdx::compress(b, codec)).collect::<Result<Vec<_>>>()?;
        // Each frame decodes to its block before anything is written.
        frames.par_iter().zip(plain.par_iter()).try_for_each(|(f, p)| -> Result<()> {
            ensure!(crate::zstdx::decompress(f, None, p.len())? == *p, "a pack block does not decode to its input");
            Ok(())
        })?;
        let mut data = Vec::with_capacity(frames.iter().map(Vec::len).sum::<usize>() + 4);
        data.extend_from_slice(pack::DATA_MAGIC);
        let mut blocks = Vec::with_capacity(frames.len());
        for (f, p) in frames.iter().zip(&plain) {
            blocks.push(pack::Block { offset: data.len() as u64, compressed: f.len() as u32, plain: p.len() as u32 });
            data.extend_from_slice(f);
        }
        let id = self.keying.id(Kind::Opaque, &data);
        entries.sort_by_key(|e| e.id);
        let index = Index { pack: id, blocks, entries };
        let index_bytes = index.encode();
        let dir = self.root.join("packs");
        for (bytes, name) in [(&data, format!("{id}.pack")), (&index_bytes, format!("{id}.idx"))] {
            let tmp = self.temp();
            let written = (|| -> Result<()> {
                let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o444).open(&tmp)?;
                f.write_all(bytes)?;
                f.sync_all()?;
                Ok(())
            })();
            if let Err(e) = written {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            // An existing pack with this id holds the same bytes.
            self.publish(&tmp, &dir.join(name))?;
        }
        File::open(&dir)?.sync_all()?;
        // The data on disk is the data the index describes.
        let mut on_disk = Vec::new();
        File::open(dir.join(format!("{id}.pack")))?.read_to_end(&mut on_disk)?;
        ensure!(self.keying.id(Kind::Opaque, &on_disk) == id, "pack {id} on disk does not match its id");
        self.reload_packs()?;
        for e in &index.entries {
            match fs::remove_file(self.object_path(&e.id)) {
                Ok(()) => report.removed += 1,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        report.packs += 1;
        report.objects += index.entries.len() as u64;
        report.pack_bytes += (data.len() + index_bytes.len()) as u64;
        Ok(report)
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
                (Kind::Blob, WHOLE | WHOLE_DICT) => (&mut s.whole_blobs, &mut s.whole_bytes),
                (Kind::Blob, DELTA) => (&mut s.delta_blobs, &mut s.delta_bytes),
                (Kind::Blob, _) => (&mut s.chunked_blobs, &mut s.chunked_bytes),
                (Kind::Times, _) => (&mut s.times, &mut s.times_bytes),
                (Kind::Opaque, _) => bail!("{} is an opaque object, which is never loose", path.display()),
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
