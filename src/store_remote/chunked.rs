//! Content-defined chunks of cache layers in pack objects (`layr_store::chunks`).
//!
//! A cache state loses its base at the next save (`history=last`), so a push cannot make a delta
//! against the old content. Chunks need only the ids that the store has: a changed build output
//! file shares most of its chunks with files that the store already holds, under any path.
//!
//! The store keeps no mutable chunk map. The pack indexes under `chunks/` are the truth: a writer
//! lists them to dedup, a reader lists them to find a chunk, and `release` rewrites a pack whose
//! chunks are partly dead into a new pack before the old one goes.

use super::{backend::Backend, keys, Store};
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

/// The packs of a layer whose index is published. A pack without its index is an unfinished
/// upload: no tree can name its chunks, and the deletion path removes it.
fn packs(backend: &Backend) -> Result<BTreeSet<String>> {
    let mut indexes = BTreeSet::new();
    for object in backend.list("chunks/")? {
        if let Some((pack, true)) = chunks::parse_name(&object.key) {
            indexes.insert(pack);
        }
    }
    Ok(indexes)
}

fn cache_dir(dir: &Path) -> PathBuf {
    dir.join("chunk-indexes")
}

/// One pack index, from the local cache or the store. Indexes never change after publication.
/// `dir` is the local store folder that caches indexes; a reader without one reads them each time.
pub(crate) fn index(backend: &Backend, dir: Option<&Path>, identity: &str, pack: &str) -> Result<Index> {
    let cached = dir.map(|dir| cache_dir(dir).join(pack));
    if let Some(Ok(bytes)) = cached.as_ref().map(std::fs::read) {
        if let Ok(index) = Index::parse(&bytes) {
            return Ok(index);
        }
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
    let packs = packs(backend)?;
    // Forget cached indexes of packs that are gone, so the cache does not grow.
    if let Some(Ok(entries)) = dir.map(|dir| std::fs::read_dir(cache_dir(dir))) {
        for entry in entries.flatten() {
            if !packs.contains(entry.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // Indexes are small: fetch the missing ones many at a time.
    let packs: Vec<String> = packs.into_iter().collect();
    let mut out = BTreeMap::new();
    for batch in packs.chunks(64) {
        let loaded: Vec<Result<Index>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch.iter().map(|pack| scope.spawn(move || index(backend, dir, identity, pack))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("index read panicked")))).collect()
        });
        for (pack, index) in batch.iter().zip(loaded) {
            out.insert(pack.clone(), index?);
        }
    }
    Ok(out)
}

/// Uploads the chunks of the files of one state.
pub(crate) struct Writer<'a> {
    store: &'a Store,
    identity: String,
    key: [u8; 32],
    /// Published chunks when the push started.
    remote: Places,
    /// Chunks in packs of this push, published or in the open builder.
    written: BTreeSet<String>,
    /// Chunks of `remote` that this push names without writing them again.
    reused: BTreeSet<String>,
    /// File content (SHA-256) to its chunk ids, from the base tree and this push.
    by_sha: BTreeMap<String, Vec<String>>,
    builder: Builder,
    /// Compresses, encrypts and uploads full packs in the background while files are chunked.
    uploader: Uploader,
    /// Compressed bytes written.
    pub bytes: u64,
    pub packs: usize,
    /// Time in chunking and ids, and in waits for the upload threads, for `LAYR_TRACE`.
    split_time: std::time::Duration,
    wait_time: std::time::Duration,
}

/// A pool of upload threads. A bounded queue keeps at most two packs per thread in memory.
struct Uploader {
    jobs: Option<std::sync::mpsc::SyncSender<Builder>>,
    results: std::sync::mpsc::Receiver<Result<u64>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    sent: usize,
}

