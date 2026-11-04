//! Content-defined chunks of cache layers in pack objects (`layr_store::chunks`).
//!
//! A cache state loses its base at the next save (`history=last`), so a push cannot make a delta
//! against the old content. Chunks need only the ids that the store has: a changed build output
//! file shares most of its chunks with files that the store already holds, under any path.
//!
//! The store keeps no mutable chunk map. The pack indexes under `chunks/` are the truth: a writer
//! lists them to dedup, a reader lists them to find a chunk, and `release` rewrites a pack whose
//! chunks are partly dead into a new pack before the old one goes.

use super::{backend::Backend, format::Tree, keys, Store};
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
    if let (Some(dir), Some(cached)) = (dir, cached) {
        std::fs::create_dir_all(cache_dir(dir))?;
        crate::fsutil::atomic_write(&cached, &bytes, 0o600)?;
    }
    Ok(index)
}

/// Every published chunk of the layer.
pub(crate) fn places(backend: &Backend, dir: Option<&Path>, identity: &str) -> Result<Places> {
    let packs = packs(backend)?;
    // Forget cached indexes of packs that are gone, so the cache does not grow.
    if let Some(Ok(entries)) = dir.map(|dir| std::fs::read_dir(cache_dir(dir))) {
        for entry in entries.flatten() {
            if !packs.contains(entry.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let mut out = Places::new();
    for pack in packs {
        for (id, offset, length) in index(backend, dir, identity, &pack)?.chunks {
            out.entry(id).or_insert(Place { pack: pack.clone(), offset, length });
        }
    }
    Ok(out)
}

/// Uploads the chunks of the files of one state.
pub(crate) struct Writer<'a> {
    store: &'a Store,
    recipient: String,
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
    /// Compressed bytes written.
    pub bytes: u64,
    pub packs: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn open(store: &'a Store, recipient: &str, identity: &str, key: [u8; 32], base: Option<&Tree>) -> Result<Self> {
        let remote = places(&store.backend, Some(&store.dir), identity)?;
        let mut by_sha = BTreeMap::new();
        if let Some(base) = base {
            for (path, ids) in &base.chunks {
                if let Some(entry) = base.files.get(path) {
                    by_sha.insert(entry.2.clone(), ids.clone());
                }
            }
        }
        Ok(Self {
            store,
            recipient: recipient.to_string(),
            identity: identity.to_string(),
            key,
            remote,
            written: BTreeSet::new(),
            reused: BTreeSet::new(),
            by_sha,
            builder: Builder::default(),
            bytes: 0,
            packs: 0,
        })
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
        for chunk in chunks::split(data) {
            let id = chunks::id(&self.key, chunk);
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
            self.flush()?;
        }
        Ok(())
    }

    /// Publishes the open pack: the pack first, then its index, which makes it visible.
    pub(crate) fn flush(&mut self) -> Result<()> {
        if self.builder.is_empty() {
            return Ok(());
        }
        let (frame, index) = std::mem::take(&mut self.builder).finish()?;
        let pack = crate::ids::new_id();
        let body = keys::encrypt(&self.recipient, &frame)?;
        let index_body = keys::encrypt(&self.recipient, &index.encode())?;
        self.bytes += (body.len() + index_body.len()) as u64;
        if !self.store.backend.put_new(&chunks::pack_name(&pack), body)? {
            bail!("chunk pack {pack} already exists");
        }
        if !self.store.backend.put_new(&chunks::index_name(&pack), index_body)? {
            bail!("chunk index {pack} already exists");
        }
        self.packs += 1;
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

/// Reads chunked files of a layer. Packs are cached in memory, up to a bound.
pub(crate) struct Reader<'a> {
    backend: &'a Backend,
    identity: String,
    places: Option<Places>,
    packs: BTreeMap<String, std::rc::Rc<Vec<u8>>>,
    pack_bytes: usize,
}

const READ_CACHE: usize = 256 << 20;

impl<'a> Reader<'a> {
    pub(crate) fn new(backend: &'a Backend, identity: &str) -> Self {
        Self { backend, identity: identity.to_string(), places: None, packs: BTreeMap::new(), pack_bytes: 0 }
    }

    fn pack(&mut self, pack: &str) -> Result<Option<std::rc::Rc<Vec<u8>>>> {
        if let Some(data) = self.packs.get(pack) {
            return Ok(Some(data.clone()));
        }
        let index = index(self.backend, None, &self.identity, pack)?;
        let Some(body) = self.backend.get(&chunks::pack_name(pack))? else {
            return Ok(None);
        };
        let frame = keys::decrypt(&self.identity, &body)?;
        let data = std::rc::Rc::new(chunks::open_pack(&frame, &index)?);
        if self.pack_bytes + data.len() > READ_CACHE {
            self.packs.clear();
            self.pack_bytes = 0;
        }
        self.pack_bytes += data.len();
        self.packs.insert(pack.to_string(), data.clone());
        Ok(Some(data))
    }

    fn chunk(&mut self, id: &str) -> Result<Vec<u8>> {
        // A release may move a chunk after this reader listed the packs: list once more.
        for attempt in 0..2 {
            if self.places.is_none() || attempt == 1 {
                self.places = Some(places(self.backend, None, &self.identity)?);
            }
            let Some(place) = self.places.as_ref().unwrap().get(id).cloned() else {
                continue;
            };
            let Some(data) = self.pack(&place.pack)? else {
                continue;
            };
            let start = usize::try_from(place.offset)?;
            let end = usize::try_from(place.offset + place.length)?;
            return Ok(data.get(start..end).context("chunk is outside its pack")?.to_vec());
        }
        bail!("chunk {id} is not in the store")
    }

    /// The content of a chunked file. The caller checks its size and SHA-256 against the tree.
    pub(crate) fn file(&mut self, ids: &[String], size: u64) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        for id in ids {
            let chunk = self.chunk(id)?;
            if data.len() as u64 + chunk.len() as u64 > size {
                bail!("chunks exceed their file size");
            }
            data.extend(chunk);
        }
        Ok(data)
    }
}

/// The deletion hook (docs/cache-overlays.md, deletion of cache states): after `dead` chunk ids
/// lost their last live tree, rewrite each pack that also holds live chunks into a new pack with
/// only those, and return the objects to delete: packs and indexes whose chunks are all dead or
/// copied. The new pack is published before the caller deletes anything, so a crash leaves at
/// worst a duplicate, and a rerun gives the same objects to delete.
// The remote deletion runner (sweep, docs/cache-overlays.md) calls it; until then only tests do.
#[allow(dead_code)]
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
        let mut writer = Writer::open(store, recipient, identity, key, None).unwrap();
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
        let mut first = Writer::open(&store, &recipient, &identity, key, None).unwrap();
        let ids = first.file(&sha(&data), &data).unwrap();
        first.flush().unwrap();
        // A second push reuses every chunk, then a deletion releases them before it publishes.
        let mut second = Writer::open(&store, &recipient, &identity, key, None).unwrap();
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
