//! Content-defined chunks of cache layers in pack objects (`layr_store::chunks`).
//!
//! A cache state loses its base at the next save (`history=last`), so a push cannot make a delta
//! against the old content. Chunks need only the ids that the store has: a changed build output
//! file shares most of its chunks with files that the store already holds, under any path.
//!
//! The store keeps no mutable chunk map. The pack indexes under `chunks/` are the truth: a writer
//! lists them to dedup, a reader lists them to find a chunk, and `release` rewrites a pack whose
//! chunks are partly dead into a new pack before the old one goes.

use super::{
    backend::{Backend, Missing, Undecodable},
    download::TRANSFER_BUFFER_BUDGET,
    keys, Store,
};
use anyhow::{bail, Context, Result};
use layr_store::chunks::{self, Builder, Index};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Where a chunk is: its pack, and its range in the pack's plaintext.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Place {
    pub pack: String,
    pub offset: u64,
    pub length: u64,
}

/// The chunk indexes of a layer, by chunk id. A chunk in two packs (a rewrite in progress) maps
/// to one of them; both hold the same bytes.
pub(crate) type Places = BTreeMap<String, Place>;

/// The packs of a layer whose index and data are both published. A pack without its index is an unfinished
/// upload: no tree can name its chunks, and the deletion path removes it. An index without its data (a delete that
/// removed the data and not the index) names chunks that nobody can read: they count as absent, so a push uploads
/// them again, and the sweep deletes the index.
fn packs(backend: &Backend) -> Result<BTreeSet<String>> {
    Ok(listing(backend)?.0.into_keys().collect())
}

/// The packs with an index and data, with the listed size of the index, and the packs with only an index.
pub(super) fn listing(backend: &Backend) -> Result<(BTreeMap<String, Option<u64>>, BTreeSet<String>)> {
    let (mut indexes, mut data) = (BTreeMap::new(), BTreeSet::new());
    for object in backend.list("chunks/")? {
        match chunks::parse_name(&object.key) {
            Some((pack, true)) => {
                indexes.insert(pack, object.size);
            }
            Some((pack, false)) => {
                data.insert(pack);
            }
            None => {}
        }
    }
    let without = indexes.keys().filter(|pack| !data.contains(*pack)).cloned().collect();
    indexes.retain(|pack, _| data.contains(pack));
    Ok((indexes, without))
}

fn cache_dir(dir: &Path) -> PathBuf {
    dir.join("chunk-indexes")
}

fn cached_index(dir: Option<&Path>, pack: &str) -> Option<Index> {
    let bytes = std::fs::read(cache_dir(dir?).join(pack)).ok()?;
    Index::parse(&bytes).ok()
}

/// One pack index, from the local cache or the store. Indexes never change after publication.
/// `dir` is the local store folder that caches indexes; a reader without one reads them each time.
pub(crate) fn index(backend: &Backend, dir: Option<&Path>, identity: &str, pack: &str) -> Result<Index> {
    if let Some(index) = cached_index(dir, pack) {
        return Ok(index);
    }
    let bytes =
        backend.decrypted(&chunks::index_name(pack), identity).with_context(|| format!("read chunk index of pack {pack}"))?;
    let index = Index::parse(&bytes)?;
    if let Some(dir) = dir {
        remember(dir, pack, &bytes);
    }
    Ok(index)
}