impl Uploader {
    fn new(backend: &Backend, recipient: &str, dir: &Path) -> Self {
        let threads = parallel();
        let (jobs, queue) = std::sync::mpsc::sync_channel::<Builder>(threads);
        let queue = std::sync::Arc::new(std::sync::Mutex::new(queue));
        let (done, results) = std::sync::mpsc::channel();
        let threads = (0..threads)
            .map(|_| {
                let (queue, done, backend, recipient, dir) =
                    (queue.clone(), done.clone(), backend.clone(), recipient.to_string(), dir.to_path_buf());
                std::thread::spawn(move || loop {
                    let Ok(builder) = queue.lock().map_err(|_| ()).and_then(|q| q.recv().map_err(|_| ())) else {
                        return;
                    };
                    let _ = done.send(upload(&backend, &recipient, &dir, builder));
                })
            })
            .collect();
        Self { jobs: Some(jobs), results, threads, sent: 0 }
    }
    fn send(&mut self, builder: Builder) -> Result<()> {
        self.jobs.as_ref().context("uploader is closed")?.send(builder).map_err(|_| anyhow::anyhow!("pack upload threads stopped"))?;
        self.sent += 1;
        Ok(())
    }
    /// Waits for every sent pack. Returns the bytes and the number of packs.
    fn wait(&mut self) -> Result<(u64, usize)> {
        let (mut bytes, mut packs) = (0, 0);
        let mut first_error = None;
        while self.sent > 0 {
            let result = self.results.recv().map_err(|_| anyhow::anyhow!("pack upload threads stopped"))?;
            self.sent -= 1;
            match result {
                Ok(b) => {
                    bytes += b;
                    packs += 1;
                }
                Err(e) => first_error = first_error.or(Some(e)),
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok((bytes, packs)),
        }
    }
}

impl Drop for Uploader {
    fn drop(&mut self) {
        self.jobs = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Publishes one pack: the pack first, then its index, which makes it visible.
fn upload(backend: &Backend, recipient: &str, dir: &Path, builder: Builder) -> Result<u64> {
    let (frame, index) = builder.finish()?;
    let pack = crate::ids::new_id();
    let body = keys::encrypt(recipient, &frame)?;
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

/// Packs compressed and uploaded at once: zstd at the pack level takes about 0.1 s per MiB on
/// one core, so a cache of some GiB needs every core.
fn parallel() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 16)
}

impl<'a> Writer<'a> {
    /// `by_sha` maps file content to the chunk ids of pushed trees; equal files skip chunking.
    pub(crate) fn open(
        store: &'a Store,
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
            reused: BTreeSet::new(),
            by_sha,
            builder: Builder::default(),
            uploader: Uploader::new(&store.backend, recipient, &store.dir),
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

    /// A file that the walk takes from the pushed tree without reading it: its chunks count as
    /// reused, so `finish` checks that the store still has them.
    pub(crate) fn reuse(&mut self, sha: &str, ids: &[String]) {
        for id in ids {
            if !self.written.contains(id) {
                self.reused.insert(id.clone());
            }
        }
        self.by_sha.entry(sha.to_string()).or_insert_with(|| ids.to_vec());
    }

    /// The chunk ids of a file, after its new chunks are queued for upload.
    pub(crate) fn file(&mut self, sha: &str, data: &[u8]) -> Result<Vec<String>> {
        if let Some(ids) = self.by_sha.get(sha).cloned() {
            // Equal content: equal chunks. They are still checked before the tree publishes.
            for id in &ids {
                if !self.written.contains(id) {
                    self.reused.insert(id.clone());
                }
            }
            return Ok(ids);
        }
        let mut ids = Vec::new();
        let started = std::time::Instant::now();
        let pieces: Vec<(String, &[u8])> = chunks::split(data).into_iter().map(|chunk| (chunks::id(&self.key, chunk), chunk)).collect();
        self.split_time += started.elapsed();
        for (id, chunk) in pieces {
            self.add(&id, chunk)?;
            ids.push(id);
        }
        self.by_sha.insert(sha.to_string(), ids.clone());
        Ok(ids)
    }

    fn add(&mut self, id: &str, chunk: &[u8]) -> Result<()> {
        if self.written.contains(id) {
            return Ok(());
        }
        if self.remote.contains_key(id) {
            self.reused.insert(id.to_string());
            return Ok(());
        }
        self.builder.add(id.to_string(), chunk);
        self.written.insert(id.to_string());
        if self.builder.full() {
            let started = std::time::Instant::now();
            self.uploader.send(std::mem::take(&mut self.builder))?;
            self.wait_time += started.elapsed();
        }
        Ok(())
    }

    /// Publishes the open pack and waits until every pack of this push is published.
    pub(crate) fn flush(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        if !self.builder.is_empty() {
            self.uploader.send(std::mem::take(&mut self.builder))?;
        }
        let (bytes, packs) = self.uploader.wait()?;
        self.wait_time += started.elapsed();
        self.bytes += bytes;
        self.packs += packs;
        Ok(())
    }

    /// Publishes the last pack, then checks that every reused chunk is still published: a
    /// deletion may have released it since the push started. Missing chunks go up again from the
    /// files in `root`. A deletion between this check and the tree publication remains possible;
    /// `fsck` reports such a tree and the next push of its line repairs it.
    pub(crate) fn finish(&mut self, root: &Path, chunked: &BTreeMap<String, Vec<String>>, content: &dyn Fn(&Path, &str) -> Result<Vec<u8>>) -> Result<()> {
        self.flush()?;
        if self.reused.is_empty() {
            return Ok(());
        }
        let now = places(&self.store.backend, Some(&self.store.dir), &self.identity)?;
        let missing: BTreeSet<String> = self.reused.iter().filter(|id| !now.contains_key(*id)).cloned().collect();
        if missing.is_empty() {
            return Ok(());
        }
        self.remote = now;
        self.reused.retain(|id| !missing.contains(id));
        for (path, ids) in chunked {
            if !ids.iter().any(|id| missing.contains(id)) {
                continue;
            }
            let data = content(root, path)?;
            for chunk in chunks::split(&data) {
                let id = chunks::id(&self.key, chunk);
                if missing.contains(&id) {
                    self.add(&id, chunk)?;
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

/// Reads chunked files of a layer. `plan` gives the order of the chunks that the caller reads;
/// background threads then load and decode the packs of that order ahead of the caller, within a
/// memory bound. A pack outside the plan, or evicted, loads when it is needed.
pub(crate) struct Reader<'a> {
    backend: &'a Backend,
    identity: String,
    places: Option<Places>,
    indexes: std::sync::Arc<BTreeMap<String, Index>>,
    packs: BTreeMap<String, std::sync::Arc<Vec<u8>>>,
    prefetch: Option<Prefetch>,
}

/// Packs that the prefetch keeps decoded ahead of the reader: about 256 MiB.
const AHEAD: usize = 64;

/// Decoded packs by position in the plan, and the positions that finished (loaded or failed).
type Ready = (BTreeMap<usize, std::sync::Arc<Vec<u8>>>, BTreeSet<usize>);

struct Shared {
    order: Vec<String>,
    /// Decoded packs by position in `order`, and the positions that finished (loaded or failed).
    ready: std::sync::Mutex<Ready>,
    changed: std::sync::Condvar,
    /// The position of the pack that the reader needs now.
    reader: std::sync::atomic::AtomicUsize,
    next: std::sync::atomic::AtomicUsize,
    stop: std::sync::atomic::AtomicBool,
}

struct Prefetch {
    shared: std::sync::Arc<Shared>,
    position: BTreeMap<String, usize>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Prefetch {
    fn start(backend: &Backend, identity: &str, indexes: std::sync::Arc<BTreeMap<String, Index>>, order: Vec<String>) -> Self {
        use std::sync::atomic::Ordering;
        let position = order.iter().enumerate().map(|(i, pack)| (pack.clone(), i)).collect();
        let shared = std::sync::Arc::new(Shared {
            order,
            ready: std::sync::Mutex::new((BTreeMap::new(), BTreeSet::new())),
            changed: std::sync::Condvar::new(),
            reader: std::sync::atomic::AtomicUsize::new(0),
            next: std::sync::atomic::AtomicUsize::new(0),
            stop: std::sync::atomic::AtomicBool::new(false),
        });
        let threads = (0..parallel())
            .map(|_| {
                let (shared, backend, identity, indexes) = (shared.clone(), backend.clone(), identity.to_string(), indexes.clone());
                std::thread::spawn(move || loop {
                    let at = shared.next.fetch_add(1, Ordering::SeqCst);
                    if at >= shared.order.len() {
                        return;
                    }
                    // Stay within AHEAD packs of the reader.
                    {
                        let mut guard = shared.ready.lock().unwrap();
                        while at >= shared.reader.load(Ordering::SeqCst) + AHEAD && !shared.stop.load(Ordering::SeqCst) {
                            guard = shared.changed.wait(guard).unwrap();
                        }
                    }
                    if shared.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let pack = &shared.order[at];
                    let loaded = (|| -> Result<Vec<u8>> {
                        let index = match indexes.get(pack) {
                            Some(index) => index.clone(),
                            None => index(&backend, None, &identity, pack)?,
                        };
                        let body = backend.get(&chunks::pack_name(pack))?.with_context(|| format!("chunk pack {pack} is missing"))?;
                        chunks::open_pack(&keys::decrypt(&identity, &body)?, &index)
                    })();
                    let mut guard = shared.ready.lock().unwrap();
                    // A failure (a release moved the chunks, a read error) is not reported here:
                    // the reader loads the pack itself and reports, or lists the packs again.
                    if let Ok(data) = loaded {
                        guard.0.insert(at, std::sync::Arc::new(data));
                    }
                    guard.1.insert(at);
                    shared.changed.notify_all();
                })
            })
            .collect();
        Self { shared, position, threads }
    }

    /// The decoded pack, waiting for the prefetch when the pack is in the plan. None when the pack
    /// is not in the plan, failed, or was dropped behind the reader: the caller loads it itself.
    fn take(&self, pack: &str) -> Option<std::sync::Arc<Vec<u8>>> {
        use std::sync::atomic::Ordering;
        let &at = self.position.get(pack)?;
        self.shared.reader.fetch_max(at, Ordering::SeqCst);
        self.shared.changed.notify_all();
        let mut guard = self.shared.ready.lock().unwrap();
        loop {
            if let Some(data) = guard.0.get(&at).cloned() {
                // Keep a few packs behind the reader: deduplicated chunks often point back.
                let keep = self.shared.reader.load(Ordering::SeqCst).saturating_sub(4);
                guard.0.retain(|position, _| *position >= keep);
                return Some(data);
            }
            if guard.1.contains(&at) || self.shared.stop.load(Ordering::SeqCst) {
                return None;
            }
            guard = self.shared.changed.wait(guard).unwrap();
        }
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        self.shared.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.shared.changed.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
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
            prefetch: None,
        }
    }

    fn places(&mut self, fresh: bool) -> Result<&Places> {
        if self.places.is_none() || fresh {
            self.indexes = std::sync::Arc::new(indexes(self.backend, None, &self.identity)?);
            self.places = Some(places_of(&self.indexes));
        }
        Ok(self.places.as_ref().unwrap())
    }

    /// The chunks that the caller reads next, in order: their packs load in the background.
    pub(crate) fn plan<'i>(&mut self, ids: impl IntoIterator<Item = &'i String>) -> Result<()> {
        let places = self.places(false)?.clone();
        let mut seen = BTreeSet::new();
        let order: Vec<String> = ids
            .into_iter()
            .filter_map(|id| places.get(id).map(|place| place.pack.clone()))
            .filter(|pack| seen.insert(pack.clone()))
            .collect();
        self.prefetch = None;
        if !order.is_empty() {
            self.prefetch = Some(Prefetch::start(self.backend, &self.identity, self.indexes.clone(), order));
        }
        Ok(())
    }

    /// One pack outside the prefetch, loaded now. At most a few stay in memory.
    fn load(&mut self, pack: &str) -> Result<Option<std::sync::Arc<Vec<u8>>>> {
        if let Some(data) = self.packs.get(pack) {
            return Ok(Some(data.clone()));
        }
        let index = match self.indexes.get(pack) {
            Some(index) => index.clone(),
            None => index(self.backend, None, &self.identity, pack)?,
        };
        let Some(body) = self.backend.get(&chunks::pack_name(pack))? else {
            return Ok(None);
        };
        let data = std::sync::Arc::new(chunks::open_pack(&keys::decrypt(&self.identity, &body)?, &index)?);
        if self.packs.len() >= 8 {
            self.packs.clear();
        }
        self.packs.insert(pack.to_string(), data.clone());
        Ok(Some(data))
    }

    /// Appends chunk `id` to `out`, which may not pass `size` bytes.
    fn append(&mut self, id: &str, out: &mut Vec<u8>, size: u64) -> Result<()> {
        // A release may move a chunk after this reader listed the packs: list once more.
        for attempt in 0..2 {
            let Some(place) = self.places(attempt == 1)?.get(id).cloned() else {
                continue;
            };
            let data = match self.prefetch.as_ref().filter(|_| attempt == 0).and_then(|p| p.take(&place.pack)) {
                Some(data) => data,
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

    /// The content of a chunked file. The caller checks its size and SHA-256 against the tree.
    pub(crate) fn file(&mut self, ids: &[String], size: u64) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0).min(64 << 20));
        for id in ids {
            self.append(id, &mut data, size)?;
        }
        Ok(data)
    }
}

/// The deletion hook (docs/cache-overlays.md, deletion of cache states): after `dead` chunk ids
/// lost their last live tree, rewrite each pack that also holds live chunks into a new pack with
/// only those, and return the objects to delete: packs and indexes whose chunks are all dead or
/// copied. The new pack is published before the caller deletes anything, so a crash leaves at
/// worst a duplicate, and a rerun gives the same objects to delete.
pub(crate) fn release(store: &Store, identity: &str, recipient: &str, dead: &BTreeSet<String>) -> Result<Vec<String>> {
    let mut delete = Vec::new();
    let all = packs(&store.backend)?;
    let mut indexes = BTreeMap::new();
    for pack in &all {
        indexes.insert(pack.clone(), index(&store.backend, Some(&store.dir), identity, pack)?);
    }
    // A chunk that another pack also holds (an earlier rewrite that crashed before its deletes)
    // need not be copied again.
    let mut holders: BTreeMap<&str, usize> = BTreeMap::new();
    for index in indexes.values() {
        for (id, _, _) in &index.chunks {
            *holders.entry(id).or_default() += 1;
        }
    }
    for (pack, index) in &indexes {
        if !index.chunks.iter().any(|(id, _, _)| dead.contains(id)) {
            continue;
        }
        let live: Vec<_> =
            index.chunks.iter().filter(|(id, _, _)| !dead.contains(id) && holders[id.as_str()] == 1).cloned().collect();
        if !live.is_empty() {
            let Some(body) = store.backend.get(&chunks::pack_name(pack))? else {
                bail!("chunk pack {pack} has an index but no data");
            };
            let data = chunks::open_pack(&keys::decrypt(identity, &body)?, index)?;
            let mut builder = Builder::default();
            for (id, offset, length) in &live {
                builder.add(id.clone(), &data[*offset as usize..(*offset + *length) as usize]);
            }
            let (frame, new_index) = builder.finish()?;
            let new = crate::ids::new_id();
            store.backend.put_new(&chunks::pack_name(&new), keys::encrypt(recipient, &frame)?)?;
            store.backend.put_new(&chunks::index_name(&new), keys::encrypt(recipient, &new_index.encode())?)?;
            for (id, _, _) in &live {
                *holders.get_mut(id.as_str()).unwrap() += 1;
            }
        }
        for (id, _, _) in &index.chunks {
            *holders.get_mut(id.as_str()).unwrap() -= 1;
        }
        delete.push(chunks::index_name(pack));
        delete.push(chunks::pack_name(pack));
    }
    Ok(delete)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut writer = Writer::open(store, recipient, identity, key, BTreeMap::new()).unwrap();
        let a = writer.file(&sha(&one), &one).unwrap();
        writer.flush().unwrap();
        let b = writer.file(&sha(&two), &two).unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.packs, 2);
        (a, b)
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
        let delete = release(&store, &identity, &recipient, &dead).unwrap();
        assert_eq!(delete.len(), 4, "both packs and both indexes go");
        // A crash before the deletes: a rerun copies nothing again and names the same objects.
        let again = release(&store, &identity, &recipient, &dead).unwrap();
        assert_eq!(again, delete);
        assert_eq!(packs(&store.backend).unwrap().len(), before.len() + 1, "one new pack for the live chunk");
        for key in &delete {
            store.backend.delete(key).unwrap();
        }
        let now = places(&store.backend, None, &identity).unwrap();
        assert_eq!(now.keys().cloned().collect::<BTreeSet<_>>(), BTreeSet::from([a[0].clone()]));
        let mut reader = Reader::new(&store.backend, &identity);
        assert_eq!(reader.file(&a[..1], 1 << 20).unwrap(), split_owned(&noise(300_000, 1))[0]);
        assert!(reader.file(&b, 1 << 20).is_err(), "a released chunk is gone");
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
        let mut first = Writer::open(&store, &recipient, &identity, key, BTreeMap::new()).unwrap();
        let ids = first.file(&sha(&data), &data).unwrap();
        first.flush().unwrap();
        // A second push knows the file from the first push's tree and reuses every chunk, then a
        // deletion releases them before it publishes.
        let mut second = Writer::open(&store, &recipient, &identity, key, first.into_hashes()).unwrap();
        assert_eq!(second.file(&sha(&data), &data).unwrap(), ids);
        assert_eq!(second.packs, 0, "nothing new to write");
        let dead: BTreeSet<String> = ids.iter().cloned().collect();
        for key in release(&store, &identity, &recipient, &dead).unwrap() {
            store.backend.delete(&key).unwrap();
        }
        assert!(places(&store.backend, None, &identity).unwrap().is_empty());
        let chunked = BTreeMap::from([("a".to_string(), ids.clone())]);
        second.finish(&files, &chunked, &|root: &Path, path: &str| Ok(std::fs::read(root.join(path))?)).unwrap();
        let now = places(&store.backend, None, &identity).unwrap();
        assert!(ids.iter().all(|id| now.contains_key(id)), "the push uploaded the released chunks again");
        assert_eq!(Reader::new(&store.backend, &identity).file(&ids, data.len() as u64).unwrap(), data);
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
