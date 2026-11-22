//! The object store on disk (docs/objects.md, "Layout").
//!
//! ```text
//! <root>/config.json         format version and id hash
//! <root>/blobs/ab/cdef...    hot blobs: the exact file bytes, so a checkout clones them
//! <root>/objects/ab/cdef...  encoded objects: trees, cold blobs (zstd, delta, chunk list), chunks
//! <root>/packs/<id>.pack     packed objects in zstd blocks, and <id>.idx (`crate::pack`)
//! <root>/states/<state>      the tree id of an imported state
//! <root>/tmp/                files being written
//! <root>/damaged/            damaged files that a repair moved aside (`Store::quarantine`)
//! ```
//!
//! A write never replaces a valid file: it writes and syncs a temporary file and links it to its final
//! name, and an existing valid name means the object is already there. So writes are idempotent,
//! concurrent writers of the same object agree, and a crash leaves no empty or partial object behind a
//! name. A file that a crash damaged anyway (an older version wrote without a sync) counts as absent:
//! the next write of that object replaces it (`check_loose`), and `damage` lists it for fsck.

use crate::fsx;
use crate::id::{Id, Keying, Kind};
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

mod maintenance;
pub use maintenance::{Damage, Damaged, MaintenanceLimit, MaintenancePlan};

use layr_object_format::loose::{self, Header, CHUNKED, DELTA, WHOLE, WHOLE_DICT};
/// Project storage v1 uses snapshots; v2 uses content-addressed objects.
pub const FORMAT_VERSION: u64 = 2;

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
            // 256 KiB: git reads files of the object store through the git pack folder, and
            // small blocks make `git log -p` 3 times and `blame` 2 times faster than 4 MiB blocks,
            // for 10% more pack bytes (docs/objects.md section 15).
            block_size: 256 << 10,
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

/// What a maintenance plan removes: state records, loose objects and hot blobs, and their bytes.
#[derive(Debug, Default)]
pub struct PruneReport {
    pub states: u64,
    pub objects: u64,
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
    /// Packs of an older, larger block size that `Store::reblock` wrote again.
    pub reblocked: u64,
    /// Loose objects whose body did not decode: left loose.
    pub unreadable: Vec<String>,
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
        self.index_if_present()?.ok_or_else(|| anyhow::anyhow!("pack {} has no index", self.id))
    }

    /// The index, or None when the index file went before this process read it: a reblock of
    /// another process wrote the objects into new packs first.
    fn index_if_present(&self) -> Result<Option<&LazyIndex>> {
        if let Some(i) = self.index.get() {
            return Ok(Some(i));
        }
        let bytes = match fs::read(&self.index_path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("pack {}", self.id)),
        };
        let parsed = damage_of(|| {
            LazyIndex::parse(bytes, &self.id, self.data.metadata()?.len()).with_context(|| format!("pack {}", self.id))
        })?;
        Ok(Some(self.index.get_or_init(|| parsed)))
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
            if let Some(e) = p.index_if_present()?.map(|i| i.find(id)).transpose()?.flatten() {
                return Ok(Some((hint as u32, e)));
            }
        }
        for (n, p) in self.packs.iter().enumerate() {
            if n == hint {
                continue;
            }
            if let Some(e) = p.index_if_present()?.map(|i| i.find(id)).transpose()?.flatten() {
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
    // Shared for this handle's lifetime: imports and their temporary wire objects remain
    // protected until their consumer finishes. Pruning opens an exclusive lease instead.
    prune_lease: File,
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
    /// Objects this handle found readable with all they need (`object_problem`), so a command checks
    /// each at most once. Only this handle removes objects while it lives (pruning excludes other
    /// handles), and it forgets them when it does.
    readable: Mutex<HashSet<Id>>,
}

/// One stored form of a blob, read from the header of its object file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    Hot,
    Whole,
    Delta { base: Id, depth: u8 },
    Chunked,
}