/// Keeps a copy of a published index. The copy is a cache that the store can rebuild, so it is
/// written without fsync, and a failed write is ignored.
fn remember(dir: &Path, pack: &str, bytes: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    let folder = cache_dir(dir);
    let temporary = folder.join(format!(".{pack}.{}", std::process::id()));
    let written = std::fs::create_dir_all(&folder).and_then(|_| {
        use std::io::Write;
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?.write_all(bytes)
    });
    if written.and_then(|_| std::fs::rename(&temporary, folder.join(pack))).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// A short fingerprint of the published packs of the layer: it changes when a push adds a pack or a release removes
/// one. A fetch that failed on a missing chunk tries the same state again only when it changed.
pub(crate) fn packs_fingerprint(backend: &Backend) -> Result<String> {
    use std::hash::{Hash, Hasher};
    // Not a security check: another layr version may give another value, and the state is tried once more.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    packs(backend)?.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

/// Every published chunk of the layer.
pub(crate) fn places(backend: &Backend, dir: Option<&Path>, identity: &str) -> Result<Places> {
    Ok(places_of(&indexes(backend, dir, identity)?))
}

fn places_of(indexes: &BTreeMap<String, Index>) -> Places {
    let mut out = Places::new();
    for (pack, index) in indexes {
        for (id, offset, length) in &index.chunks {
            out.entry(id.clone()).or_insert(Place { pack: pack.clone(), offset: *offset, length: *length });
        }
    }
    out
}

/// The published pack indexes of the layer, by pack.
pub(crate) fn indexes(backend: &Backend, dir: Option<&Path>, identity: &str) -> Result<BTreeMap<String, Index>> {
    // A release of another machine may delete a listed pack before its index is read; its live chunks are in a new
    // pack then, so the listing runs again.
    for _ in 0..3 {
        if let Some(out) = indexes_once(backend, dir, identity)? {
            return Ok(out);
        }
    }
    indexes_once(backend, dir, identity)?.context("the chunk packs changed during each of 4 listings; retry")
}

/// The published indexes, or None when a listed pack went away before its index was read.
fn indexes_once(backend: &Backend, dir: Option<&Path>, identity: &str) -> Result<Option<BTreeMap<String, Index>>> {
    let packs = listing(backend)?.0;
    // Forget cached indexes of packs that are gone, so the cache does not grow.
    if let Some(Ok(entries)) = dir.map(|dir| std::fs::read_dir(cache_dir(dir))) {
        for entry in entries.flatten() {
            if !packs.contains_key(entry.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let mut out = BTreeMap::new();
    let mut missing = Vec::new();
    for (pack, size) in packs {
        if let Some(index) = cached_index(dir, &pack) {
            out.insert(pack, index);
        } else {
            missing.push((pack, size));
        }
    }
    // Listed sizes reserve the bytes of each read (`Backend::visit_many`).
    let sizes: Vec<Option<u64>> = missing.iter().map(|(_, size)| *size).collect();
    let missing: Vec<String> = missing.into_iter().map(|(pack, _)| pack).collect();
    let names = missing.iter().map(|pack| chunks::index_name(pack)).collect::<Vec<_>>();
    let loaded = backend.visit_decrypted_many(&names, &sizes, identity, |i, bytes| {
        let index = Index::parse(&bytes)?;
        if let Some(dir) = dir {
            remember(dir, &missing[i], &bytes);
        }
        out.insert(missing[i].clone(), index);
        Ok(())
    });
    if let Err(error) = loaded {
        let now = self::packs(backend)?;
        if missing.iter().any(|pack| !now.contains(pack)) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(out))
}

/// Uploads the chunks of the files of one state.
pub(crate) struct Writer<'scope, 'env> {
    store: &'scope Store,
    identity: String,
    key: [u8; 32],
    /// Published chunks when the push started.
    remote: Places,
    /// Chunks in packs of this push, published or in the open builder.
    written: BTreeSet<String>,
    /// File content (SHA-256) to its chunk ids, from the base tree and this push.
    by_sha: BTreeMap<String, Vec<String>>,
    builder: Builder,
    /// Compresses, encrypts and uploads full packs on the transfer pool while files are chunked.
    uploads: super::download::Pool<'scope, 'env, Builder, u64>,
    /// Compressed bytes written.
    pub bytes: u64,
    pub packs: usize,
    /// Time in chunking and ids, and in waits for the upload threads, for `LAYR_TRACE`.
    split_time: std::time::Duration,
    wait_time: std::time::Duration,
}

/// Publishes one pack under `name` of the size of its pack object: the pack first, then its index, which makes it
/// visible.
fn upload(backend: &Backend, recipient: &str, dir: &Path, builder: Builder, name: impl FnOnce(u64) -> String) -> Result<u64> {
    let (frame, index) = builder.finish()?;
    let body = keys::encrypt(recipient, &frame)?;
    let pack = name(body.len() as u64);
    let encoded = index.encode();
    let index_body = keys::encrypt(recipient, &encoded)?;
    let bytes = (body.len() + index_body.len()) as u64;
    if !backend.put_new(&chunks::pack_name(&pack), body)? {
        bail!("chunk pack {pack} already exists");
    }
    if !backend.put_new(&chunks::index_name(&pack), index_body)? {
        bail!("chunk index {pack} already exists");
    }
    remember(dir, &pack, &encoded);
    Ok(bytes)
}

/// The first 24 characters of the id of each pack that a release copies the `live` chunks of `pack` into; the last 12
/// are a discriminator of 4 hex digits and the size of the pack object in 8 (a pack is far below 4 GiB). A release that
/// stopped before `pack` went finds its copies in a listing, and the listed size shows that a copy is whole.
fn copy_prefix(pack: &str, live: &[(String, u64, u64)]) -> String {
    use layr_hashers::{ContentAddress, Digest};
    let mut text = format!("layr chunk release\0{pack}");
    for (id, _, _) in live {
        text.push('\0');
        text.push_str(id);
    }
    let digest = ContentAddress::digest(text.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes).into_uuid().hyphenated().to_string()[..24].to_string()
}

/// The size of a copy (`copy_prefix`) that its id names.
fn copy_size(id: &str) -> Option<u64> {
    u64::from_str_radix(id.get(28..)?, 16).ok()
}

/// A Vec may reserve twice the bytes it holds while a transport or decoder grows it.
const VEC_GROWTH_WORST: usize = 2;

fn pack_bounds() -> (usize, usize) {
    let compressed = layr_store::zst::compress_bound(chunks::PACK_LIMIT);
    (compressed, keys::ciphertext_len(compressed))
}

/// Every representation of one pack while it is built, compressed, encrypted and sent, allowing growth of each.
fn pack_charge() -> usize {
    let (compressed, encrypted) = pack_bounds();
    VEC_GROWTH_WORST * (chunks::PACK_LIMIT + compressed + encrypted)
}

/// Reader decoders that fit the budget beside the packs kept around the reader (`ahead_limit`).
fn worker_cap() -> usize {
    ((TRANSFER_BUFFER_BUDGET - (KEEP_BEHIND + READER_CACHE + 1) * chunks::PACK_LIMIT) / pack_charge() - 1).max(1)
}

fn parallel() -> usize {
    crate::fsutil::workers().min(worker_cap())
}

impl<'scope, 'env> Writer<'scope, 'env> {
    /// `by_sha` maps file content to the chunk ids of pushed trees; equal files skip chunking. Packs go up on threads
    /// of `scope`.
    pub(crate) fn open(
        scope: &'scope std::thread::Scope<'scope, 'env>,
        store: &'scope Store,
        recipient: &str,
        identity: &str,
        key: [u8; 32],
        by_sha: BTreeMap<String, Vec<String>>,
    ) -> Result<Self> {
        let remote = places(&store.backend, Some(&store.dir), identity)?;
        Ok(Self {
            store,
            identity: identity.to_string(),
            key,
            remote,
            written: BTreeSet::new(),
            by_sha,
            builder: Builder::default(),
            uploads: {
                let recipient = recipient.to_string();
                let upload = move |builder| upload(&store.backend, &recipient, &store.dir, builder, |_| crate::ids::new_id());
                super::download::Pool::new(scope, super::download::JOBS, TRANSFER_BUFFER_BUDGET as u64, upload)
            },
            bytes: 0,
            packs: 0,
            split_time: std::time::Duration::ZERO,
            wait_time: std::time::Duration::ZERO,
        })
    }

    /// File content to chunk ids, for the next state of the push.
    pub(crate) fn into_hashes(self) -> BTreeMap<String, Vec<String>> {
        if std::env::var_os("LAYR_TRACE").is_some() {
            eprintln!("trace {:>8.1} ms  store chunk split and ids", self.split_time.as_secs_f64() * 1000.0);
            eprintln!("trace {:>8.1} ms  store chunk upload wait", self.wait_time.as_secs_f64() * 1000.0);
        }
        self.by_sha
    }

    /// The chunk ids of a file, after its new chunks are queued for upload.
    pub(crate) fn file(&mut self, sha: &str, data: &[u8]) -> Result<Vec<String>> {
        if let Some(ids) = self.by_sha.get(sha).cloned() {
            // Equal content: equal chunks. They are still checked before the tree publishes.
            return Ok(ids);
        }
        let mut ids = Vec::new();
        let started = std::time::Instant::now();
        let pieces: Vec<(String, &[u8])> =
            chunks::split(data).into_iter().map(|chunk| (chunks::id(&self.key, chunk), chunk)).collect();
        self.split_time += started.elapsed();
        for (id, chunk) in pieces {
            self.add(&id, chunk, data.len())?;
            ids.push(id);
        }
        self.by_sha.insert(sha.to_string(), ids.clone());
        Ok(ids)
    }

    /// Adds a chunk of a file of `held` bytes that the caller holds while the packs go up.
    fn add(&mut self, id: &str, chunk: &[u8], held: usize) -> Result<()> {
        if self.written.contains(id) {
            return Ok(());
        }
        if self.remote.contains_key(id) {
            return Ok(());
        }
        self.builder.add(id.to_string(), chunk);
        self.written.insert(id.to_string());
        if self.builder.full() {
            self.send(true, (pack_charge() + held) as u64)?;
        }
        Ok(())
    }

    /// Queues the open pack (`pack`), then waits until `room` more bytes fit: a next pack and the file that the caller
    /// holds, or (`u64::MAX`) every pack is up.
    fn send(&mut self, pack: bool, room: u64) -> Result<()> {
        let started = std::time::Instant::now();
        let (bytes, packs) = (&mut self.bytes, &mut self.packs);
        let mut uploaded = |_, size| {
            *bytes += size;
            *packs += 1;
            Ok(())
        };
        if pack && !self.builder.is_empty() {
            self.uploads.submit(std::mem::take(&mut self.builder), pack_charge() as u64, &mut uploaded)?;
        }
        self.uploads.wait_for(room, &mut uploaded)?;
        self.wait_time += started.elapsed();
        Ok(())
    }

    /// Makes room for a file of `size` bytes before the caller reads it, beside the pack being built. A file larger
    /// than the budget waits until no pack travels.
    pub(crate) fn reserve(&mut self, size: u64) -> Result<()> {
        self.send(false, (pack_charge() as u64).saturating_add(size))
    }

    /// Publishes the open pack and waits until every pack of this push is published.
    pub(crate) fn flush(&mut self) -> Result<()> {
        self.send(true, u64::MAX)
    }

    /// Publishes the last pack, then checks that every chunk of the tree (`chunked`) that this push did not write is
    /// still published: a deletion may have released it since the push started, or before (a delta copies the ids
    /// of its unchanged files from a base tree that a sweep kept only as a base). Missing chunks go up again from the
    /// files in `root`. A deletion between this check and the tree publication remains possible; `fsck` reports such
    /// a tree and the next push of its line repairs it.
    pub(crate) fn finish(
        &mut self,
        root: &Path,
        chunked: &BTreeMap<String, Vec<String>>,
        content: &dyn Fn(&Path, &str) -> Result<Vec<u8>>,
    ) -> Result<()> {
        self.flush()?;
        if chunked.values().flatten().all(|id| self.written.contains(id)) {
            return Ok(());
        }
        let now = places(&self.store.backend, Some(&self.store.dir), &self.identity)?;
        let missing: BTreeSet<String> =
            chunked.values().flatten().filter(|id| !self.written.contains(*id) && !now.contains_key(*id)).cloned().collect();
        if missing.is_empty() {
            return Ok(());
        }
        self.remote = now;
        for (path, ids) in chunked {
            if !ids.iter().any(|id| missing.contains(id)) {
                continue;
            }
            let size = std::fs::symlink_metadata(root.join(path)).map_or(0, |meta| meta.len());
            self.reserve(size)?;
            let data = content(root, path)?;
            for chunk in chunks::split(&data) {
                let id = chunks::id(&self.key, chunk);
                if missing.contains(&id) {
                    self.add(&id, chunk, data.len())?;
                }
            }
        }
        self.flush()?;
        if let Some(id) = missing.iter().find(|id| !self.written.contains(*id)) {
            bail!("chunk {id} was released during the push and its file changed; retry the push");
        }
        Ok(())
    }
}

/// Reads chunked files of a layer. A plan (`Reader::plan`) gives the order of the chunks that the caller reads next;
/// their packs load ahead of the caller on a transfer pool. A pack outside the plan, or dropped behind the reader,
/// loads when it is needed.
pub(crate) struct Reader<'a> {
    backend: &'a Backend,
    identity: String,
    places: Option<Places>,
    indexes: std::sync::Arc<BTreeMap<String, Index>>,
    packs: BTreeMap<String, std::sync::Arc<Vec<u8>>>,
    pub(super) dir: Option<PathBuf>,
}

const KEEP_BEHIND: usize = 4;
const READER_CACHE: usize = 8;
/// Retained decoded packs use `(KEEP_BEHIND + READER_CACHE + ahead) * PACK_LIMIT`.
/// Active decoders (including the synchronous reader) each reserve twice the sum of the
/// decoded, compressed, and encrypted size bounds for geometric Vec growth. The total
/// is at most TRANSFER_BUFFER_BUDGET; queued window jobs do not hold pack buffers.
fn ahead_limit(workers: usize) -> usize {
    let active = (workers + 1) * pack_charge();
    (TRANSFER_BUFFER_BUDGET - active) / chunks::PACK_LIMIT - KEEP_BEHIND - READER_CACHE
}

/// Runs as the load of a pack starts, by pack: tests hold loads back and see which ones start.
#[cfg(test)]
type PackHook = std::sync::Arc<dyn Fn() + Send + Sync>;
#[cfg(test)]
static PACK_HOOKS: std::sync::Mutex<BTreeMap<String, PackHook>> = std::sync::Mutex::new(BTreeMap::new());

/// One decoded pack; None when its data is gone.
fn decode(backend: &Backend, identity: &str, index: &Index, pack: &str) -> Result<Option<Vec<u8>>> {
    #[cfg(test)]
    {
        let hook = PACK_HOOKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(pack).cloned();
        if let Some(hook) = hook {
            hook();
        }
    }
    let Some(body) = backend.get_within(&chunks::pack_name(pack), Some(pack_bounds().1 as u64))? else {
        return Ok(None);
    };
    let mut decoded = chunks::open_pack(&keys::decrypt(identity, &body)?, index)?;
    decoded.shrink_to_fit();
    Ok(Some(decoded))
}

type Decoded = (usize, Option<Vec<u8>>);

/// The packs of a plan, by read position: they load on one transfer pool for the whole read, at most `ahead` of the
/// caller, and the caller waits only for the pack that it reads.
struct Ahead<'scope, 'env> {
    pool: super::download::Pool<'scope, 'env, usize, Decoded>,
    position: BTreeMap<String, usize>,
    ahead: usize,
    /// Positions on the pool, the next one to submit, and decoded packs (None: the load failed) by position.
    pending: BTreeSet<usize>,
    next: usize,
    window: BTreeMap<usize, Option<std::sync::Arc<Vec<u8>>>>,
}

impl Ahead<'_, '_> {
    /// A decoded pack reached the caller.
    fn landed(pending: &mut BTreeSet<usize>, window: &mut BTreeMap<usize, Option<std::sync::Arc<Vec<u8>>>>, decoded: Decoded) {
        pending.remove(&decoded.0);
        window.insert(decoded.0, decoded.1.map(std::sync::Arc::new));
    }

    /// The decoded pack of the plan. Before, the pool takes the packs up to `ahead` after it. None when the pack is
    /// not in the plan, failed, or was dropped behind the reader: the caller loads it itself and reports.
    fn take(&mut self, pack: &str) -> Result<Option<std::sync::Arc<Vec<u8>>>> {
        let Some(&at) = self.position.get(pack) else {
            return Ok(None);
        };
        // Keep a few packs behind the reader: deduplicated chunks often point back.
        self.window.retain(|position, _| position + KEEP_BEHIND >= at);
        self.next = self.next.max(at);
        while self.next < self.position.len().min(at + self.ahead) {
            self.pending.insert(self.next);
            let landed = &mut |_, decoded| Ok(Self::landed(&mut self.pending, &mut self.window, decoded));
            self.pool.submit(self.next, pack_charge() as u64, landed)?;
            self.next += 1;
        }
        while self.pending.contains(&at) {
            self.pool.next_result(&mut |_, decoded| Ok(Self::landed(&mut self.pending, &mut self.window, decoded)))?;
        }
        Ok(self.window.get(&at).cloned().flatten())
    }
}

/// A reader with a plan: see `Reader::plan`.
pub(crate) struct Planned<'r, 'a, 'scope, 'env> {
    reader: &'r mut Reader<'a>,
    ahead: Ahead<'scope, 'env>,
}

impl Planned<'_, '_, '_, '_> {
    /// The content of the next chunked file of the plan (`Reader::file`).
    pub(crate) fn next_file(&mut self, ids: &[String], size: u64) -> Result<Vec<u8>> {
        self.reader.read(ids, size, Some(&mut self.ahead))
    }
}

impl<'a> Reader<'a> {
    pub(crate) fn new(backend: &'a Backend, identity: &str) -> Self {
        Self {
            backend,
            identity: identity.to_string(),
            places: None,
            indexes: std::sync::Arc::new(BTreeMap::new()),
            packs: BTreeMap::new(),
            dir: None,
        }
    }

    fn places(&mut self, fresh: bool) -> Result<&Places> {
        if self.places.is_none() || fresh {
            self.indexes = std::sync::Arc::new(indexes(self.backend, self.dir.as_deref(), &self.identity)?);
            self.places = Some(places_of(&self.indexes));
        }
        Ok(self.places.as_ref().unwrap())
    }

    /// Fails when a chunk of `ids` is in no published pack, before any pack is read: the layer was listed once more
    /// first, as `append` does, since a release may have moved the chunk.
    pub(crate) fn require(&mut self, ids: &[&String]) -> Result<()> {
        if ids.iter().all(|id| self.places(false).is_ok_and(|p| p.contains_key(*id))) {
            return Ok(());
        }
        let places = self.places(true)?;
        if let Some(id) = ids.iter().find(|id| !places.contains_key(**id)) {
            bail!("chunk {id} is not in the store");
        }
        Ok(())
    }

    /// The chunks that the caller reads next, in order, through the returned reader: their packs load on a transfer
    /// pool of `scope` while the caller reads, and the scope's end stops the loads that the caller no longer needs.
    pub(crate) fn plan<'r, 'i, 'scope, 'env>(
        &'r mut self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
        ids: impl IntoIterator<Item = &'i String>,
    ) -> Result<Planned<'r, 'a, 'scope, 'env>>
    where
        'a: 'scope,
    {
        self.plan_with(scope, ids, parallel())
    }

    /// `plan` with `workers` decoders.
    fn plan_with<'r, 'i, 'scope, 'env>(
        &'r mut self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
        ids: impl IntoIterator<Item = &'i String>,
        workers: usize,
    ) -> Result<Planned<'r, 'a, 'scope, 'env>>
    where
        'a: 'scope,
    {
        let places = self.places(false)?.clone();
        let mut seen = BTreeSet::new();
        let order: Vec<String> = ids
            .into_iter()
            .filter_map(|id| places.get(id).map(|place| place.pack.clone()))
            .filter(|pack| seen.insert(pack.clone()) && !self.packs.contains_key(pack))
            .collect();
        let position = order.iter().enumerate().map(|(i, pack)| (pack.clone(), i)).collect();
        let ahead = ahead_limit(workers);
        let (backend, identity, indexes) = (self.backend, self.identity.clone(), self.indexes.clone());
        // A failure (a release moved the chunks, a read error) is not reported here: the reader loads the pack itself
        // and reports, or lists the packs again.
        let load = move |at: usize| -> Result<Decoded> {
            let name = &order[at];
            let data = match indexes.get(name) {
                Some(index) => decode(backend, &identity, index, name),
                None => index(backend, None, &identity, name).and_then(|index| decode(backend, &identity, &index, name)),
            };
            Ok((at, data.ok().flatten()))
        };
        // Up to `ahead` packs wait on the pool: only `workers` decode at a time, and a queued job holds no buffer.
        let pool = super::download::Pool::new(scope, workers, (ahead * pack_charge()) as u64, load);
        let window = BTreeMap::new();
        let ahead = Ahead { pool, position, ahead, pending: BTreeSet::new(), next: 0, window };
        Ok(Planned { reader: self, ahead })
    }

    /// One pack outside the window, loaded now. At most a few stay in memory.
    fn load(&mut self, pack: &str) -> Result<Option<std::sync::Arc<Vec<u8>>>> {
        if let Some(data) = self.packs.get(pack) {
            return Ok(Some(data.clone()));
        }
        let index = match self.indexes.get(pack) {
            Some(index) => index.clone(),
            None => index(self.backend, self.dir.as_deref(), &self.identity, pack)?,
        };
        let Some(decoded) = decode(self.backend, &self.identity, &index, pack)? else {
            return Ok(None);
        };
        let data = std::sync::Arc::new(decoded);
        if self.packs.len() >= READER_CACHE {
            self.packs.clear();
        }
        self.packs.insert(pack.to_string(), data.clone());
        Ok(Some(data))
    }

    /// Appends chunk `id` to `out`, which may not pass `size` bytes.
    fn append(&mut self, id: &str, out: &mut Vec<u8>, size: u64, mut ahead: Option<&mut Ahead<'_, '_>>) -> Result<()> {
        // A release may move a chunk after this reader listed the packs: list once more.
        for attempt in 0..2 {
            let Some(place) = self.places(attempt == 1)?.get(id).cloned() else {
                continue;
            };
            let prefetched = match (self.packs.get(&place.pack), ahead.as_deref_mut()) {
                (Some(data), _) => Some(data.clone()),
                (None, Some(ahead)) if attempt == 0 => ahead.take(&place.pack)?,
                (None, _) => None,
            };
            let data = match prefetched {
                Some(data) => {
                    if !self.packs.contains_key(&place.pack) && self.packs.len() >= READER_CACHE {
                        self.packs.clear();
                    }
                    self.packs.insert(place.pack.clone(), data.clone());
                    data
                }
                None => match self.load(&place.pack)? {
                    Some(data) => data,
                    None => continue,
                },
            };
            if out.len() as u64 + place.length > size {
                bail!("chunks exceed their file size");
            }
            let start = usize::try_from(place.offset)?;
            let end = usize::try_from(place.offset + place.length)?;
            out.extend_from_slice(data.get(start..end).context("chunk is outside its pack")?);
            return Ok(());
        }
        bail!("chunk {id} is not in the store")
    }

    fn read(&mut self, ids: &[String], size: u64, mut ahead: Option<&mut Ahead<'_, '_>>) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0).min(64 << 20));
        for id in ids {
            self.append(id, &mut data, size, ahead.as_deref_mut())?;
        }
        Ok(data)
    }

    /// The content of a chunked file. The caller checks its size and SHA-256 against the tree.
    pub(crate) fn file(&mut self, ids: &[String], size: u64) -> Result<Vec<u8>> {
        self.read(ids, size, None)
    }
}

/// The deletion hook (docs/cache-overlays.md, deletion of cache states): after `dead` chunk ids
/// lost their last live tree, rewrite each pack that also holds live chunks into a new pack with
/// only those, then delete the pack and its index with `delete`, one pack at a time. The new pack
/// is published before its old pack goes, so a stop leaves at worst a duplicate of the live chunks
/// of one pack. `delete` false: the object stays, and the release stops there; a later pass
/// releases the packs after it, and finds the whole copy of the stopped pack (`copy_prefix`)
/// instead of copying it again. A live chunk is copied even when another pack holds it too: a
/// sweep on another machine may release that pack at the same time, relying on this one. Only the
/// packs of `indexes`, the ones that the caller found `dead` in, are touched: a pack published
/// since (a push in flight that uploads a dead chunk again) stays.
pub(crate) fn release(
    store: &Store,
    identity: &str,
    recipient: &str,
    indexes: &BTreeMap<String, Index>,
    dead: &BTreeSet<String>,
    delete: &mut dyn FnMut(&str) -> bool,
) -> Result<()> {
    // The live chunks that this call copied already, into a pack that no other sweep knows of yet.
    let mut copied = BTreeSet::new();
    // The pack objects with their sizes, and the packs with an index, to find the copies of an earlier pass.
    let (mut listed, mut indexed) = (BTreeMap::<String, Option<u64>>::new(), BTreeSet::new());
    for object in store.backend.list("chunks/")? {
        match chunks::parse_name(&object.key) {
            Some((pack, true)) => {
                indexed.insert(pack);
            }
            Some((pack, false)) => {
                listed.insert(pack, object.size);
            }
            None => {}
        }
    }
    for (pack, index) in indexes {
        if !index.chunks.iter().any(|(id, _, _)| dead.contains(id)) {
            continue;
        }
        let live: Vec<_> = index.chunks.iter().filter(|(id, _, _)| !dead.contains(id) && !copied.contains(id)).cloned().collect();
        if !live.is_empty() {
            // Any copy under this prefix of an earlier pass is used when its pack object has the size in its name
            // and its index lists `live`. A copy of another size is cut short: it is not used, and its index goes
            // once a whole copy exists, so that no reader picks it. Another object under the prefix is an orphan of a
            // stopped upload.
            let prefix = copy_prefix(pack, &live);
            let same = |copy: &Index| {
                copy.chunks.iter().map(|(id, _, length)| (id, length)).eq(live.iter().map(|(id, _, length)| (id, length)))
            };
            // Whether a copy's index lists `live`: the index that the sweep read, else read now. An index that is gone or
            // does not decode is not a copy of `live`; any other failure stops the pass, so that it does not add a copy
            // next to a whole one.
            let lists_live = |id: &str| -> Result<bool> {
                if let Some(index) = indexes.get(id) {
                    return Ok(same(index));
                }
                match store.backend.decrypted(&chunks::index_name(id), identity) {
                    Ok(bytes) => Ok(Index::parse(&bytes).is_ok_and(|copy| same(&copy))),
                    Err(e) if e.downcast_ref::<Undecodable>().is_some() || e.downcast_ref::<Missing>().is_some() => Ok(false),
                    Err(e) => Err(e),
                }
            };
            let mut bad = Vec::new();
            let mut found = false;
            // Every copy under the prefix is looked at, also after a whole one: a cut copy of `live` loses its index.
            for (id, size) in listed.range(prefix.clone()..).take_while(|(id, _)| id.starts_with(&prefix)) {
                if !indexed.contains(id) || !lists_live(id)? {
                    continue;
                }
                // A listing without sizes (some gateways): the copy is read to learn its size, until a whole copy is
                // found. A failed read stops the pass, so that it does not add a copy next to a whole one.
                let size = match size {
                    Some(size) => Some(*size),
                    None if found => continue,
                    None => store
                        .backend
                        .get_within(&chunks::pack_name(id), Some(u64::try_from(pack_bounds().1)?))?
                        .map(|body| u64::try_from(body.len()))
                        .transpose()?,
                };
                match size {
                    Some(size) if Some(size) == copy_size(id) => found = true,
                    Some(_) => bad.push(id.clone()),
                    None => {}
                }
            }
            // A pack whose data is gone (a delete that removed the data and not the index) has nothing to copy: its
            // index goes too. Its live chunks are lost; a cache is regenerable, and the next push of their trees
            // reports them.
            let data = if found { None } else { decode(&store.backend, identity, index, pack)? };
            if !found && data.is_none() {
                eprintln!("layr: chunk pack {pack} has an index but no data; {} live chunk(s) of it are lost", live.len());
            }
            if let Some(data) = &data {
                let mut builder = Builder::default();
                for (id, offset, length) in &live {
                    let range = usize::try_from(*offset)?..usize::try_from(*offset + *length)?;
                    builder.add(id.clone(), data.get(range).context("chunk is outside its pack")?);
                }
                // The first free 4-digit discriminator under the prefix: 0000 when the prefix holds nothing yet.
                let used = |stem: &String| {
                    let mut ids = listed.keys().chain(&indexed);
                    ids.any(|id| id.starts_with(stem.as_str()))
                };
                let stem = (0..=0xffffu32)
                    .map(|n| format!("{prefix}{n:04x}"))
                    .find(|stem| !used(stem))
                    .context("no free id for a chunk pack copy")?;
                upload(&store.backend, recipient, &store.dir, builder, |size| format!("{stem}{size:08x}"))?;
            }
            if found || data.is_some() {
                copied.extend(live.into_iter().map(|(id, _, _)| id));
                if !bad.into_iter().all(|id| delete(&chunks::index_name(&id))) {
                    break;
                }
            }
        }
        if !delete(&chunks::index_name(pack)) || !delete(&chunks::pack_name(pack)) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn prefetch_budget_covers_all_pack_holders() {
        let plain = layr_store::chunks::PACK_LIMIT;
        let compressed = layr_store::zst::compress_bound(plain);
        let encrypted = layr_store::keys::ciphertext_len(compressed);
        for workers in 1..=super::worker_cap() {
            let retained = (super::KEEP_BEHIND + super::READER_CACHE + super::ahead_limit(workers)) * plain;
            let active = (workers + 1) * super::VEC_GROWTH_WORST * (plain + compressed + encrypted);
            assert!(retained + active <= super::TRANSFER_BUFFER_BUDGET, "{workers} workers");
        }
    }

    #[test]
    fn grease_worst_case_fits_bound() {
        use base64::Engine;
        let tag = format!("{}-grease", "x".repeat(8));
        let args = format!(" {}", "x".repeat(8)).repeat(4);
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode([0; 99]);
        let lines = encoded.as_bytes().chunks(64).count();
        let stanza_len = 3 + tag.len() + args.len() + 1 + encoded.len() + lines;
        let grease_bound = layr_store::keys::ciphertext_len(0) - 184 - 16;
        assert!(stanza_len <= grease_bound);
    }

    #[test]
    fn oversized_window_pack_is_bounded() {
        let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x88, 0x09, 0x00, 0x00, 0x61];
        let index = layr_store::chunks::Index { v: 1, chunks: vec![(String::new(), 0, 1)] };
        let error = layr_store::chunks::open_pack(&frame, &index).unwrap_err();
        assert!(error.to_string().contains("pack frame content size is missing"));
    }

    #[test]
    fn incompressible_final_chunk_reaches_pack_limit() {
        use rand::RngCore;
        let mut bytes = vec![0u8; layr_store::chunks::PACK_LIMIT - 1];
        rand::thread_rng().fill_bytes(&mut bytes);
        let mut builder = layr_store::chunks::Builder::default();
        for chunk in bytes[..layr_store::chunks::PACK_TARGET - 1].chunks(layr_store::chunks::MAX) {
            builder.add(String::new(), chunk);
        }
        assert!(!builder.full());
        builder.add(String::new(), &bytes[layr_store::chunks::PACK_TARGET - 1..]);
        assert!(builder.full());
        let (frame, index) = builder.finish().unwrap();
        assert!(frame.len() <= layr_store::zst::compress_bound(layr_store::chunks::PACK_LIMIT));
        layr_store::zst::check_pack_header(&frame, layr_store::chunks::PACK_LIMIT).unwrap();
        assert_eq!(layr_store::chunks::open_pack(&frame, &index).unwrap(), bytes);
    }

    #[test]
    fn age_bound_matches_ciphertext_size() {
        let identity = layr_store::keys::generate_identity();
        let recipient = layr_store::keys::recipient_of(&identity).unwrap();
        for size in [0, 1, 65536, 65537] {
            let body = layr_store::keys::encrypt(&recipient, &vec![0; size]).unwrap();
            assert!(body.len() <= layr_store::keys::ciphertext_len(size));
            assert_eq!(body.capacity(), layr_store::keys::ciphertext_len(size));
        }
    }

    use super::*;
    use std::sync::mpsc;

    #[test]
    fn decoded_pack_has_exact_capacity() {
        let mut builder = Builder::default();
        while !builder.full() {
            builder.add(String::new(), &[1; 256 * 1024]);
        }
        let (frame, index) = builder.finish().unwrap();
        let data = chunks::open_pack(&frame, &index).unwrap();
        assert_eq!(data.len(), data.capacity());
    }

    fn store(root: &Path) -> Store {
        Store {
            backend: Backend::file(root.join("remote")),
            url: format!("file://{}", root.join("remote").display()),
            project: "01a12290-0000-7000-8000-000000000002".into(),
            machine: "01a12290-0000-7000-8000-000000000003".into(),
            key: ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
            dir: root.join("local"),
            paper_key_text: None,
            person: None,
            pin: None,
            aws_profile: None,
            layer: None,
            epoch: None,
            epoch_public: false,
            publishing_epoch: false,
            force_public: false,
            public_clone: false,
            public_reader: std::sync::atomic::AtomicBool::new(false),
            mode: None,
        }
    }

    /// A temporary folder that goes away with the test.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn noise(len: usize, seed: u8) -> Vec<u8> {
        let mut state = seed as u64 + 1;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    }

    fn sha(data: &[u8]) -> String {
        use layr_hashers::{ContentAddress, Digest};
        hex::encode(ContentAddress::digest(data))
    }

    /// Two files in separate packs; returns their chunk ids.
    fn two_packs(store: &Store, identity: &str, recipient: &str) -> (Vec<String>, Vec<String>) {
        let key = chunks::key(identity, "epoch");
        let (one, two) = (noise(300_000, 1), noise(300_000, 2));
        std::thread::scope(|scope| {
            let mut writer = Writer::open(scope, store, recipient, identity, key, BTreeMap::new()).unwrap();
            let a = writer.file(&sha(&one), &one).unwrap();
            writer.flush().unwrap();
            let b = writer.file(&sha(&two), &two).unwrap();
            writer.flush().unwrap();
            assert_eq!(writer.packs, 2);
            (a, b)
        })
    }

    #[test]
    fn release_rewrites_live_chunks_and_is_idempotent() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let (a, b) = two_packs(&store, &identity, &recipient);
        let before = packs(&store.backend).unwrap();
        // The second chunk of the first file and every chunk of the second file die.
        let dead: BTreeSet<String> = a[1..].iter().chain(&b).cloned().collect();
        let deleted = release_now(&store, &identity, &recipient, &indexes(&store.backend, None, &identity).unwrap(), &dead);
        assert_eq!(deleted.len(), 4, "both packs and both indexes go");
        assert_eq!(packs(&store.backend).unwrap().len(), before.len() - 1, "one new pack for the live chunk");
        // A rerun finds nothing dead to release.
        assert!(release_now(&store, &identity, &recipient, &indexes(&store.backend, None, &identity).unwrap(), &dead).is_empty());
        let now = places(&store.backend, None, &identity).unwrap();
        assert_eq!(now.keys().cloned().collect::<BTreeSet<_>>(), BTreeSet::from([a[0].clone()]));
        let mut reader = Reader::new(&store.backend, &identity);
        assert_eq!(reader.file(&a[..1], 1 << 20).unwrap(), split_owned(&noise(300_000, 1))[0]);
        assert!(reader.file(&b, 1 << 20).is_err(), "a released chunk is gone");
    }

    /// A release reads a pack within the bound of its format, as a reader does: an oversized body fails before it is read.
    #[test]
    fn release_reads_a_pack_within_its_bound() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let (a, _) = two_packs(&store, &identity, &recipient);
        let pack = places(&store.backend, None, &identity).unwrap()[&a[0]].pack.clone();
        let path = root.path().join("remote").join(chunks::pack_name(&pack));
        std::fs::File::options().write(true).open(path).unwrap().set_len(pack_bounds().1 as u64 + 1).unwrap();
        let dead = BTreeSet::from([a[1].clone()]);
        let scanned = indexes(&store.backend, None, &identity).unwrap();
        let error = release(&store, &identity, &recipient, &scanned, &dead, &mut |_| true).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds its size bound"), "{error:#}");
    }

    /// A release whose new pack is not created (the remote kept an object of that name) deletes nothing: the live chunk
    /// stays in its old pack.
    #[test]
    fn a_release_whose_copy_is_not_created_deletes_nothing() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let (a, _) = two_packs(&store, &identity, &recipient);
        let faults = || super::super::backend::FILE_FAULTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        faults().insert(root.path().join("remote"), vec![("put", "chunks/".to_string())]);
        let dead = BTreeSet::from([a[1].clone()]);
        let scanned = indexes(&store.backend, None, &identity).unwrap();
        let mut deleted = Vec::new();
        let released = release(&store, &identity, &recipient, &scanned, &dead, &mut |key| {
            deleted.push(key.to_string());
            store.backend.delete(key).is_ok()
        });
        faults().remove(&root.path().join("remote"));
        assert!(released.is_err() && deleted.is_empty(), "{released:?}: {deleted:?}");
        assert_eq!(Reader::new(&store.backend, &identity).file(&a[..1], 1 << 20).unwrap(), split_owned(&noise(300_000, 1))[0]);
    }

    /// Two sweeps release two packs that both hold a live chunk, each from its own scan: neither relies on the copy
    /// in the other pack, so the chunk stays readable.
    #[test]
    fn two_sweeps_never_release_both_copies_of_a_live_chunk() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let key = chunks::key(&identity, "epoch");
        let (live, one, two) = (noise(1000, 1), noise(1000, 2), noise(1000, 3));
        let id = |data: &[u8]| chunks::id(&key, data);
        publish_pack(&store, &recipient, &[(id(&live), &live), (id(&one), &one)]);
        publish_pack(&store, &recipient, &[(id(&live), &live), (id(&two), &two)]);
        // Both scan first. One finds the first dead chunk, the other the second one.
        let scanned = indexes(&store.backend, None, &identity).unwrap();
        for dead in [&one, &two] {
            release_now(&store, &identity, &recipient, &scanned, &BTreeSet::from([id(dead)]));
        }
        let mut reader = Reader::new(&store.backend, &identity);
        assert_eq!(reader.file(&[id(&live)], 1000).unwrap(), live);
    }

    /// A release that deletes each object it is given; returns them.
    fn release_now(
        store: &Store,
        identity: &str,
        recipient: &str,
        indexes: &BTreeMap<String, Index>,
        dead: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut deleted = Vec::new();
        release(store, identity, recipient, indexes, dead, &mut |key| {
            deleted.push(key.to_string());
            store.backend.delete(key).is_ok()
        })
        .unwrap();
        deleted
    }

    /// Publishes a pack of `chunks`, by id.
    fn publish_pack(store: &Store, recipient: &str, chunks: &[(String, &Vec<u8>)]) {
        let mut builder = Builder::default();
        for (id, data) in chunks {
            builder.add(id.clone(), data);
        }
        let (frame, index) = builder.finish().unwrap();
        let name = crate::ids::new_id();
        store.backend.put_new(&chunks::pack_name(&name), keys::encrypt(recipient, &frame).unwrap()).unwrap();
        store.backend.put_new(&chunks::index_name(&name), keys::encrypt(recipient, &index.encode()).unwrap()).unwrap();
    }

    /// A release goes one pack at a time: a delete that fails at a pack stops it there, before it copies the packs
    /// after it, and the next release copies none of the packs before it again.
    #[test]
    fn a_failed_delete_stops_a_release_at_its_pack() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let key = chunks::key(&identity, "epoch");
        let id = |data: &[u8]| chunks::id(&key, data);
        let live: Vec<Vec<u8>> = (0..4).map(|n| noise(1000, n)).collect();
        let gone: Vec<Vec<u8>> = (0..4).map(|n| noise(1000, 10 + n)).collect();
        for (live, gone) in live.iter().zip(&gone) {
            publish_pack(&store, &recipient, &[(id(live), live), (id(gone), gone)]);
        }
        let dead: BTreeSet<String> = gone.iter().map(|data| id(data)).collect();
        let scanned = indexes(&store.backend, None, &identity).unwrap();
        let originals: Vec<&String> = scanned.keys().collect();
        let refused = chunks::index_name(originals[1]);
        release(&store, &identity, &recipient, &scanned, &dead, &mut |key| key != refused && store.backend.delete(key).is_ok())
            .unwrap();
        let now = packs(&store.backend).unwrap();
        assert!(!now.contains(originals[0]) && originals[1..].iter().all(|pack| now.contains(*pack)), "{now:?}");
        assert_eq!(now.len(), 3 + 2, "the copies of the first two packs, and none of the packs after them");
        release_now(&store, &identity, &recipient, &indexes(&store.backend, None, &identity).unwrap(), &dead);
        assert_eq!(packs(&store.backend).unwrap().len(), 4, "one copy of each pack: the stopped one's copy is found again");
        let mut reader = Reader::new(&store.backend, &identity);
        for data in &live {
            assert_eq!(&reader.file(&[id(data)], 1000).unwrap(), data);
        }
    }

    /// A pack whose index the remote refuses to delete stays across passes; each pass finds the copy of its live
    /// chunks that the first one published instead of adding one.
    #[test]
    fn a_release_refused_on_every_pass_copies_once() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let (a, _) = two_packs(&store, &identity, &recipient);
        let pack = places(&store.backend, None, &identity).unwrap()[&a[0]].pack.clone();
        let refused = chunks::index_name(&pack);
        let dead = BTreeSet::from([a[1].clone()]);
        let before = packs(&store.backend).unwrap();
        let remote = root.path().join("remote");
        let requests = || super::super::backend::FILE_REQUESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for pass in 0..3 {
            requests().insert(remote.clone(), Vec::new());
            let scanned = indexes(&store.backend, None, &identity).unwrap();
            release(&store, &identity, &recipient, &scanned, &dead, &mut |key| {
                key != refused && store.backend.delete(key).is_ok()
            })
            .unwrap();
            let log = requests().remove(&remote).unwrap();
            let moved = |(op, key): &&(&str, String)| {
                (*op == "get" && *key == chunks::pack_name(&pack)) || (*op == "put" && key.ends_with(".pack.age"))
            };
            // The first pass reads the old pack and writes the copy; the later ones transfer no pack.
            assert_eq!(log.iter().filter(moved).count(), if pass == 0 { 2 } else { 0 }, "pass {pass}: {log:?}");
        }
        let now = packs(&store.backend).unwrap();
        assert_eq!(now.difference(&before).count(), 1, "one copy: {now:?}");
        assert!(now.contains(&pack));
        let scanned = indexes(&store.backend, None, &identity).unwrap();
        assert_eq!(release_now(&store, &identity, &recipient, &scanned, &dead).len(), 2, "the old pack goes once allowed");
        assert_eq!(packs(&store.backend).unwrap().len(), before.len(), "and its copy stays the only one");
        assert_eq!(Reader::new(&store.backend, &identity).file(&a[..1], 1 << 20).unwrap(), split_owned(&noise(300_000, 1))[0]);
    }

    /// A copy of an earlier release that is not whole (its index is gone, or its pack object is cut short) is not
    /// used: the live chunks go to one new copy under the same prefix, which later passes find, also when the listing
    /// has no sizes. The index of the cut copy goes, so a reader takes the whole one.
    #[test]
    fn a_release_does_not_rely_on_a_partial_copy() {
        for (no_sizes, index_stays) in [(false, false), (false, true), (true, false), (true, true)] {
            let case = format!("no sizes {no_sizes}, index stays {index_stays}");
            let root = Scratch::new();
            let store = store(root.path());
            let remote = root.path().join("remote");
            let faults = || super::super::backend::FILE_FAULTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if no_sizes {
                faults().insert(remote.clone(), vec![("unsized", "chunks/".to_string())]);
            }
            let identity = keys::generate_identity();
            let recipient = keys::recipient_of(&identity).unwrap();
            let (a, _) = two_packs(&store, &identity, &recipient);
            let pack = places(&store.backend, None, &identity).unwrap()[&a[0]].pack.clone();
            let dead = BTreeSet::from([a[1].clone()]);
            let before = packs(&store.backend).unwrap();
            // A first pass publishes the copy, and the remote keeps the old pack.
            release(&store, &identity, &recipient, &indexes(&store.backend, None, &identity).unwrap(), &dead, &mut |_| false)
                .unwrap();
            let copy = packs(&store.backend).unwrap().difference(&before).next().unwrap().clone();
            if !index_stays {
                std::fs::remove_file(remote.join(chunks::index_name(&copy))).unwrap();
            }
            let file = std::fs::File::options().write(true).open(remote.join(chunks::pack_name(&copy))).unwrap();
            file.set_len(file.metadata().unwrap().len() / 2).unwrap();
            // The remote refuses to delete the old pack on each pass: the first one copies its live chunk once more,
            // the later ones find that copy.
            let requests = || super::super::backend::FILE_REQUESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let source = [chunks::index_name(&pack), chunks::pack_name(&pack)];
            for pass in 0..3 {
                requests().insert(remote.clone(), Vec::new());
                let scanned = indexes(&store.backend, None, &identity).unwrap();
                release(&store, &identity, &recipient, &scanned, &dead, &mut |key| {
                    !source.iter().any(|refused| refused == key) && store.backend.delete(key).is_ok()
                })
                .unwrap();
                let log = requests().remove(&remote).unwrap();
                let moved = |(op, key): &&(&str, String)| {
                    (*op == "get" && *key == chunks::pack_name(&pack)) || (*op == "put" && key.ends_with(".pack.age"))
                };
                assert_eq!(log.iter().filter(moved).count(), if pass == 0 { 2 } else { 0 }, "{case}, pass {pass}: {log:?}");
            }
            let now = packs(&store.backend).unwrap();
            let fresh: Vec<_> = now.iter().filter(|p| !before.contains(*p)).collect();
            assert_eq!(fresh.len(), 1, "{case}: one whole copy: {now:?}");
            assert!(*fresh[0] != copy && fresh[0][..24] == copy[..24], "{case}: {fresh:?}");
            let scanned = indexes(&store.backend, None, &identity).unwrap();
            release_now(&store, &identity, &recipient, &scanned, &dead);
            assert!(!packs(&store.backend).unwrap().contains(&pack));
            // The cut copy's object is still there; the live chunk reads from the whole one.
            assert!(remote.join(chunks::pack_name(&copy)).exists());
            assert_eq!(
                Reader::new(&store.backend, &identity).file(&a[..1], 1 << 20).unwrap(),
                split_owned(&noise(300_000, 1))[0],
                "{case}"
            );
            faults().remove(&remote);
        }
    }

    /// A retry finds the whole copy of an earlier pass through the indexes that the sweep read, with no read of the
    /// copy's index; when it must read that index and the read fails, it stops instead of copying again.
    #[test]
    fn a_release_whose_copy_index_cannot_be_read_copies_nothing() {
        let root = Scratch::new();
        let store = store(root.path());
        let remote = root.path().join("remote");
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let (a, _) = two_packs(&store, &identity, &recipient);
        let pack = places(&store.backend, None, &identity).unwrap()[&a[0]].pack.clone();
        let dead = BTreeSet::from([a[1].clone()]);
        let before = packs(&store.backend).unwrap();
        let source = [chunks::index_name(&pack), chunks::pack_name(&pack)];
        let refuse = |key: &str| !source.iter().any(|refused| refused == key) && store.backend.delete(key).is_ok();
        release(&store, &identity, &recipient, &indexes(&store.backend, None, &identity).unwrap(), &dead, &mut |key| refuse(key))
            .unwrap();
        let copy = packs(&store.backend).unwrap().difference(&before).next().unwrap().clone();
        let faults = || super::super::backend::FILE_FAULTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let requests = || super::super::backend::FILE_REQUESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let moved = |log: &[(&str, String)]| {
            log.iter()
                .filter(|(op, key)| (*op == "get" && *key == source[1]) || (*op == "put" && key.ends_with(".pack.age")))
                .count()
        };
        // The sweep read the copy's index: the retry uses it.
        let scanned = indexes(&store.backend, None, &identity).unwrap();
        faults().insert(remote.clone(), vec![("get", chunks::index_name(&copy))]);
        requests().insert(remote.clone(), Vec::new());
        release(&store, &identity, &recipient, &scanned, &dead, &mut |key| refuse(key)).unwrap();
        assert_eq!(moved(&requests().remove(&remote).unwrap()), 0);
        // The sweep did not read it, and its read fails: the pass stops before any copy.
        let mut unread = scanned.clone();
        unread.remove(&copy);
        requests().insert(remote.clone(), Vec::new());
        assert!(release(&store, &identity, &recipient, &unread, &dead, &mut |key| refuse(key)).is_err());
        assert_eq!(moved(&requests().remove(&remote).unwrap()), 0);
        faults().remove(&remote);
        assert_eq!(packs(&store.backend).unwrap().difference(&before).count(), 1, "one copy");
    }

    fn split_owned(data: &[u8]) -> Vec<Vec<u8>> {
        chunks::split(data).into_iter().map(<[u8]>::to_vec).collect()
    }

    #[test]
    fn a_chunk_released_during_a_push_goes_up_again() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let key = chunks::key(&identity, "epoch");
        let files = root.path().join("files");
        std::fs::create_dir_all(&files).unwrap();
        let data = noise(300_000, 3);
        std::fs::write(files.join("a"), &data).unwrap();
        let ids = std::thread::scope(|scope| {
            let mut first = Writer::open(scope, &store, &recipient, &identity, key, BTreeMap::new()).unwrap();
            let ids = first.file(&sha(&data), &data).unwrap();
            first.flush().unwrap();
            // A second push knows the file from the first push's tree and reuses every chunk, then a
            // deletion releases them before it publishes.
            let mut second = Writer::open(scope, &store, &recipient, &identity, key, first.into_hashes()).unwrap();
            assert_eq!(second.file(&sha(&data), &data).unwrap(), ids);
            assert_eq!(second.packs, 0, "nothing new to write");
            let dead: BTreeSet<String> = ids.iter().cloned().collect();
            let indexes = indexes(&store.backend, None, &identity).unwrap();
            release_now(&store, &identity, &recipient, &indexes, &dead);
            assert!(places(&store.backend, None, &identity).unwrap().is_empty());
            let chunked = BTreeMap::from([("a".to_string(), ids.clone())]);
            second.finish(&files, &chunked, &|root: &Path, path: &str| Ok(std::fs::read(root.join(path))?)).unwrap();
            let now = places(&store.backend, None, &identity).unwrap();
            assert!(ids.iter().all(|id| now.contains_key(id)), "the push uploaded the released chunks again");
            ids
        });
        assert_eq!(Reader::new(&store.backend, &identity).file(&ids, data.len() as u64).unwrap(), data);
    }

    /// Small files, one pack each, beyond one window; returns the files and their chunk ids.
    fn one_pack_each(store: &Store, identity: &str) -> (Vec<Vec<u8>>, Vec<Vec<String>>) {
        let recipient = keys::recipient_of(identity).unwrap();
        let key = chunks::key(identity, "epoch");
        let files: Vec<Vec<u8>> =
            (0..ahead_limit(parallel()) + 3).map(|i| noise(1000, u8::try_from(i % 256).unwrap_or_default())).collect();
        let ids = std::thread::scope(|scope| {
            let mut writer = Writer::open(scope, store, &recipient, identity, key, BTreeMap::new()).unwrap();
            let ids = files
                .iter()
                .map(|data| {
                    let ids = writer.file(&sha(data), data).unwrap();
                    writer.flush().unwrap();
                    ids
                })
                .collect();
            assert_eq!(writer.packs, files.len());
            ids
        });
        (files, ids)
    }

    /// The plan loads on the transfer pool (no reader threads of its own): a read queues the packs after it, packs
    /// behind the reader go, and a pack read again after that loads alone.
    #[test]
    fn planned_packs_load_ahead_on_the_pool() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let ahead = ahead_limit(parallel());
        let (files, ids) = one_pack_each(&store, &identity);
        let mut reader = Reader::new(&store.backend, &identity);
        std::thread::scope(|scope| {
            let mut planned = reader.plan(scope, ids.iter().flatten()).unwrap();
            assert_eq!(planned.next_file(&ids[0], 1000).unwrap(), files[0]);
            let queued = planned.ahead.window.len() + planned.ahead.pending.len();
            assert_eq!(queued, ahead, "the first read queues a whole window");
            for (data, ids) in files.iter().zip(&ids).skip(1) {
                assert_eq!(&planned.next_file(ids, 1000).unwrap(), data);
            }
            assert!(planned.ahead.window.len() <= KEEP_BEHIND + 1, "packs behind the reader go");
            assert_eq!(planned.next_file(&ids[0], 1000).unwrap(), files[0], "a dropped pack loads again");
        });
    }

    /// Holds back the loads of `packs` until the returned sender goes, and reports each load as it starts. A load
    /// held 10 s gives up and sets the returned flag: the test failed to let it go.
    fn hold(packs: &[String]) -> (mpsc::Sender<()>, mpsc::Receiver<String>, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let (release, gate) = mpsc::channel::<()>();
        let gate = std::sync::Arc::new(std::sync::Mutex::new(gate));
        let (started, starts) = mpsc::channel();
        let stuck = std::sync::Arc::<std::sync::atomic::AtomicBool>::default();
        for pack in packs {
            let (gate, started, stuck, name) = (gate.clone(), started.clone(), stuck.clone(), pack.clone());
            let hook = move || {
                started.send(name.clone()).unwrap();
                let held = gate.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10));
                if held == Err(mpsc::RecvTimeoutError::Timeout) {
                    stuck.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            };
            PACK_HOOKS.lock().unwrap().insert(pack.clone(), std::sync::Arc::new(hook));
        }
        (release, starts, stuck)
    }

    /// The pack of each file of `one_pack_each`.
    fn packs_of(store: &Store, identity: &str, ids: &[Vec<String>]) -> Vec<String> {
        let places = places(&store.backend, None, identity).unwrap();
        ids.iter().map(|ids| places[&ids[0]].pack.clone()).collect()
    }

    /// A read waits for its own pack, not for the other packs of the window after it: writing a file overlaps the
    /// loads of the next ones. Two decoders: the second pack loads beside the first and is held until the first file
    /// is read.
    #[test]
    fn a_read_waits_only_for_its_own_pack() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let (files, ids) = one_pack_each(&store, &identity);
        let packs = packs_of(&store, &identity, &ids);
        let (release, starts, stuck) = hold(&packs[1..2]);
        let mut reader = Reader::new(&store.backend, &identity);
        std::thread::scope(|scope| {
            let mut planned = reader.plan_with(scope, ids.iter().flatten(), 2).unwrap();
            assert_eq!(planned.next_file(&ids[0], 1000).unwrap(), files[0]);
            drop(release);
            for (data, ids) in files.iter().zip(&ids).skip(1) {
                assert_eq!(&planned.next_file(ids, 1000).unwrap(), data);
            }
        });
        assert_eq!(starts.try_iter().collect::<Vec<_>>(), &packs[1..2]);
        assert!(!stuck.load(std::sync::atomic::Ordering::SeqCst), "the first file waited for the second pack");
    }

    /// A restore that stops after its first file ends the scope of its plan: the loads that it queued and that did not
    /// start are skipped, and the scope joins the running ones. One decoder holds the second pack; the third waits
    /// queued when the plan goes, and never loads.
    #[test]
    fn a_plan_dropped_after_one_file_stops_its_loads() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let (files, ids) = one_pack_each(&store, &identity);
        let packs = packs_of(&store, &identity, &ids);
        let (release, starts, stuck) = hold(&packs[1..]);
        let mut reader = Reader::new(&store.backend, &identity);
        std::thread::scope(|scope| {
            let mut planned = reader.plan_with(scope, ids.iter().flatten(), 1).unwrap();
            assert_eq!(planned.next_file(&ids[0], 1000).unwrap(), files[0]);
            assert_eq!(starts.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), packs[1]);
            assert!(planned.ahead.pending.contains(&2), "the third pack waits queued");
            drop(planned);
            drop(release);
        });
        assert_eq!(starts.try_iter().collect::<Vec<_>>(), Vec::<String>::new(), "a queued load ran after its plan went");
        assert!(!stuck.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn a_pack_without_its_index_is_not_a_place() {
        let root = Scratch::new();
        let store = store(root.path());
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let (a, _) = two_packs(&store, &identity, &recipient);
        let pack = places(&store.backend, None, &identity).unwrap()[&a[0]].pack.clone();
        store.backend.delete(&chunks::index_name(&pack)).unwrap();
        assert!(!places(&store.backend, None, &identity).unwrap().contains_key(&a[0]));
    }
}