// Experimental object stores used v1 for the same encoding. Only their marker changes.
fn upgrade_config(root: &Path, config: &str) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = root.join(format!(".config-upgrade-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let result = (|| -> Result<()> {
        file.write_all(config.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, root.join("config.json"))?;
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    result
}

impl Store {
    /// Open the store at `root`, made if absent. The id hash of an existing store must match.
    pub fn open(root: &Path, keying: Keying) -> Result<Store> {
        let config = root.join("config.json");
        let want = format!("{{\"ids\":\"{}\",\"v\":{FORMAT_VERSION}}}\n", keying.name());
        let legacy = format!("{{\"ids\":\"{}\",\"v\":1}}\n", keying.name());
        match fs::read_to_string(&config) {
            Ok(text) if text == legacy => upgrade_config(root, &want)?,
            Ok(text) => ensure!(text == want, "the object store at {} has another format: {}", root.display(), text.trim()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(root.join("tmp")).with_context(|| format!("cannot make {}", root.join("tmp").display()))?;
                let tmp = root.join("tmp").join(format!("config-{}", std::process::id()));
                // A file of an earlier process with this id is stale (`remove_orphans` runs later).
                let _ = fs::remove_file(&tmp);
                write_synced(&tmp, &[want.as_bytes()], 0o644)?;
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
        // Check an existing format before creating directories or removing temporary files.
        for dir in ["blobs", "dicts", "objects", "packs", "states", "tmp"] {
            fs::create_dir_all(root.join(dir)).with_context(|| format!("cannot make {}", root.join(dir).display()))?;
        }
        remove_orphans(&root.join("tmp"));
        let lease = Self::prune_lease(root)?;
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockShared)?;
        Self::load(root, keying, lease)
    }

    /// Open an existing store without initialization, cleanup, or a config upgrade.
    pub fn open_existing(root: &Path, keying: Keying) -> Result<Store> {
        let lease = Self::prune_lease(root)?;
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockShared)?;
        Self::open_leased(root, keying, lease)
    }

    /// `open_existing` without the shared prune lease for the handle's lifetime: a long-lived
    /// reader (the server of a fast line) takes `lease` around each write, so a prune can run
    /// between them. It reads only objects of states that the project keeps.
    pub fn open_existing_unleased(root: &Path, keying: Keying) -> Result<Store> {
        let lease = Self::prune_lease(root)?;
        Self::open_leased(root, keying, lease)
    }

    /// A shared prune lease until the returned file is dropped (`open_existing_unleased`).
    pub fn lease(&self) -> Result<File> {
        let lease = Self::prune_lease(&self.root)?;
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockShared)?;
        Ok(lease)
    }

    fn open_leased(root: &Path, keying: Keying, lease: File) -> Result<Store> {
        let config = fs::read_to_string(root.join("config.json"))?;
        let current = format!("{{\"ids\":\"{}\",\"v\":{FORMAT_VERSION}}}\n", keying.name());
        let legacy = format!("{{\"ids\":\"{}\",\"v\":1}}\n", keying.name());
        ensure!(
            config == current || config == legacy,
            "the object store at {} has another format: {}",
            root.display(),
            config.trim()
        );
        Self::load(root, keying, lease)
    }

    fn load(root: &Path, keying: Keying, prune_lease: File) -> Result<Store> {
        let store = Store {
            root: root.to_path_buf(),
            prune_lease,
            keying,
            options: Options::default(),
            added: Added::default(),
            clone_hook: None,
            packs: RwLock::new(Arc::new(PackSet::default())),
            last_pack: AtomicU32::new(0),
            cache: Mutex::new(BlockCache::default()),
            dict: RwLock::new(None),
            readable: Mutex::new(HashSet::new()),
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

    /// Open a hot blob atomically with respect to cooling. Once opened, its descriptor
    /// remains usable after unlink; a vanished path means the verified cold form is ready.
    pub fn open_hot(&self, id: &Id) -> io::Result<Option<File>> {
        if let Some(dir) = std::env::var_os("LAYR_TEST_HOT_OPEN_BARRIER") {
            let dir = PathBuf::from(dir);
            // ast-grep-ignore: object-store-synced-writes (a test barrier, not a store file)
            fs::write(dir.join("ready"), b"")?;
            let start = std::time::Instant::now();
            while !dir.join("release").exists() && start.elapsed() < std::time::Duration::from_secs(5) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        match File::open(self.fan("blobs", id)) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The whole file of a hot blob, if the store has one.
    /// Where the hot copy of a blob would be, without checking that it is there.
    pub fn hot_candidate(&self, id: &Id) -> PathBuf {
        self.fan("blobs", id)
    }

    pub fn hot_path(&self, id: &Id) -> Option<PathBuf> {
        let p = self.fan("blobs", id);
        p.is_file().then_some(p)
    }

    fn object_path(&self, id: &Id) -> PathBuf {
        self.fan("objects", id)
    }

    /// A hot blob, or an object that reads with what its form needs (`object_problem`), for readers.
    pub fn has(&self, id: &Id) -> bool {
        matches!(self.object_problem(id, true), Ok(None))
    }

    /// List the packs again, as a read does after a miss: a store open since before another one packed its loose
    /// objects then finds them (`has` does not list the packs again).
    pub fn refresh_packs(&self) -> Result<()> {
        self.reload_packs().map(drop)
    }

    /// Whether a writer can reuse `id` (a hot blob counts with `hot`): it reads with what its form
    /// needs. A damaged object, or one whose chunk, delta base or dictionary is missing, is absent, so
    /// the write makes it again; an I/O error stops the write.
    fn present(&self, id: &Id, hot: bool) -> Result<bool> {
        Ok(self.object_problem(id, hot)?.is_none())
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
                    let data = match File::open(&data_path) {
                        Ok(f) => f,
                        // A reblock removes the index first, then the data.
                        Err(e) if e.kind() == io::ErrorKind::NotFound && !self.root.join("packs").join(format!("{name}.idx")).exists() => {
                            continue
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            return Err(e).with_context(|| format!("pack {name} has an index but no data"))
                        }
                        Err(e) => return Err(e).with_context(|| format!("open pack {name}")),
                    };
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
            Ok(data) => return damage_of(|| self.loose_payload(id, &data)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot read object {id}")),
        }
        let mut set = self.pack_set();
        let mut found = set.find(id, &self.last_pack)?;
        if found.is_none() {
            set = self.reload_packs()?;
            found = set.find(id, &self.last_pack)?;
        }
        let Some((pack, e)) = found else { return Err(unreadable(format!("object {id} is missing"))) };
        self.packed(&set, pack, &e)
    }

    /// The payload of a pack entry.
    fn packed(&self, set: &PackSet, pack: u32, e: &pack::Entry) -> Result<Payload> {
        let block = damage_of(|| self.block(set, pack, e.block))?;
        let bytes = block[e.offset as usize..(e.offset + e.length) as usize].to_vec();
        Ok(Payload { kind: e.kind, form: e.form, bytes })
    }

    fn loose_payload(&self, id: &Id, data: &[u8]) -> Result<Payload> {
        {
            {
                let header = Header::decode(data).with_context(|| format!("object {id} has no valid header"))?;
                let kind = header.kind;
                Ok(match header.form {
                    loose::Form::Whole => {
                        Payload { kind, form: pack::RAW, bytes: layr_store::zst::decompress(&data[6..], None)? }
                    }
                    loose::Form::Dictionary => {
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
                    loose::Form::Delta => Payload { kind, form: pack::DELTA, bytes: data[6..].to_vec() },
                    loose::Form::Chunked => Payload { kind, form: pack::CHUNKED, bytes: data[6..].to_vec() },
                })
            }
        }
    }

    /// A unique temporary path in `tmp/`.
    fn temp(&self) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        self.root.join("tmp").join(format!("{}-{n}", std::process::id()))
    }

    /// A new temporary file holding `parts`, synced to disk before any name points at it.
    fn write_temp(&self, parts: &[&[u8]], mode: u32) -> Result<PathBuf> {
        let tmp = self.temp();
        write_synced(&tmp, parts, mode)?;
        Ok(tmp)
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
        if self.present(id, false)? {
            return Ok(false);
        }
        let header = Header { kind, form: loose::Form::from_byte(form)? }.encode();
        let parts: Vec<&[u8]> = std::iter::once(header.as_slice()).chain(body.iter().copied()).collect();
        let n = parts.iter().map(|p| p.len() as u64).sum();
        let tmp = self.write_temp(&parts, 0o444)?;
        let replace = match check_loose(&dst) {
            Ok(None) => false,
            // A damaged file of this object (#452), or one that needs what is missing, is replaced. A
            // valid copy that another writer published since the check reads the same, so replacing
            // it is harmless.
            Ok(Some(_)) => true,
            Err(error) if is_unreadable(error.as_ref()) => true,
            // An I/O error is no damage: nothing is replaced.
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                return Err(error);
            }
        };
        let added = if replace {
            if let Err(e) = fs::rename(&tmp, &dst) {
                let _ = fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("cannot store {}", dst.display()));
            }
            true
        } else {
            self.publish(&tmp, &dst)?
        };
        if added {
            self.added.objects.fetch_add(1, Ordering::Relaxed);
            self.added.bytes.fetch_add(n, Ordering::Relaxed);
        }
        Ok(added)
    }

    pub fn put_tree(&self, tree: &Tree) -> Result<Id> {
        let bytes = tree.encode();
        let id = self.keying.id(Kind::Tree, &bytes);
        if !self.present(&id, false)? {
            let frame = layr_store::zst::compress(&bytes, None)?;
            self.write_object(&id, Kind::Tree, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    pub fn put_times(&self, times: &crate::times::Times) -> Result<Id> {
        let bytes = times.encode();
        let id = self.keying.id(Kind::Times, &bytes);
        if !self.present(&id, false)? {
            let frame = layr_store::zst::compress(&bytes, None)?;
            self.write_object(&id, Kind::Times, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    /// Raw times bytes, with type and id checked before a source-specific decoder uses them.
    pub fn times_bytes(&self, id: &Id) -> Result<Vec<u8>> {
        let p = self.payload(id)?;
        damage_of(|| {
            ensure!(p.kind == Kind::Times && p.form == pack::RAW, "object {id} is not a times table");
            ensure!(self.keying.id(Kind::Times, &p.bytes) == *id, "times {id} do not match their id");
            Ok(())
        })?;
        Ok(p.bytes)
    }

    pub fn get_times(&self, id: &Id) -> Result<crate::times::Times> {
        Ok(crate::times::Times::decode(&self.times_bytes(id)?)?)
    }

    #[cfg(test)]
    pub(crate) fn put_times_unchecked_for_test(&self, bytes: &[u8]) -> Result<Id> {
        let id = self.keying.id(Kind::Times, bytes);
        if !self.present(&id, false)? {
            let frame = layr_store::zst::compress(bytes, None)?;
            self.write_object(&id, Kind::Times, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    /// Store a canonical flat Cloud V1 times table without interpreting it as a local table.
    pub fn put_cloud_times(&self, bytes: &[u8]) -> Result<Id> {
        crate::cloud_times::validate(bytes)?;
        let id = self.keying.id(Kind::Times, bytes);
        if !self.present(&id, false)? {
            let frame = layr_store::zst::compress(bytes, None)?;
            self.write_object(&id, Kind::Times, WHOLE, &[&frame])?;
        }
        Ok(id)
    }

    pub fn get_tree(&self, id: &Id) -> Result<Tree> {
        let p = self.payload(id)?;
        damage_of(|| {
            ensure!(p.kind == Kind::Tree && p.form == pack::RAW, "object {id} is not a tree");
            ensure!(self.keying.id(Kind::Tree, &p.bytes) == *id, "tree {id} does not match its id");
            Ok(Tree::decode(&p.bytes)?)
        })
    }

    /// The id of the file at `path`, read once.
    pub fn hash_file(&self, path: &Path) -> Result<(Id, u64)> {
        let mut f = File::open(path)?;
        let len = f.metadata()?.len();
        Ok((self.hash_open_file(&mut f, len).with_context(|| path.display().to_string())?, len))
    }

    /// Hash an already-open file with the length observed on that descriptor.
    pub fn hash_open_file(&self, f: &mut File, len: u64) -> Result<Id> {
        self.hash_reader(f, len)
    }

    /// Hash a core reader with the object's existing keying and reusable scratch.
    pub fn hash_reader(&self, f: &mut impl std::io::Read, len: u64) -> Result<Id> {
        thread_local! {
            static BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        BUFFER.with_borrow_mut(|buffer| {
            let buf = fsx::file_buffer(buffer, len);
            Ok(self.keying.hash_reader(Kind::Blob, len, f, buf)?)
        })
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
            Temperature::Cold if self.present(&id, true)? => Ok((id, len)),
            Temperature::Cold => {
                if len < self.options.chunk_min_file || (!bases.is_empty() && len <= self.options.delta_max) {
                    self.put_cold_with(&fs::read(path)?, bases, judge)
                } else {
                    self.put_chunked_file(path)
                }
            }
        }
    }

    /// Store `data` as a hot blob (a file in `blobs/`) and return its id.
    pub fn put_hot_bytes(&self, data: &[u8]) -> Result<(Id, u64)> {
        let id = self.keying.id(Kind::Blob, data);
        let len = data.len() as u64;
        if self.fan("blobs", &id).exists() {
            return Ok((id, len));
        }
        let tmp = self.temp();
        if let Err(e) = fs::write(&tmp, data) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        if self.publish(&tmp, &self.fan("blobs", &id))? {
            self.added.objects.fetch_add(1, Ordering::Relaxed);
            self.added.bytes.fetch_add(len, Ordering::Relaxed);
        }
        Ok((id, len))
    }

    /// Stream a representation-independent file through the existing object publication path.
    pub fn put_reader_with(
        &self,
        reader: &mut dyn std::io::Read,
        temperature: Temperature,
        bases: &[Id],
        judge: bool,
    ) -> Result<(Id, u64)> {
        use std::os::unix::fs::OpenOptionsExt;
        let temporary = self.temp();
        let result = (|| {
            let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
            std::io::copy(reader, &mut file)?;
            drop(file);
            self.put_file_with(&temporary, temperature, bases, judge)
        })();
        let cleanup = fs::remove_file(&temporary);
        let result = result?;
        cleanup?;
        Ok(result)
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
            // The copy or clone reaches the disk before its name does (#452).
            rustix::fs::fsync(File::open(&tmp)?)?;
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
        Some(chunk_list(&p.bytes).ok()?.iter().fold(0u64, |total, (_, n)| total.saturating_add(*n)))
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
        if self.present(&id, true)? {
            return Ok((id, len));
        }
        let mut best: Option<(Vec<u8>, Id, u8)> = None;
        if len <= self.options.delta_max {
            for base_id in bases.iter().filter(|p| **p != id) {
                if !self.present(base_id, true)? {
                    continue;
                }
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
        // One dictionary per store; the first one written that reads stays. A damaged one does not
        // keep the store from opening: fsck reports it (`damage`).
        for name in names {
            let Ok(id) = Id::parse(&name) else { continue };
            match self.dict_bytes(&id) {
                Ok(bytes) => {
                    *self.dict.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id, bytes));
                    break;
                }
                Err(error) if is_unreadable(error.as_ref()) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn dict_bytes(&self, id: &Id) -> Result<Arc<Vec<u8>>> {
        if let Some((have, bytes)) = self.dict.read().expect("no panic holds the lock").as_ref() {
            if have == id {
                return Ok(bytes.clone());
            }
        }
        damage_of(|| {
            let bytes =
                fs::read(self.root.join("dicts").join(id.hex())).with_context(|| format!("dictionary {id} is missing"))?;
            ensure!(self.keying.id(Kind::Opaque, &bytes) == *id, "dictionary {id} does not match its id");
            Ok(Arc::new(bytes))
        })
    }

    /// Train the store dictionary on small whole blobs of `order`, unless the store has one.
    fn train_dict(&self, order: &[Id]) -> Result<Option<Id>> {
        if let Some((id, _)) = self.dict.read().expect("no panic holds the lock").as_ref() {
            return Ok(Some(*id));
        }
        let mut samples = Vec::new();
        let mut total = 0;
        for id in order {
            // An object that does not decode is no sample (the repack reports it).
            let Ok(p) = self.payload(id) else { continue };
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
        let tmp = self.write_temp(&[&dict], 0o444)?;
        self.publish(&tmp, &self.root.join("dicts").join(id.hex()))?;
        self.load_dict()?;
        Ok(Some(id))
    }

    fn put_chunk(&self, chunk: &[u8]) -> Result<Id> {
        let id = self.keying.id(Kind::Chunk, chunk);
        if !self.present(&id, false)? {
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
        if !self.present(&id, false)? {
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
        damage_of(|| {
            ensure!(self.keying.id(Kind::Blob, &data) == *id, "blob {id} does not match its id");
            Ok(())
        })?;
        Ok(data)
    }

    fn read_blob_unchecked(&self, id: &Id, depth: u32, hot: bool) -> Result<Vec<u8>> {
        ensure!(depth <= 255, "delta chain of {id} is too long");
        if hot {
            if let Some(mut file) = self.open_hot(id)? {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                return Ok(bytes);
            }
        }
        let Payload { kind, form, bytes: body } = self.payload(id)?;
        damage_of(|| {
            ensure!(kind == Kind::Blob, "object {id} is a {}, not a blob", kind.name());
            ensure!(form != pack::DELTA || body.len() > 33, "delta object {id} is cut short");
            Ok(())
        })?;
        match form {
            pack::RAW => Ok(body),
            pack::DELTA => {
                let base_id = Id(body[1..33].try_into().expect("32 bytes"));
                let base = self.read_blob_unchecked(&base_id, depth + 1, true)?;
                damage_of(|| layr_store::zst::decompress(&body[33..], Some(&base)))
            }
            _ => {
                let mut out = Vec::new();
                for (chunk_id, len) in damage_of(|| chunk_list(&body))? {
                    let p = self.payload(&chunk_id)?;
                    damage_of(|| {
                        ensure!(p.kind == Kind::Chunk && p.form == pack::RAW, "object {chunk_id} is not a chunk");
                        ensure!(p.bytes.len() as u64 == len, "chunk {chunk_id} has the wrong length");
                        ensure!(self.keying.id(Kind::Chunk, &p.bytes) == chunk_id, "chunk {chunk_id} does not match its id");
                        Ok(())
                    })?;
                    out.extend_from_slice(&p.bytes);
                }
                Ok(out)
            }
        }
    }

    /// Record the tree and the times table of an imported state:
    /// `{"times":"<id>","tree":"<id>"}` (docs/objects.md, "States").
    pub fn set_state(&self, state: &str, r: &StateRef) -> Result<()> {
        ensure!(!state.is_empty() && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid state id {state}");
        let text = format!("{{\"times\":\"{}\",\"tree\":\"{}\"}}\n", r.times, r.tree);
        let tmp = self.write_temp(&[text.as_bytes()], 0o644)?;
        let dir = self.root.join("states");
        if let Err(e) = fs::rename(&tmp, dir.join(state)) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        // The record of the state names it next: its name must be on disk first.
        rustix::fs::fsync(File::open(&dir)?)?;
        Ok(())
    }

    /// `set_state` for a new state only: fails when `state` exists, also when another process
    /// records it at the same time (a hard link does not replace its target).
    pub fn create_state(&self, state: &str, r: &StateRef) -> Result<()> {
        ensure!(!state.is_empty() && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid state id {state}");
        let tmp = self.temp();
        fs::write(&tmp, format!("{{\"times\":\"{}\",\"tree\":\"{}\"}}\n", r.times, r.tree))?;
        let linked = fs::hard_link(&tmp, self.root.join("states").join(state));
        let _ = fs::remove_file(&tmp);
        match linked {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => anyhow::bail!("state {state} already exists"),
            r => Ok(r?),
        }
    }

    pub fn state_ref(&self, state: &str) -> Result<Option<StateRef>> {
        if state.is_empty() || !state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Ok(None);
        }
        let bytes = match fs::read(self.root.join("states").join(state)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let text = damage_of(|| Ok(String::from_utf8(bytes)?))?;
        let field = |name: &str| -> Result<Id> {
            let key = format!("\"{name}\":\"");
            let at = text.find(&key).ok_or_else(|| anyhow::anyhow!("state {state} has no {name}"))? + key.len();
            Ok(Id::parse(text.get(at..at + 64).unwrap_or(""))?)
        };
        let mut r = damage_of(|| {
            let r = StateRef { tree: field("tree")?, times: field("times")? };
            ensure!(text == format!("{{\"times\":\"{}\",\"tree\":\"{}\"}}\n", r.times, r.tree), "state {state} is not canonical");
            Ok(r)
        })?;
        if self.has(&r.tree) && self.has(&r.times) {
            let bytes = self.times_bytes(&r.times)?;
            if bytes.first() == Some(&1) {
                // Version 1 was used by both the flat Cloud table and an experimental
                // per-folder table. Verify both against the tree and refuse ambiguity.
                let flat = crate::cloud_times::to_local(self, &r.tree, &bytes);
                let folder = crate::cloud_times::from_local(self, &r.tree, &r.times)
                    .and_then(|wire| crate::cloud_times::to_local(self, &r.tree, &wire));
                let times = match (flat, folder) {
                    (Ok(a), Ok(b)) if a != b => bail!("state {state} has ambiguous version 1 times"),
                    (Ok(a), _) | (_, Ok(a)) => a,
                    (Err(a), Err(b)) => bail!("state {state} has invalid version 1 times: flat {a:#}; folder {b:#}"),
                };
                r.times = times;
                // This read can overlap a writer that replaces the state mapping. Returning the
                // converted table is safe; writing it here could restore an obsolete mapping.
            }
        }
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
        self.pack_selected(None)
    }

    fn pack_selected(&self, selected: Option<&MaintenancePlan>) -> Result<PackReport> {
        use rayon::prelude::*;
        let set = self.reload_packs()?;
        let mut report = PackReport::default();
        let mut loose: HashMap<Id, u64> = HashMap::new();
        for dir in fan_folders(&self.root.join("objects"))? {
            let fan = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let Ok(id) = Id::parse(&format!("{fan}{}", entry.file_name().to_string_lossy())) else { continue };
                if selected.is_some_and(|plan| !plan.pack.contains(&id)) {
                    continue;
                }
                if let Some((pack, packed)) = set.find(&id, &self.last_pack)? {
                    // The loose copy goes only when the packed form reads with what it needs: a copy
                    // that a write made again over a damaged packed form stays, and is not packed again.
                    if self.packed_problem(&set, pack, &packed)?.is_none() {
                        fs::remove_file(entry.path())?;
                        report.removed += 1;
                    }
                    continue;
                }
                loose.insert(id, entry.metadata()?.len());
            }
        }
        report.loose_bytes = loose.values().sum();
        let order = match selected {
            Some(plan) => self.pack_order_roots(&loose, &plan.roots),
            None => self.pack_order(&loose)?,
        };
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
            let payloads: Vec<Result<Payload>> = batch.par_iter().map(|id| self.payload(id)).collect();
            for (id, p) in batch.iter().zip(payloads) {
                // A body that does not decode stays loose and is reported: it stops no repack.
                let p = match p {
                    Ok(p) => p,
                    Err(error) if is_unreadable(error.as_ref()) => {
                        report.unreadable.push(format!("object {id} does not decode: {error:#}"));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
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

    /// Write again, in blocks of `block_size`, the packs that have larger blocks (an older
    /// default: 4 MiB). The objects keep their order (path order from `pack`). The new packs are
    /// complete on disk before an old pack goes; a reader that has an old pack open still reads it,
    /// and a reader that finds no old index reads the packs again. A store with no such pack
    /// changes nothing.
    pub fn reblock(&self) -> Result<PackReport> {
        use rayon::prelude::*;
        let set = self.reload_packs()?;
        let mut old: Vec<(u32, Vec<pack::Entry>)> = Vec::new();
        for (n, p) in set.packs.iter().enumerate() {
            let index = p.index()?;
            let mut count = vec![0u32; index.blocks.len()];
            let mut entries = Vec::with_capacity(index.count);
            for i in 0..index.count {
                let e = index.entry(i)?;
                count[e.block as usize] += 1;
                entries.push(e);
            }
            // A block of one object may be larger than `block_size`; a block of several is not.
            if index.blocks.iter().zip(&count).any(|(b, &c)| c > 1 && b.plain as usize > self.options.block_size) {
                entries.sort_by_key(|e| (e.block, e.offset));
                old.push((n as u32, entries));
            }
        }
        let mut report = PackReport::default();
        if old.is_empty() {
            return Ok(report);
        }
        let codec = crate::zstdx::Codec { level: self.options.pack_level, long: self.options.pack_long, dict: None };
        let mut builder = Builder::new(self.options.block_size);
        let mut full: Vec<Builder> = Vec::new();
        let flush = |full: &mut Vec<Builder>, report: &mut PackReport| -> Result<()> {
            let done =
                std::mem::take(full).into_par_iter().map(|b| self.write_pack(b, &codec)).collect::<Result<Vec<PackReport>>>()?;
            for d in done {
                report.packs += d.packs;
                report.objects += d.objects;
                report.pack_bytes += d.pack_bytes;
            }
            Ok(())
        };
        let mut seen: HashSet<Id> = HashSet::new();
        for (n, entries) in &old {
            for e in entries {
                if !seen.insert(e.id) {
                    continue;
                }
                let block = self.block(&set, *n, e.block)?;
                builder.add(e.id, e.kind, e.form, &block[e.offset as usize..(e.offset + e.length) as usize]);
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
        ensure!(report.objects == seen.len() as u64, "a reblock wrote {} of {} objects", report.objects, seen.len());
        let dir = self.root.join("packs");
        for (n, _) in &old {
            // A new pack has no large block, so its id is never the id of an old pack.
            let id = set.packs[*n as usize].id;
            for ext in ["idx", "pack"] {
                match fs::remove_file(dir.join(format!("{id}.{ext}"))) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            report.reblocked += 1;
        }
        File::open(&dir)?.sync_all()?;
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
        let mut roots = Vec::new();
        // The order only groups neighbors: a damaged state record leaves its objects to the rest.
        roots.extend(states.iter().filter_map(|state| self.state_ref(state).ok().flatten()));
        Ok(self.pack_order_roots(loose, &roots))
    }

    fn pack_order_roots(&self, loose: &HashMap<Id, u64>, roots: &[StateRef]) -> Vec<Id> {
        let mut seen = HashSet::new();
        let (mut trees, mut others) = (Vec::new(), Vec::new());
        for r in roots {
            self.order_tree(&r.tree, loose, &mut seen, &mut trees, &mut others);
            self.order_times(&r.times, loose, &mut seen, &mut trees);
        }
        let mut rest: Vec<Id> = loose.keys().filter(|id| !seen.contains(*id)).copied().collect();
        rest.sort();
        trees.extend(others);
        trees.extend(rest);
        trees
    }

    /// A times table and the tables below it. A table already in a pack ends the walk: a table
    /// is packed with or after the tables it names.
    fn order_times(&self, id: &Id, loose: &HashMap<Id, u64>, seen: &mut HashSet<Id>, out: &mut Vec<Id>) {
        if !loose.contains_key(id) || !seen.insert(*id) {
            return;
        }
        out.push(*id);
        // The order only groups neighbors: a table that cannot be read goes with the rest.
        let Ok(times) = self.get_times(id) else { return };
        for r in times.records {
            if let crate::times::Record::Folder(child) = r {
                self.order_times(&child, loose, seen, out);
            }
        }
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
    }

    fn order_tree(&self, id: &Id, loose: &HashMap<Id, u64>, seen: &mut HashSet<Id>, trees: &mut Vec<Id>, others: &mut Vec<Id>) {
        if !seen.insert(*id) {
            return;
        }
        if loose.contains_key(id) {
            trees.push(*id);
        }
        // A tree, blob or chunk list that cannot be read goes with the rest; the repack reports it.
        let Ok(tree) = self.get_tree(id) else { return };
        for e in tree.entries {
            match e.node {
                crate::tree::Node::Dir { tree } => self.order_tree(&tree, loose, seen, trees, others),
                crate::tree::Node::File { blob, .. } if loose.contains_key(&blob) && seen.insert(blob) => {
                    others.push(blob);
                    let Ok(p) = self.payload(&blob) else { continue };
                    if p.form != pack::CHUNKED {
                        continue;
                    }
                    for (chunk, _) in chunk_list(&p.bytes).unwrap_or_default() {
                        if loose.contains_key(&chunk) && seen.insert(chunk) {
                            others.push(chunk);
                        }
                    }
                }
                _ => {}
            }
        }
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
            let tmp = self.write_temp(&[bytes], 0o444)?;
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
            let header = check_loose(&path)
                .with_context(|| path.display().to_string())?
                .with_context(|| format!("{} vanished", path.display()))?
                .header;
            let (count, bytes) = match (header.kind, header.form) {
                (Kind::Tree, _) => (&mut s.trees, &mut s.tree_bytes),
                (Kind::Chunk, _) => (&mut s.chunks, &mut s.chunk_bytes),
                (Kind::Blob, loose::Form::Whole | loose::Form::Dictionary) => (&mut s.whole_blobs, &mut s.whole_bytes),
                (Kind::Blob, loose::Form::Delta) => (&mut s.delta_blobs, &mut s.delta_bytes),
                (Kind::Blob, loose::Form::Chunked) => (&mut s.chunked_blobs, &mut s.chunked_bytes),
                (Kind::Times, _) => (&mut s.times, &mut s.times_bytes),
                (Kind::Opaque, _) => unreachable!("check_loose refuses opaque objects"),
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

/// A loose object file as its first bytes show it: its header, the bytes a repack decodes from it (a
/// whole frame's content size, else the body length), and what its form needs to read: its chunks
/// or delta base (`needs`) and its dictionary.
pub(crate) struct Loose {
    pub(crate) header: Header,
    pub(crate) input: u64,
    pub(crate) needs: Vec<(Id, Kind)>,
    pub(crate) dict: Option<Id>,
}

/// The loose object file at `path`, checked from its first 64 bytes (a chunk list whole); None when
/// there is no file. `Unreadable` when the file cannot be an object: empty or cut short (a crash
/// before its data reached the disk, #452), an unknown header, a zstd frame header or a chunk list
/// that does not parse. Any other I/O error goes back as it is. The one check of presence
/// (`object_problem`), writers, maintenance and fsck. A body cut after a valid frame header shows only
/// on a read (a repack then leaves the object loose and reports it).
pub(crate) fn check_loose(path: &Path) -> Result<Option<Loose>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    damage_of(|| {
        let len = file.metadata()?.len();
        let mut head = Vec::with_capacity(64);
        (&mut file).take(64).read_to_end(&mut head)?;
        ensure!(!head.is_empty(), "is empty");
        ensure!(head.len() > 6, "is cut short ({} bytes)", head.len());
        let header = Header::decode(&head).context("has no valid header")?;
        ensure!(header.kind != Kind::Opaque, "is an opaque object, which is never loose");
        let frame = |at: usize| zstd_safe::get_frame_content_size(head.get(at..).unwrap_or_default());
        let (input, needs, dict) = match header.form {
            // A whole frame names its content size: a tiny frame can decode to a very large payload.
            loose::Form::Whole => (frame(6).ok().flatten().context("has no valid zstd frame")?, Vec::new(), None),
            loose::Form::Dictionary => {
                let input = frame(38).ok().flatten().context("has no valid zstd frame")?;
                let mut dict = [0; 32];
                dict.copy_from_slice(&head[6..38]);
                (input, Vec::new(), Some(Id(dict)))
            }
            // The depth byte and the base id come first.
            loose::Form::Delta => {
                ensure!(frame(39).is_ok(), "has no valid zstd frame");
                (len - 6, body_needs(pack::DELTA, &head[6..])?, None)
            }
            // A chunk list is small (34 bytes per chunk): it is checked whole.
            loose::Form::Chunked => {
                let mut body = head[6..].to_vec();
                file.read_to_end(&mut body)?;
                (len - 6, body_needs(pack::CHUNKED, &body)?, None)
            }
        };
        Ok(Some(Loose { header, input, needs, dict }))
    })
}

/// What a stored blob body of `form` (`pack::RAW`, `DELTA` or `CHUNKED`) needs to read: its delta
/// base or its chunks.
pub(crate) fn body_needs(form: u8, body: &[u8]) -> Result<Vec<(Id, Kind)>> {
    match form {
        pack::DELTA => {
            ensure!(body.len() > 33, "has a delta cut short");
            let mut base = [0; 32];
            base.copy_from_slice(&body[1..33]);
            Ok(vec![(Id(base), Kind::Blob)])
        }
        pack::CHUNKED => Ok(chunk_list(body)?.into_iter().map(|(chunk, _)| (chunk, Kind::Chunk)).collect()),
        _ => Ok(Vec::new()),
    }
}

/// An object or state record that cannot be read: missing, damaged on disk, or not what its id names.
/// Typed, so a caller tells damage from other failures (`line delete --discard` deletes without a
/// final save on damage only).
#[derive(Debug)]
pub struct Unreadable(pub String);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreadable {}

pub(crate) fn unreadable(what: String) -> anyhow::Error {
    anyhow::Error::new(Unreadable(what))
}

/// Run a read and check of stored bytes: a failure to decode or validate them, or a file that is not
/// there, ends early or is not text where text belongs, is damage (`Unreadable`). Any other I/O error (EIO, EACCES, ENOSPC, ...) is
/// operational and goes back unchanged, so no caller takes it for damage.
pub(crate) fn damage_of<T>(check: impl FnOnce() -> Result<T>) -> Result<T> {
    check().map_err(|error| {
        let operational = error.chain().any(|e| {
            e.downcast_ref::<io::Error>().is_some_and(|io| {
                !matches!(io.kind(), io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData)
            })
        });
        if operational || is_unreadable(error.as_ref()) {
            error
        } else {
            unreadable(format!("{error:#}"))
        }
    })
}

/// Whether `error` comes from a damaged or missing object or state record (`Unreadable`).
pub fn is_unreadable(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(e) = next {
        if e.is::<Unreadable>() {
            return true;
        }
        next = e.source();
    }
    false
}

/// The chunks of a chunk list (`CHUNKED` body): ids and lengths, with no byte after them.
pub(crate) fn chunk_list(body: &[u8]) -> Result<Vec<(Id, u64)>> {
    let mut reader = Reader::new(body);
    let mut out = Vec::new();
    for _ in 0..reader.varint()? {
        out.push((reader.id()?, reader.varint()?));
    }
    ensure!(reader.done(), "has bytes after its chunk list");
    Ok(out)
}

/// Write the new file `tmp` and sync it (removed again on an error). A plain fsync on every system:
/// it survives a kernel crash or a file system that turns read-only (#452); F_FULLFSYNC per object
/// would make large writes on macOS very slow.
fn write_synced(tmp: &Path, parts: &[&[u8]], mode: u32) -> Result<()> {
    let written = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(tmp)?;
        for part in parts {
            f.write_all(part)?;
        }
        rustix::fs::fsync(&f)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(tmp);
    }
    written
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
    fn hash_file_ids_are_fixed_for_plain_and_keyed_stores() -> anyhow::Result<()> {
        let dir = scratch("hash-file-ids");
        let path = dir.join("file");
        // Independently computed SHA-256 / HMAC-SHA256 of "blob <len>\0" and i * 31 mod 256.
        let cases: [(u64, &str, &str); 3] = [
            (
                0,
                "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813",
                "36b663e28418866e8cb27f0d85472e4686f62fdf37f8d0f2988a3f9ea1fc3386",
            ),
            (
                4096,
                "eab24b561322be2a89ec804dc57ad63eb85754869b0ddd9dd4df3c552cf95560",
                "79cc741001db4095ae995c97a39fd3bcab2df17de81b62e9920f885d0250d4a2",
            ),
            (
                (1 << 20) + 17,
                "d424c32a92d46dce1dad6e7c9dcca49b816f9df4f265ea10d7ee00a3810d9f3b",
                "cca6ed13a5d0e351047d1bc6d716d694dd9e02c2b208f4007402f406eb0f3775",
            ),
        ];
        for (name, keying, column) in [("plain", Keying::Plain, 0), ("keyed", Keying::Keyed([42; 32]), 1)] {
            let store = Store::open(&dir.join(name), keying)?;
            // Read large then small files too: stale scratch bytes must never enter the hash.
            for &(len, plain, keyed) in cases.iter().chain(cases.iter().rev()) {
                let bytes: Vec<u8> = (0..len).map(|i| (i * 31).to_le_bytes()[0]).collect();
                fs::write(&path, &bytes)?;
                let (id, size) = store.hash_file(&path)?;
                assert_eq!(size, len);
                assert_eq!(id.hex(), [plain, keyed][column], "{name}, {len} bytes");
            }
        }
        Ok(())
    }

    #[test]
    fn prune_keeps_what_kept_states_reach_and_drops_the_rest() {
        let dir = scratch("prune");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let write = |name: &str, files: &[(&str, &[u8])], previous: Option<Id>| {
            let src = dir.join(name);
            fs::create_dir_all(&src).unwrap();
            for (f, data) in files {
                fs::write(src.join(f), data).unwrap();
            }
            let opts = crate::WriteOptions { temperature: Temperature::Cold, previous, exclude: Vec::new(), similar: false };
            let r = crate::write_tree(&store, &src, &opts).unwrap();
            let refs = StateRef { tree: r.tree.unwrap(), times: r.times.unwrap() };
            store.set_state(name, &refs).unwrap();
            refs
        };
        let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut big2 = big.clone();
        big2[100] ^= 1;
        let a = write("a", &[("kept", b"kept bytes"), ("big", &big)], None);
        let b = write("b", &[("gone", b"only in b"), ("big", &big2)], Some(a.tree));
        let gone = Keying::Plain.id(Kind::Blob, b"only in b");
        assert!(store.has(&gone));
        let prune = || {
            let plan = store.plan_maintenance(&["a".to_string()].into(), &[], MaintenanceLimit::UNLIMITED).unwrap();
            store.prune_planned(&plan).unwrap();
            plan.prune
        };
        let report = prune();
        assert_eq!(report.states, 1, "the record of b goes");
        assert!(store.state_ref("b").unwrap().is_none());
        assert!(!store.has(&gone), "a blob that only b reaches goes");
        assert!(!store.has(&b.tree), "the tree of b goes");
        assert_eq!(store.read_blob(&Keying::Plain.id(Kind::Blob, b"kept bytes")).unwrap(), b"kept bytes");
        assert_eq!(store.read_blob(&Keying::Plain.id(Kind::Blob, &big)).unwrap(), big, "a's blobs still read");
        let again = prune();
        assert_eq!((again.states, again.objects, again.blobs), (0, 0, 0), "a second prune finds nothing");
        // A blob that another index names (the git ids of a project) stays without a state.
        write("c", &[("git", b"read by git only")], None);
        let git = Keying::Plain.id(Kind::Blob, b"read by git only");
        let plan = store
            .plan_maintenance_keeping(&["a".to_string()].into(), &[git].into(), &[], MaintenanceLimit::UNLIMITED)
            .unwrap();
        store.prune_planned(&plan).unwrap();
        assert_eq!(plan.prune.states, 1, "the record of c goes");
        assert_eq!(store.read_blob(&git).unwrap(), b"read by git only", "the named blob stays");
    }

    #[test]
    fn reblock_writes_large_blocks_again_and_readers_keep_reading() {
        let dir = scratch("reblock");
        let root = dir.join("store");
        let mut old = Store::open(&root, Keying::Plain).unwrap();
        old.options.block_size = 4 << 20;
        old.options.train_dict = false;
        let files: Vec<Vec<u8>> = (0..300u64).map(|i| noise(4000 + i as usize, i)).collect();
        let ids: Vec<Id> = files.iter().map(|f| old.put_cold_bytes(f, None).unwrap().0).collect();
        old.pack().unwrap();
        let packs = |s: &Store| s.reload_packs().unwrap().packs.iter().map(|p| p.id).collect::<Vec<Id>>();
        let before = packs(&old);
        assert_eq!(before.len(), 1);
        // One reader read the old index; another lists the old pack but has not read its index.
        let warm = Store::open(&root, Keying::Plain).unwrap();
        assert_eq!(warm.read_blob(&ids[0]).unwrap(), files[0]);
        let cold = Store::open(&root, Keying::Plain).unwrap();
        cold.reload_packs().unwrap();
        let mut new = Store::open(&root, Keying::Plain).unwrap();
        new.options.block_size = 64 << 10;
        let r = new.reblock().unwrap();
        assert_eq!((r.reblocked, r.objects), (1, 300));
        let after = packs(&new);
        assert!(!after.contains(&before[0]), "the old pack goes");
        for p in &new.reload_packs().unwrap().packs {
            let index = p.index().unwrap();
            assert!(index.blocks.iter().all(|b| b.plain as usize <= 64 << 10), "every block is small");
        }
        for (id, f) in ids.iter().zip(&files) {
            assert_eq!(&warm.read_blob(id).unwrap(), f, "a reader with the old index open");
            assert_eq!(&cold.read_blob(id).unwrap(), f, "a reader without the old index");
            assert_eq!(&new.read_blob(id).unwrap(), f);
        }
        let again = new.reblock().unwrap();
        assert_eq!((again.reblocked, again.packs), (0, 0), "a second reblock changes nothing");
        assert_eq!(packs(&new), after);
    }

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
    fn object_storage_is_v2_and_legacy_upgrade_preserves_objects() {
        let dir = scratch("object-storage-v2");
        for (name, keying) in [("plain", Keying::Plain), ("secret", Keying::Keyed([7; 32]))] {
            let root = dir.join(name);
            let store = Store::open(&root, keying.clone()).unwrap();
            let config = format!("{{\"ids\":\"{}\",\"v\":2}}\n", keying.name());
            assert_eq!(fs::read_to_string(root.join("config.json")).unwrap(), config);
            let (id, _) = store.put_cold_bytes(b"existing content", None).unwrap();
            drop(store);
            fs::write(root.join("config.json"), format!("{{\"ids\":\"{}\",\"v\":1}}\n", keying.name())).unwrap();
            let legacy_config = fs::read(root.join("config.json")).unwrap();
            let existing = Store::open_existing(&root, keying.clone()).unwrap();
            assert_eq!(existing.read_blob(&id).unwrap(), b"existing content");
            assert_eq!(fs::read(root.join("config.json")).unwrap(), legacy_config);
            drop(existing);
            let store = Store::open(&root, keying.clone()).unwrap();
            assert_eq!(fs::read_to_string(root.join("config.json")).unwrap(), config);
            assert_eq!(store.read_blob(&id).unwrap(), b"existing content");
            assert_eq!(store.put_cold_bytes(b"existing content", None).unwrap().0, id);
            drop(store);
            assert_eq!(Store::open(&root, keying).unwrap().read_blob(&id).unwrap(), b"existing content");
        }
    }

    #[test]
    fn incompatible_format_does_not_change_the_store() {
        let dir = scratch("incompatible-format-no-write");
        for (name, config) in [
            ("future", "{\"ids\":\"sha256\",\"v\":999}\n"),
            ("invalid", "invalid config\n"),
            ("other-ids", "{\"ids\":\"hmac-sha256\",\"v\":2}\n"),
            ("legacy-other-ids", "{\"ids\":\"hmac-sha256\",\"v\":1}\n"),
        ] {
            let root = dir.join(name);
            fs::create_dir_all(root.join("tmp")).unwrap();
            fs::write(root.join("config.json"), config).unwrap();
            let orphan = root.join("tmp/999999999-0");
            fs::write(&orphan, b"preserve me").unwrap();
            assert!(Store::open(&root, Keying::Plain).is_err());
            assert_eq!(fs::read_to_string(root.join("config.json")).unwrap(), config);
            assert_eq!(fs::read(&orphan).unwrap(), b"preserve me");
            let mut entries: Vec<_> = fs::read_dir(&root).unwrap().map(|entry| entry.unwrap().file_name()).collect();
            entries.sort();
            assert_eq!(entries, vec![std::ffi::OsString::from("config.json"), std::ffi::OsString::from("tmp")]);
        }
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
    fn check_loose_reads_the_input_of_each_form() {
        let dir = scratch("check-loose-forms");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let input = |id: &Id| check_loose(&store.object_path(id)).unwrap().unwrap().input;
        let body = |id: &Id| fs::metadata(store.object_path(id)).unwrap().len() - 6;
        let v1 = noise(200_000, 11);
        let (whole, _) = store.put_cold_bytes(&v1, None).unwrap();
        assert_eq!(input(&whole), 200_000, "a whole frame names its content size");
        let mut v2 = v1.clone();
        v2[7] ^= 1;
        let (delta, _) = store.put_cold_bytes(&v2, Some(whole)).unwrap();
        assert!(matches!(store.form(&delta).unwrap(), Form::Delta { .. }));
        assert_eq!(input(&delta), body(&delta));
        let (chunked, _) = store.put_cold_bytes(&noise(2 << 20, 12), None).unwrap();
        assert_eq!(store.form(&chunked).unwrap(), Form::Chunked);
        assert_eq!(input(&chunked), body(&chunked));
        // A dictionary frame follows the dictionary id.
        let path = dir.join("dictionary-form");
        let mut bytes = b"LOB1by".to_vec();
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&layr_store::zst::compress(&[5; 3000], None).unwrap());
        fs::write(&path, bytes).unwrap();
        assert_eq!(check_loose(&path).unwrap().unwrap().input, 3000);
    }

    #[test]
    fn damage_is_a_typed_error() {
        let dir = scratch("typed-damage");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let tree = store.put_tree(&Tree::default()).unwrap();
        let path = store.object_path(&tree);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, b"").unwrap();
        let error = store.get_tree(&tree).unwrap_err().context("a caller's context");
        assert!(is_unreadable(error.as_ref()), "{error:#}");
        fs::remove_file(&path).unwrap();
        assert!(is_unreadable(store.get_tree(&tree).unwrap_err().as_ref()), "a missing object");
        let (chunked, _) = store.put_cold_bytes(&noise(2 << 20, 3), None).unwrap();
        let list = store.object_path(&chunked);
        fs::set_permissions(&list, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&list, b"LOB1bc\x01").unwrap();
        assert!(!store.has(&chunked), "a chunk list cut short is no object");
        assert!(is_unreadable(store.read_blob(&chunked).unwrap_err().as_ref()));
        // An operational I/O error (here an injected EIO) is never damage, wherever a read meets it.
        let eio = damage_of(|| -> Result<()> {
            Err(anyhow::Error::from(io::Error::from_raw_os_error(rustix::io::Errno::IO.raw_os_error())).context("read a block"))
        })
        .unwrap_err();
        assert!(!is_unreadable(eio.as_ref()), "{eio:#}");
        assert_eq!(eio.downcast_ref::<io::Error>().and_then(io::Error::raw_os_error), Some(rustix::io::Errno::IO.raw_os_error()));
        let short = damage_of(|| -> Result<()> { Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()) }).unwrap_err();
        assert!(is_unreadable(short.as_ref()), "a file that ends early is damage");
        // Other failures are not damage, such as a file this process may not read.
        assert!(!is_unreadable(anyhow::anyhow!("the cache setup is busy").as_ref()));
        if !rustix::process::geteuid().is_root() {
            let other = store.put_tree(&Tree { entries: Vec::new(), links: vec![vec![b"a".to_vec()]] }).unwrap();
            fs::set_permissions(store.object_path(&other), fs::Permissions::from_mode(0o000)).unwrap();
            let refused = store.get_tree(&other).unwrap_err();
            assert!(!is_unreadable(refused.as_ref()), "{refused:#}");
        }
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
