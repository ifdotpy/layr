//! Publish immutable records, packed file frames and signed state trees.

use super::{
    canonical::canonical,
    format::{self, Entry, Header, Link, Tree},
    keys, reader, zst, Descriptor, Options, Report, Store,
};
use crate::{ctx::Ctx, fsutil, oplog, repo::Repo};
use anyhow::{bail, Context, Result};
use layr_hashers::{ContentAddress, Digest};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
struct Pushed {
    attempt: String,
    tree: Tree,
    depth: usize,
    #[serde(skip)]
    canonical: Vec<u8>,
}

#[derive(Default)]
struct ContentHashes {
    entries: BTreeMap<String, Vec<Link>>,
    dirty: BTreeMap<String, Vec<Link>>,
    tentative: BTreeMap<String, (Vec<Link>, i64)>,
    pending: BTreeMap<String, Vec<Entry>>,
    /// The data parts that the trees of this push link: each tree published after its parts went up and, in a layer,
    /// after the check that the remote lists the parts that it reused. Cleared when a part or base went meanwhile.
    published: BTreeSet<String>,
}
impl ContentHashes {
    fn get(&self, sha: &str) -> Option<&Vec<Link>> {
        self.entries.get(sha).or_else(|| self.tentative.get(sha).map(|value| &value.0))
    }
    fn insert(&mut self, sha: String, links: Vec<Link>) {
        self.dirty.insert(sha.clone(), links.clone());
        self.entries.insert(sha, links);
    }
    fn seed(&mut self, sha: String, links: Vec<Link>) {
        // A chunked file has no frames to reuse (`Tree::chunks`).
        if links.is_empty() {
            return;
        }
        if !self.entries.contains_key(&sha) {
            self.insert(sha, links);
        }
    }
    fn pending_part(&mut self, object: String, modified: i64, entries: Vec<Entry>) {
        for entry in &entries {
            if !self.entries.contains_key(&entry.2) {
                self.tentative.insert(entry.2.clone(), (entry.3.clone(), modified));
            }
        }
        self.pending.insert(object, entries);
    }
    fn protect(&mut self, tree: &Tree) -> Vec<String> {
        let objects: BTreeSet<_> = tree.files.values().flat_map(|entry| entry.3.iter().map(Link::object)).collect();
        let protected: Vec<_> = self.pending.keys().filter(|object| objects.contains(*object)).cloned().collect();
        for object in &protected {
            for entry in self.pending.remove(object).unwrap() {
                self.tentative.remove(&entry.2);
                self.seed(entry.2, entry.3);
            }
        }
        protected
    }
    /// Drops every chain that `stale` names: its frames are gone from the remote or never publish. Returns the
    /// content hashes that lost their chain, for the journal (`Journal::forget`).
    fn forget(&mut self, stale: impl Fn(&str, &[Link]) -> bool) -> Vec<String> {
        let mut forgotten = Vec::new();
        self.entries.retain(|sha, chain| stale(sha, chain).then(|| forgotten.push(sha.clone())).is_none());
        self.dirty.retain(|sha, chain| !stale(sha, chain));
        self.tentative.retain(|sha, (chain, _)| !stale(sha, chain));
        forgotten
    }
    fn deadline(&self, tree: &Tree, started: i64) -> i64 {
        tree.files.values().filter_map(|entry| self.tentative.get(&entry.2).map(|value| value.1)).fold(started, i64::min)
            + 23 * 3600
    }
}

/// A listing page of the remotes (gateway and S3 return at most 1,000 objects per page).
const LISTING_PAGE: usize = 1000;
/// Above this many estimated objects a listing of `states/` can exceed the gateway's 100,000 objects.
const LISTING_OBJECTS: usize = 50_000;

/// The objects under `states/<id>/` that this push discovered. One listing of `states/` replaces many exact prefix
/// probes when its pages cost less; a tiny push against a long history probes only the prefixes it needs. A base
/// decision never uses this memory: it probes again (`fresh`).
struct Attempts<'a> {
    backend: &'a super::backend::Backend,
    /// Objects by state, from an exact probe or the bulk listing.
    seen: BTreeMap<String, Vec<super::backend::Object>>,
    listed: bool,
    /// States that the local index knows: the estimate of the listing's size.
    known: usize,
    /// The objects of the last bulk listing: a listing again costs at least their pages.
    listed_objects: usize,
    /// Each tree asks for the parts that it reuses before it publishes: in a layer, whose sweep deletes parts, and on
    /// a remote that lost a completed tip (a partial replica can lack any part of the journal's chains).
    check_parts: bool,
    /// The remote lost a completed tip (a partial replica): a base tree that it still has can be a delta on a tree that
    /// it lost, so each tree goes up complete.
    complete_trees: bool,
}

impl<'a> Attempts<'a> {
    fn new(backend: &'a super::backend::Backend, known: usize) -> Self {
        Self {
            backend,
            seen: BTreeMap::new(),
            listed: false,
            known,
            listed_objects: 0,
            check_parts: false,
            complete_trees: false,
        }
    }

    /// Learns the prefixes of `states` not seen yet (with `fresh`, all of them again): from one listing of `states/`
    /// when its estimated pages (a tree and about one data part per state, or the size of the last listing) are fewer
    /// than the probes, or from parallel exact probes.
    fn discover(&mut self, states: impl IntoIterator<Item = String>, fresh: bool) -> Result<()> {
        let unseen: Vec<String> =
            states.into_iter().filter(|id| fresh || (!self.listed && !self.seen.contains_key(id))).collect();
        let objects = (2 * (self.known + unseen.len())).max(self.listed_objects);
        if unseen.len() > objects.div_ceil(LISTING_PAGE) && objects <= LISTING_OBJECTS {
            // A remote that refuses or cannot list that much falls back to the exact probes.
            if let Ok(listing) = self.backend.list("states/") {
                // The listing is newer than every earlier probe: it replaces them.
                self.listed = true;
                self.listed_objects = listing.len();
                self.seen.clear();
                for object in listing {
                    if let Some((state, _)) = object.key.strip_prefix("states/").and_then(|key| key.split_once('/')) {
                        self.seen.entry(state.to_string()).or_default().push(object);
                    }
                }
                return Ok(());
            }
        }
        // One list per state, many at a time: a project with hundreds of lines and refs pays the latency of
        // the remote once per batch of workers, not once per state.
        let backend = self.backend;
        let seen = &mut self.seen;
        super::download::run(
            unseen.len(),
            super::download::JOBS,
            super::download::JOBS as u64,
            |_| 1,
            |i| backend.list(&format!("states/{}/", unseen[i])),
            |i, objects| {
                seen.insert(unseen[i].clone(), objects);
                Ok(())
            },
        )
    }

    /// The objects of `states/<state>/`. `fresh` sends a new exact probe now: a delta goes up on a base only
    /// after this request saw its tree, since another machine's sweep can delete it at any time.
    fn objects(&mut self, state: &str, fresh: bool) -> Result<&[super::backend::Object]> {
        if fresh || !(self.listed || self.seen.contains_key(state)) {
            let objects = self.backend.list(&format!("states/{state}/"))?;
            self.seen.insert(state.to_string(), objects);
        }
        Ok(self.seen.get(state).map_or(&[], Vec::as_slice))
    }

    /// The data parts of `objects` that the remote does not hold now: their state prefixes are discovered again (one
    /// listing or exact probes, `discover`). What discovery saw of other states stays.
    fn missing(&mut self, objects: BTreeSet<String>) -> Result<BTreeSet<String>> {
        let state_of = |key: &str| format::parse_data_name(key).map(|(state, _, _)| state);
        let states: BTreeSet<String> = objects.iter().filter_map(|key| state_of(key)).collect();
        self.discover(states, true)?;
        let held = |key: &String| {
            state_of(key).and_then(|state| self.seen.get(&state)).is_some_and(|seen| seen.iter().any(|o| &o.key == key))
        };
        Ok(objects.iter().filter(|key| !held(key)).cloned().collect())
    }

    fn has_tree(&mut self, state: &str, attempt: &str, fresh: bool) -> Result<bool> {
        let name = format::tree_name(state, attempt);
        Ok(self.objects(state, fresh)?.iter().any(|object| object.key == name))
    }

    /// The first complete attempt of `state` on the remote.
    fn complete(&mut self, state: &str) -> Result<Option<String>> {
        let mut first = None;
        for object in self.objects(state, false)? {
            if let Some((remote, attempt)) = format::parse_tree_name(&object.key) {
                if remote != state {
                    bail!("remote tree is outside its state prefix");
                }
                first = first.or(Some(attempt));
            }
        }
        Ok(first)
    }
}

/// Authenticated reads of remote trees for one push. One identity, one record check and one reader with its tree
/// cache serve every recovered state and every base that the local tree cache lacks: a shared parent chain is read
/// once, not once per base.
struct Hydration<'a> {
    store: &'a Store,
    descriptor: &'a Descriptor,
    paper: Option<&'a str>,
    record_lines: &'a [String],
    /// The folder of the reader's operation cache (`Reader::cache_in`).
    cache: Option<std::path::PathBuf>,
    identity: std::cell::OnceCell<Option<String>>,
    records: std::cell::OnceCell<reader::Records>,
}

impl<'a> Hydration<'a> {
    /// The reader of this push in `slot`, made on first use. None without a reading identity; `required` returns
    /// the identity's error instead.
    fn reader<'s, 'r>(
        &'s self,
        slot: &'r mut Option<reader::Reader<'s>>,
        required: bool,
    ) -> Result<Option<&'r mut reader::Reader<'s>>> {
        if slot.is_none() {
            let identity = match self.identity.get() {
                Some(identity) => identity,
                None => {
                    let identity = match self.store.identity(self.descriptor, self.paper) {
                        Ok(identity) => Some(identity),
                        Err(error) if required => return Err(error),
                        Err(_) => None,
                    };
                    self.identity.get_or_init(|| identity)
                }
            };
            let Some(identity) = identity.as_deref() else { return Ok(None) };
            let records = match self.records.get() {
                Some(records) => records,
                None => {
                    let records = reader::records_with_local(
                        &self.store.backend,
                        identity,
                        &self.descriptor.project,
                        self.record_lines,
                        Some(&reader::seen_file(&self.store.dir)),
                    )?;
                    self.records.get_or_init(|| records)
                }
            };
            let mut reader = reader::Reader::new(&self.store.backend, identity, &records.keys);
            reader.cache_in(self.cache.clone());
            *slot = Some(reader);
        }
        Ok(slot.as_mut())
    }
}

/// One data part going up: its number, its frames and their entries.
type PartJob = (u32, Vec<u8>, Vec<Entry>);
type PartPool<'scope, 'env> = super::download::Pool<'scope, 'env, PartJob, ()>;

/// The most bytes that one part holds while it uploads: its plaintext buffer until `put_part` encrypted it, and the
/// ciphertext, which the request body and every try share without a copy.
fn part_charge(capacity: usize, len: usize) -> u64 {
    (capacity + keys::ciphertext_len(len)) as u64
}

/// The part that the walk builds: at most twice `PART_LIMIT` of capacity.
fn building_charge() -> u64 {
    part_charge(2 * format::PART_LIMIT, format::PART_LIMIT)
}

/// What the walk holds for one file of `size` bytes beside the part it builds: the content and its frame, and for a
/// delta the base content (at most `DELTA_LIMIT`) and a second frame.
fn walk_charge(size: u64, delta: bool) -> u64 {
    let frame = zst::compress_bound(usize::try_from(size).unwrap_or(usize::MAX)) as u64;
    size.saturating_add(frame).saturating_add(if delta { format::DELTA_LIMIT as u64 + frame } else { 0 })
}

/// Queues one part on the transfer pool. Then, before the walk builds the next part, waits until a full part fits the
/// upload budget too (`building_charge`). A larger part (one file whose frame exceeds `PART_LIMIT`) travels alone: the
/// walk drained the pool before it took the frame, and it waits here until the part is acknowledged, whatever its
/// charge.
fn send_part(pool: &mut PartPool<'_, '_>, part: u32, data: Vec<u8>, entries: Vec<Entry>) -> Result<()> {
    let charge = part_charge(data.capacity(), data.len());
    let next = if data.len() > format::PART_LIMIT { u64::MAX } else { building_charge() };
    pool.submit((part, data, entries), charge, &mut |_, ()| Ok(()))?;
    pool.wait_for(next, &mut |_, ()| Ok(()))
}

/// Sends the part being built, if any, after its journal row.
fn flush_part(
    pool: &mut PartPool<'_, '_>,
    journal: Option<&mut super::push_journal::Journal>,
    (state, attempt): (&str, &str),
    part: &mut u32,
    current: &mut Vec<u8>,
    entries: &mut Vec<Entry>,
) -> Result<()> {
    if current.is_empty() {
        return Ok(());
    }
    if let Some(journal) = journal {
        journal.prepare_part(state, attempt, *part, entries)?;
    }
    send_part(pool, *part, std::mem::take(current), std::mem::take(entries))?;
    *part += 1;
    Ok(())
}

fn push_now() -> i64 {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("LAYR_TEST_PUSH_CLOCK") {
        return std::fs::read_to_string(path).unwrap().trim().parse().unwrap();
    }
    chrono::Utc::now().timestamp()
}

// Completed uploads are durable remotely; both local caches can be discarded.
const CHECKPOINT_STATES: usize = 128;
const RECORD_SEGMENT_BYTES: usize = 4 << 20;
const INDEX_STATES: usize = 16_384;
#[cfg(test)]
type BaseHook = Box<dyn FnMut(&str, &str)>;
#[cfg(test)]
thread_local! {
    static PUSH_STOP_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static PUSH_DETERMINISTIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PUSH_CACHE_DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Runs with `(state, base)` before the probe of a selected base: tests delete trees there.
    static PUSH_BEFORE_BASE: std::cell::RefCell<Option<BaseHook>> = const { std::cell::RefCell::new(None) };
    /// Runs with `(state, base)` after the parts of a state on a probed base are up, before the last probe of the base.
    static PUSH_BEFORE_TREE: std::cell::RefCell<Option<BaseHook>> = const { std::cell::RefCell::new(None) };
}
#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    attempt: String,
    depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct PushIndex {
    format: u32,
    states: BTreeMap<String, Cached>,
}
fn valid_cached(state: &str, entry: &Cached) -> bool {
    crate::model::is_uuid(state)
        && format::valid_attempt(&entry.attempt)
        && entry.depth < format::FULL_TREE_EVERY
        && entry.root.as_ref().is_none_or(|root| root.len() == 64 && hex::decode(root).is_ok())
}
fn cache_tree(cache: &mut Option<super::push_cache::TreeCache>, pushed: &Pushed, uploaded_bytes: u64) -> Cached {
    Cached {
        attempt: pushed.attempt.clone(),
        depth: pushed.depth,
        root: cache.as_mut().and_then(|cache| {
            cache.record_upload(uploaded_bytes).ok()?;
            cache.put(&pushed.tree)
        }),
    }
}
fn cached_tree(cache: &Option<super::push_cache::TreeCache>, entry: &Cached) -> Option<Pushed> {
    let tree = cache.as_ref()?.get(entry.root.as_deref()?).ok()?;
    let canonical = tree.canonical().ok()?;
    Some(Pushed { attempt: entry.attempt.clone(), tree, depth: entry.depth, canonical })
}
fn load_index(dir: &Path) -> Result<BTreeMap<String, Cached>> {
    let path = dir.join("pushed-v2.json.zst");
    // The index is a cache: a missing or unreadable one is empty, and pushes find the states again.
    let Some(bytes) = std::fs::read(&path).ok().and_then(|bytes| zst::decompress_with_limit(&bytes, None, 8 << 20).ok()) else {
        return Ok(BTreeMap::new());
    };
    let Ok(index) = serde_json::from_slice::<PushIndex>(&bytes) else {
        return Ok(BTreeMap::new());
    };
    if index.format != 2 {
        return Ok(BTreeMap::new());
    }
    let mut states = index.states;
    states.retain(|id, entry| valid_cached(id, entry));
    Ok(states)
}
fn checkpoint(dir: &Path, index: &BTreeMap<String, Cached>) -> Result<()> {
    // UUIDv7 sorting keeps recent completions. Evicted metadata is rediscovered by state prefix.
    let states = index.iter().rev().take(INDEX_STATES).map(|(id, entry)| (id.clone(), entry.clone())).collect();
    let bytes = serde_json::to_vec(&PushIndex { format: 2, states })?;
    fsutil::atomic_write(&dir.join("pushed-v2.json.zst"), &zst::compress(&bytes, None)?, 0o600)?;
    // Only retire old formats after the replacement metadata is safely published.
    if dir.join("pushed.json").exists() {
        std::fs::remove_file(dir.join("pushed.json"))?;
    }
    if dir.join("pushed").exists() {
        std::fs::remove_dir_all(dir.join("pushed"))?;
    }
    Ok(())
}
pub(super) fn parents_first(states: Vec<crate::model::StateRec>) -> Result<Vec<crate::model::StateRec>> {
    let mut pending: BTreeMap<_, _> = states.into_iter().map(|s| (s.id.clone(), s)).collect();
    let mut counts = BTreeMap::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ready = BTreeSet::new();
    for (id, state) in &pending {
        let mut count = 0;
        for parent in state.parents.iter().chain(state.derived_from.iter()) {
            if pending.contains_key(parent) {
                count += 1;
                children.entry(parent.clone()).or_default().push(id.clone());
            }
        }
        counts.insert(id.clone(), count);
        if count == 0 {
            ready.insert((state.time, id.clone()));
        }
    }
    let mut ordered = Vec::new();
    while let Some((_, id)) = ready.pop_first() {
        ordered.push(pending.remove(&id).unwrap());
        for child in children.remove(&id).unwrap_or_default() {
            let count = counts.get_mut(&child).unwrap();
            *count -= 1;
            if *count == 0 {
                ready.insert((pending[&child].time, child));
            }
        }
    }
    if !pending.is_empty() {
        bail!("local state ancestry has a cycle");
    }
    Ok(ordered)
}

pub(super) fn initialize(store: &Store, ctx: &crate::ctx::Ctx, output: &Path) -> Result<(String, String)> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = std::path::absolute(if output.is_absolute() { output.to_path_buf() } else { ctx.cwd.join(output) })?;
    let parent = path.parent().context("paper key file has no parent")?;
    let parent = crate::privs::as_caller_fs(ctx, || Ok(std::fs::canonicalize(parent)?))?;
    let path = parent.join(path.file_name().context("paper key file has no name")?);
    let local_root = std::fs::canonicalize(&ctx.store.root)?;
    if path.starts_with(&local_root) {
        bail!("the paper key file must be outside the local store and line folders");
    }
    if let super::backend::Backend::File(root, _) = &store.backend {
        let remote_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        if path.starts_with(remote_root) {
            bail!("the paper key file must be outside the remote store");
        }
    }
    let existing_slot = store.backend.get("slots/paper.age")?;
    let existing_file = crate::privs::as_caller_fs(ctx, || {
        match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
            Ok(file) => {
                let meta = file.metadata()?;
                if !meta.is_file() || meta.uid() != ctx.caller.uid || meta.mode() & 0o777 != 0o600 {
                    bail!("paper key file must be owned by the caller with mode 0600");
                }
                let mut text = String::new();
                file.take(4097).read_to_string(&mut text)?;
                if text.len() > 4096 {
                    bail!("invalid paper key file");
                }
                Ok(Some(text))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    })?;
    let (paper, vault) = if let Some(text) = existing_file {
        let slot = existing_slot.as_ref().context("paper key file already exists; use a new --paper-key-out path")?;
        let paper = keys::identity_from_paper(&text).map_err(|_| anyhow::anyhow!("invalid paper key file"))?;
        let vault = keys::open_slot(slot, &paper).map_err(|_| anyhow::anyhow!("store is being created by another writer"))?;
        (paper, vault)
    } else {
        let paper = keys::generate_identity();
        let words = keys::words_of_identity(&paper)?;
        crate::privs::as_caller_fs(ctx, || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            file.write_all(format!("{words}\n").as_bytes())?;
            file.sync_all()?;
            std::fs::File::open(&parent)?.sync_all()?;
            Ok(())
        })?;
        (paper, keys::generate_identity())
    };
    let recipient = publish_initial_slots(store, &paper, vault)?;
    Ok((path.to_string_lossy().into_owned(), recipient))
}

pub(super) fn initialize_tty(store: &Store, ctx: &Ctx) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("--paper-key-tty requires terminal stdin and stdout; run it in your private terminal without redirection");
    }
    let mut terminal = crate::privs::as_caller_fs(ctx, || {
        Ok(std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY).open("/dev/tty")?)
    })
    .context("--paper-key-tty requires a controlling terminal; run it in your private terminal")?;
    if !terminal.is_terminal() {
        bail!("--paper-key-tty requires a controlling terminal");
    }
    // A service worker can inherit another session's controlling terminal.
    // Never display a caller's key on that terminal.
    let foreground = rustix::termios::tcgetpgrp(&terminal).context("cannot check the controlling terminal")?;
    if rustix::termios::tcgetpgrp(std::io::stdin()).ok() != Some(foreground) {
        bail!("--paper-key-tty requires stdin on your controlling terminal");
    }
    if store.backend.get("slots/paper.age")?.is_some() {
        bail!("store creation already started; recover with the original paper key");
    }
    let paper = keys::generate_identity();
    let words = keys::words_of_identity(&paper)?;
    writeln!(terminal, "Paper key (24 words):\n{words}\n\nSave these words privately. They will not be shown again.")?;
    write!(terminal, "Type the final word of the paper key to confirm: ")?;
    terminal.flush()?;
    let mut confirmation = String::new();
    std::io::BufReader::new((&terminal).take(128)).read_line(&mut confirmation)?;
    if confirmation.trim() != words.split_whitespace().last().expect("paper key has words") {
        bail!("paper key confirmation did not match; no store objects were published");
    }
    publish_initial_slots(store, &paper, keys::generate_identity())
}

fn publish_initial_slots(store: &Store, paper: &str, mut vault: String) -> Result<String> {
    let paper_slot = keys::seal_slot(&vault, &keys::recipient_of(paper)?)?;
    if !store.backend.put_new("slots/paper.age", paper_slot)? {
        let slot = store.backend.get("slots/paper.age")?.context("paper slot disappeared")?;
        vault = keys::open_slot(&slot, paper).map_err(|_| anyhow::anyhow!("store is being created by another writer"))?;
    }
    initialization_failpoint("paper")?;
    let machine_slot = format!("slots/machine-{}.age", store.machine);
    if !store.backend.put_new(&machine_slot, keys::machine_slot(&vault, &store.key.verifying_key())?)? {
        let slot = store.backend.get(&machine_slot)?.context("machine slot disappeared")?;
        if keys::open_machine_slot(&slot, &store.key)? != vault {
            bail!("existing machine slot belongs to another store");
        }
    }
    initialization_failpoint("machine")?;
    let descriptor = Descriptor {
        format: 2,
        project: store.project.clone(),
        recipient: keys::recipient_of(&vault)?,
        mode: Default::default(),
    };
    if !store.backend.put_new("store.json", canonical(&serde_json::to_value(&descriptor)?)?)? {
        if store.descriptor()?.recipient != descriptor.recipient {
            bail!("store is being created by another writer");
        }
    }
    initialization_failpoint("store")?;
    keys::recipient_of(paper)
}

/// A new Private or Public store: only store.json. It has no slots and no paper key; the server's access list
/// decides who reads and writes.
pub(super) fn initialize_plain(store: &Store, mode: format::Mode) -> Result<()> {
    let descriptor = Descriptor { format: 2, project: store.project.clone(), recipient: keys::PLAIN.into(), mode };
    if !store.backend.put_new("store.json", canonical(&serde_json::to_value(&descriptor)?)?)? {
        let existing = store.root_descriptor()?;
        if existing.mode != mode || existing.project != descriptor.project {
            bail!("store is being created by another writer");
        }
    }
    Ok(())
}

// Unit tests inject interruption after each immutable publication.
#[cfg(test)]
thread_local! { static INIT_STOP: std::cell::RefCell<Option<&'static str>> = const {std::cell::RefCell::new(None)}; }
fn initialization_failpoint(point: &str) -> Result<()> {
    #[cfg(test)]
    if INIT_STOP.with(|value| *value.borrow() == Some(point)) {
        bail!("injected initialization interruption");
    }
    let _ = point;
    Ok(())
}

/// A push runs with no project lock: the uploads are the long part. The states it reads are pinned
/// (`push_locked`), so gc keeps them. A lock file in the push folder lets one push of this remote run
/// at a time: its journal and index live there.
pub(super) fn push(store: &Store, ctx: &Ctx, repo: &Repo, opts: &Options) -> Result<Report> {
    let _push = push_folder_lock(store)?;
    push_locked(store, ctx, repo, opts)
}

/// The lock of one remote folder: one push or sweep of that remote at a time (a sweep must not delete a tree that
/// a push uses as its base), whichever URL names it (`Store::lock_dir`).
pub(super) fn push_folder_lock(store: &Store) -> Result<std::fs::File> {
    let dir = store.lock_dir();
    std::fs::create_dir_all(&dir)?;
    let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(dir.join("push.lock"))?;
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(f)
}

pub(super) fn push_locked(store: &Store, ctx: &Ctx, repo: &Repo, opts: &Options) -> Result<Report> {
    let mut local = crate::sync_local::Local::read(ctx, repo);
    if store.is_api() {
        let _push = crate::trace::span("store push");
        // Compatibility path: the separate API object codec is migrated after the v2 fast path.
        let (_, _, mut report) = prepare_push(store, &local, opts)?;
        super::cloud_objects::push_states(store, ctx, repo, opts, &mut report)?;
        return Ok(report);
    }
    push_with_local(store, &mut local, opts)
}

pub(crate) fn push_with_local(store: &Store, local: &mut super::Replica<'_>, opts: &Options) -> Result<Report> {
    let _push = crate::trace::span("store push");
    let (descriptor, blind_layer, mut report) = prepare_push(store, local, opts)?;
    let stage = crate::trace::span("store load index");
    let mut index = load_index(&store.dir)?;
    let mut journal = super::push_journal::Journal::open(&store.dir)?;
    for done in journal.completed()? {
        let root = index.get(&done.state).filter(|entry| entry.attempt == done.attempt).and_then(|entry| entry.root.clone());
        index.insert(done.state, Cached { attempt: done.attempt, depth: done.depth, root });
    }
    // The old per-state tree files are unnecessary even before a new checkpoint. Free
    // them first so upgrading a disk-full store does not need extra scratch space.
    if store.dir.join("pushed").exists() {
        std::fs::remove_dir_all(store.dir.join("pushed"))?;
    }
    let mut tree_cache = super::push_cache::TreeCache::open(&store.dir).ok();
    #[cfg(test)]
    if PUSH_CACHE_DISABLED.with(|disabled| disabled.get()) {
        tree_cache = None;
    }
    // Completed immutable trees are never collected. Cache metadata is written only after
    // publication, so a regular push need not enumerate every historical remote object.
    index.retain(|id, entry| valid_cached(id, entry));
    let mut attempts = Attempts::new(&store.backend, index.len());
    // The attempts of completed tips that the remote lacks: no chain of the journal links their parts again.
    let mut lost = BTreeMap::new();
    if !blind_layer {
        // A completed upload can be missing from a partial replica. Recheck advertised tips
        // without scanning historical states, so a later push can complete that replica.
        let tips: Vec<String> = local.tips()?.into_iter().filter(|tip| index.contains_key(tip)).collect();
        attempts.discover(tips.iter().cloned(), false)?;
        for tip in tips {
            let attempt = index[&tip].attempt.clone();
            if !attempts.has_tree(&tip, &attempt, false)? {
                index.remove(&tip);
                lost.insert(tip, attempt);
            }
        }
    }
    attempts.check_parts = store.layer.is_some() || !lost.is_empty();
    attempts.complete_trees = !lost.is_empty();
    let mut present: BTreeMap<String, String> = index.iter().map(|(id, entry)| (id.clone(), entry.attempt.clone())).collect();
    drop(stage);
    let stage = crate::trace::span("store select history");
    let mut record_lines = Vec::new();
    local.visit_records(None, &mut |line| {
        record_lines.push(line.to_owned());
        Ok(())
    })?;
    let mut states = Vec::new();
    let mut recovered = Vec::new();
    let candidates = local.push_candidates(&record_lines, opts, &present.keys().cloned().collect(), &mut report.omitted)?;
    if !blind_layer {
        attempts.discover(candidates.iter().map(|state| state.id.clone()), false)?;
    }
    for state in candidates {
        let id = state.id.clone();
        if !blind_layer {
            if let Some(attempt) = attempts.complete(&id)? {
                // A fresh writer can discover another writer's completed history without
                // having a reader slot. Only an existing transfer journal needs recovery.
                if journal.resuming {
                    recovered.push(id.clone());
                }
                present.insert(id.clone(), attempt.clone());
                // No checkpoint authenticates a local root for this publication.
                // Ignore every remaining local node and hydrate its signed remote tree.
                index.insert(id, Cached { attempt, depth: format::FULL_TREE_EVERY - 1, root: None });
                continue;
            }
        }
        // Pinned before the trees are read: gc keeps the state until this command ends. A state that a free step
        // already queued (no owner keeps it; a deleted line's state that a thin layer still uses as its base) is
        // not sent: nothing on the remote needs it.
        match local.pin_state(&id) {
            Ok(()) => {}
            Err(e) => {
                report.omitted.push(format!("{} {}: {e:#}", id, serde_json::to_value(state.kind)?));
                continue;
            }
        }
        states.push(state);
    }
    states.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
    drop(stage);
    let stage = crate::trace::span("store secret checks");
    // Check changed files against one current secret source index before uploading any state.
    local.check_secrets(&states.iter().map(|state| state.id.clone()).collect::<Vec<_>>())?;
    drop(stage);
    let stage = crate::trace::span("store collect hashes");
    let mut hashes = ContentHashes { entries: journal.hashes()?, ..Default::default() };
    journal.forget(&hashes.forget(|_, chain| chain.iter().any(|link| lost.get(&link.0) == Some(&link.1))))?;
    // File content to chunk ids from pushed trees, like `hashes` for frames: a cache base is gone
    // locally after its next save, but its tree still says which files the store holds.
    let mut chunk_hashes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Authenticate another writer's records only if a missing local parent cache is needed.
    // An ordinary incremental push reuses its already verified local parent directly.
    // Hydration uses the same checked segment cache as fetch: losing a tree cache must not
    // download the entire record history again just to authenticate that tree.
    let hydration = Hydration {
        store,
        descriptor: &descriptor,
        paper: opts.paper_key.as_deref(),
        record_lines: &record_lines,
        cache: local.fetch_temp_dir(),
        identity: Default::default(),
        records: Default::default(),
    };
    let mut remote_trees = None;
    let mut pending = journal.pending()?;
    if blind_layer {
        // A blind writer cannot read a part of an interrupted push to check it, and reuses none: a fresh attempt sends
        // its files again.
        for part in std::mem::take(&mut pending) {
            journal.discard_part(&part.object)?;
        }
    }
    if !pending.is_empty() || !recovered.is_empty() {
        let started = std::time::Instant::now();
        journal.stats.seed_started_ms = crate::ids::now_ms();
        let source = hydration.reader(&mut remote_trees, true)?.context("this writer cannot read the remote trees")?;
        for part in pending {
            journal.stats.seed_pending_parts += 1;
            let modified = match format::parse_data_name(&part.object) {
                Some((state, _, _)) => {
                    journal.stats.seed_metadata_lists += usize::from(!attempts.listed && !attempts.seen.contains_key(&state));
                    attempts.objects(&state, false)?.iter().find(|object| object.key == part.object).map(|object| object.modified)
                }
                None => None,
            };
            let modified = modified.filter(|modified| push_now() - *modified < 12 * 3600);
            if let Some(modified) = modified.filter(|_| part.entries.iter().all(|entry| source.read(entry).is_ok())) {
                journal.validate_part(&part.object, modified)?;
                hashes.pending_part(part.object, modified, part.entries);
            } else {
                journal.discard_part(&part.object)?;
            }
        }
        for id in recovered {
            let attempt = &present[&id];
            let (_, _, tree) = source.tree(&id, attempt)?;
            let depth = source.tree_depth(&id, attempt)?;
            let protected = hashes.protect(&tree);
            for entry in tree.files.values() {
                hashes.seed(entry.2.clone(), entry.3.clone());
            }
            seed_chunks(&mut chunk_hashes, &tree);
            journal.checkpoint(&id, attempt, depth, &hashes.dirty, &protected)?;
            hashes.dirty.clear();
            index.get_mut(&id).unwrap().depth = depth;
            journal.stats.seed_states += 1;
        }
        journal.stats.seed_reader_gets = source.get_count();
        journal.stats.seed_finished_ms = crate::ids::now_ms();
        journal.stats.seed_seconds = started.elapsed().as_secs_f64();
    }
    drop(stage);
    let states = parents_first(states)?;
    if store.backend.can_read() {
        // The bases from earlier pushes without a local tree load together, in batches by delta round, before the
        // first upload. Each one still gets its fresh probe when it is selected below.
        let pushing: BTreeSet<&str> = states.iter().map(|state| state.id.as_str()).collect();
        let mut cold = BTreeSet::new();
        for state in &states {
            // A state that this push sends first is the base then: nothing to load. A walk of the ancestry stops there.
            let Some(id) = select_base(state, &present, &index, &pushing, local)? else { continue };
            let uncached = tree_cache.is_none() || index.get(&id).is_none_or(|entry| entry.root.is_none());
            if let Some(attempt) = present.get(&id).filter(|_| uncached && !pushing.contains(id.as_str())) {
                cold.insert((id.clone(), attempt.clone()));
            }
        }
        if !cold.is_empty() {
            if let Some(source) = hydration.reader(&mut remote_trees, false)? {
                // The sizes that discovery listed reserve the bytes of the reads.
                source.sizes.extend(attempts.seen.values().flatten().filter_map(|o| Some((o.key.clone(), o.size?))));
                // A tree that cannot load here is read, or found unusable, at its own base selection.
                let _ = source.prefetch_trees(cold.into_iter().collect());
            }
        }
    }
    let mut latest: Option<(String, Pushed)> = None;
    for state in states {
        let stage = crate::trace::span("store select parent");
        // Only the state uploaded just before is used without a probe: this push published it.
        let held = latest.as_ref().map(|(last, _)| last.clone());
        // A blind writer's base is only the state uploaded just before: its files go up as deltas on that one.
        let base = match select_base(&state, &present, &index, &BTreeSet::new(), local)?
            .filter(|id| !blind_layer || held.as_ref() == Some(id))
        {
            Some(id) if held.as_ref() == Some(&id) => latest.take(),
            Some(id) => {
                // A tree that the local index names can be gone from the remote: a sweep of another machine (or an
                // earlier one of this machine, before it knew a state built on it) deleted it. A delta needs its base
                // on the remote, so the state then goes up as a complete tree, and the index forgets the base.
                // Each selected base gets a new exact probe; only the state uploaded just before is reused without.
                #[cfg(test)]
                PUSH_BEFORE_BASE.with(|hook| hook.borrow_mut().as_mut().map(|hook| hook(&state.id, &id)));
                let listed = attempts.has_tree(&id, &present[&id], true)?;
                let mut pushed = if listed { index.get(&id).and_then(|entry| cached_tree(&tree_cache, entry)) } else { None };
                if !listed {
                    // An earlier state of this push may have reused the frames of the base: its parts can be gone too.
                    // A sweep ran meanwhile: what the earlier trees of this push linked is asked for again.
                    hashes.published.clear();
                    let attempt = present[&id].clone();
                    journal.forget(&hashes.forget(|_, chain| chain.iter().any(|link| link.0 == id && link.1 == attempt)))?;
                    index.remove(&id);
                    present.remove(&id);
                } else if pushed.is_none() && store.backend.can_read() {
                    if let Some(source) = hydration.reader(&mut remote_trees, false)? {
                        let attempt = present[&id].clone();
                        // A delta tree whose base another machine never finished (or swept) cannot be read: the
                        // state is not a usable base, so this state goes up as a complete tree.
                        let read = source.tree(&id, &attempt).and_then(|(_, canonical, tree)| {
                            Ok(Pushed { depth: source.tree_depth(&id, &attempt)?, attempt: attempt.clone(), tree, canonical })
                        });
                        match read {
                            Ok(restored) => {
                                index.insert(id.clone(), cache_tree(&mut tree_cache, &restored, 0));
                                pushed = Some(restored);
                            }
                            Err(e) => {
                                eprintln!(
                                    "layr: the remote tree of {} cannot be read ({e:#}); a complete tree goes instead",
                                    crate::ids::short(&id)
                                );
                                index.remove(&id);
                                present.remove(&id);
                            }
                        }
                    }
                }
                pushed.map(|pushed| {
                    for entry in pushed.tree.files.values() {
                        hashes.seed(entry.2.clone(), entry.3.clone());
                    }
                    seed_chunks(&mut chunk_hashes, &pushed.tree);
                    (id, pushed)
                })
            }
            None => None,
        };
        drop(stage);
        let stage = crate::trace::span("store upload state");
        let source = local.open_state(&state.id)?;
        let root = source.legacy_root();
        // A base that a history limit deleted here (docs/cache-overlays.md) cannot be compared:
        // the state goes up as a complete tree.
        // Pin first, then look: eviction can take a tree that is not pinned (`cmd::admin::evict_caches`), and a pin
        // after its reservation is refused.
        let base = base.filter(|(id, _)| match local.pin_state(id) {
            Ok(()) => local.has_state(id),
            Err(_) => false,
        });
        let base_source = base.as_ref().map(|(id, _)| local.open_state(id)).transpose()?;
        let mut base = base;
        let (pushed, files, bytes) = loop {
            let probe = base.as_ref().is_some_and(|(id, _)| held.as_ref() != Some(id));
            let uploaded = upload_state_journal(
                store,
                &descriptor,
                &state.id,
                root,
                base.as_ref(),
                &mut hashes,
                &mut chunk_hashes,
                base.as_ref().and(base_source.as_ref()).map(|source| source.legacy_root()),
                btrfs_identity(root, &state.subvol),
                Some(&mut journal),
                Some((&mut attempts, probe)),
            );
            let gone = match uploaded {
                Err(error) if error.is::<Unpublished>() => error.downcast::<Unpublished>().map_err(|e| anyhow::anyhow!(e))?,
                uploaded => break uploaded?,
            };
            // A sweep ran meanwhile: what the earlier trees of this push linked is asked for again.
            hashes.published.clear();
            // As when the first probe misses: without its tree, or with a part that its tree links gone, the base is
            // no base (a delta keeps its entries); the state goes up as a complete tree. No chain that links a part
            // that went, a frame of the attempt that did not publish or, without the base, the base is reused.
            let lost = |base: &Pushed| {
                gone.parts.is_empty()
                    || base.tree.files.values().flat_map(|entry| &entry.3).any(|link| gone.parts.contains(&link.object()))
            };
            let dropped = base.take_if(|(_, pushed)| lost(pushed));
            match &dropped {
                Some((id, _)) if gone.parts.is_empty() => eprintln!(
                    "layr: the remote tree of {} went during the push; a complete tree goes instead",
                    crate::ids::short(id)
                ),
                _ => eprintln!(
                    "layr: {} part(s) that the tree of {} links went during the push; their files go up again",
                    gone.parts.len(),
                    crate::ids::short(&state.id)
                ),
            }
            let inherited: BTreeMap<&str, &[Link]> = dropped
                .iter()
                .flat_map(|(_, base)| base.tree.files.values())
                .map(|entry| (entry.2.as_str(), entry.3.as_slice()))
                .collect();
            let links = |link: &Link| {
                gone.parts.contains(&link.object())
                    || (link.0 == state.id && link.1 == gone.attempt)
                    || dropped.as_ref().is_some_and(|(id, base)| link.0 == *id && link.1 == base.attempt)
            };
            journal.forget(&hashes.forget(|sha, chain| chain.iter().any(links) || inherited.get(sha) == Some(&chain)))?;
            if let Some((id, _)) = &dropped {
                index.remove(id);
                present.remove(id);
            }
        };
        drop(stage);
        let stage = crate::trace::span("store checkpoint");
        let protected = hashes.protect(&pushed.tree);
        journal.checkpoint(&state.id, &pushed.attempt, pushed.depth, &hashes.dirty, &protected)?;
        hashes.dirty.clear();
        hashes.published.extend(pushed.tree.files.values().flat_map(|entry| &entry.3).map(Link::object));
        present.insert(state.id.clone(), pushed.attempt.clone());
        index.insert(state.id.clone(), cache_tree(&mut tree_cache, &pushed, bytes));
        #[cfg(test)]
        PUSH_STOP_AFTER.with(|stop| -> Result<()> {
            if stop.get() == Some(report.states + 1) {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
            }
            Ok(())
        })?;
        if (report.states + 1) % CHECKPOINT_STATES == 0 {
            checkpoint(&store.dir, &index)?;
        }
        latest = Some((state.id.clone(), pushed));
        drop(stage);
        report.states += 1;
        report.state_ids.push(state.id.clone());
        report.files += files;
        report.bytes += bytes;
    }
    checkpoint(&store.dir, &index)?;
    let stats = journal.finish()?;
    if std::env::var_os("LAYR_TRACE").is_some() {
        eprintln!("trace push-journal {}", serde_json::to_string(&stats)?);
    }
    super::gitpack::push(store, local, opts, &mut report)?;
    Ok(report)
}

/// The base of `state`: its first parent or source that the remote holds, else its nearest pushed ancestor. A state of
/// `ahead` (sent earlier in this push) counts as pushed: the walk of the ancestry stops there.
fn select_base(
    state: &crate::model::StateRec,
    present: &BTreeMap<String, String>,
    index: &BTreeMap<String, Cached>,
    ahead: &BTreeSet<&str>,
    local: &super::Replica<'_>,
) -> Result<Option<String>> {
    let known = |id: &str| ahead.contains(id) || present.contains_key(id) || index.contains_key(id);
    let direct = state.parents.iter().chain(&state.derived_from).find(|id| known(id));
    if direct.is_some() || (index.is_empty() && ahead.is_empty()) {
        return Ok(direct.cloned());
    }
    local.nearest_ancestor(&state.id, &mut |id| index.contains_key(id) || ahead.contains(id))
}

/// A layer that this machine writes and cannot read (a write grant without a read identity): it lists none of the
/// layer's objects, so it never learns whether a part of another push is still there. Each of its trees is complete,
/// and links only the parts of its own attempt and of the trees that the same push published before.
fn blind_layer(store: &Store) -> bool {
    store.layer.is_some() && !store.backend.can_read()
}

fn prepare_push(store: &Store, local: &super::Replica<'_>, opts: &Options) -> Result<(Descriptor, bool, Report)> {
    if opts.caches && serde_json::from_value::<crate::model::StateKind>(serde_json::json!("cache")).is_err() {
        bail!("store --caches is not available until cache states exist");
    }
    super::epochs::check_current(store)?;
    store.private_dir()?;
    let descriptor = store.descriptor()?;
    let blind_layer = blind_layer(store);
    let mut report = Report { omitted: local.omitted_cache_refs(opts)?, ..Default::default() };
    local.verify_records()?;
    // A list is enough for a writer without permission to read encrypted objects.
    let stage = crate::trace::span("store records");
    // A reader checks that the remote history of this machine is the local one. Another store
    // with the same machine identity writes other records under the same numbers; a push from it
    // must fail, not skip or drop its records.
    // `own-records` names the newest segment of this machine that this store wrote or checked.
    let own_file = store.dir.join("own-records");
    // A root key epoch keeps this folder and can reuse segment names: the epoch is in the name.
    let scope = store.epoch.as_ref().map_or("root", |e| e.key.epoch.as_str()).to_string();
    let known = std::fs::read_to_string(&own_file).unwrap_or_default();
    let seen = |key: &str| known.trim() == format!("{scope} {key}");
    let identity = if blind_layer { None } else { store.identity(&descriptor, None).ok() };
    let own_history = |key: &str, seq: u64| -> Result<()> {
        let Some(identity) = &identity else { return Ok(()) };
        if seen(key) {
            return Ok(());
        }
        let body = store.backend.get(key)?.context("record segment disappeared")?;
        let body = String::from_utf8(keys::decrypt(identity, &body)?)?;
        let remote = body.lines().last().unwrap_or_default();
        if local.record_line(&store.machine, seq)?.as_deref() != Some(remote) {
            bail!(
                "the remote has other records of this machine ({}, record {seq}): another store uses this machine identity; run one writer per identity",
                store.machine
            );
        }
        Ok(())
    };
    // Layr Cloud API v1: the token belongs to this machine, so only its own segments go up, signed by the key
    // that the server knows for it.
    let api = store.backend.api();
    if let Some(api) = api {
        register_machine(store, api)?;
    }
    for machine in local.record_machines()? {
        if api.is_some() && machine != store.machine {
            continue;
        }
        let mut last = 0;
        let mut last_key = None;
        for object in if blind_layer { Vec::new() } else { store.backend.list(&format!("records/{machine}/"))? } {
            let (m, _, end) = format::parse_segment_name(&object.key).context("invalid remote segment name")?;
            if m != machine {
                bail!("segment is outside its machine prefix");
            }
            if end > last {
                last = end;
                last_key = Some(object.key);
            }
        }
        if let Some(key) = last_key.as_deref().filter(|_| machine == store.machine) {
            own_history(key, last)?;
            if !seen(key) {
                std::fs::write(&own_file, format!("{scope} {key}"))?;
            }
        }
        let mut lines = Vec::new();
        local.visit_records(Some((&machine, last)), &mut |line| {
            lines.push(line.to_owned());
            Ok(())
        })?;
        if lines.is_empty() {
            continue;
        }
        let mut start = 0;
        while start < lines.len() {
            let mut end = start;
            let mut size = 0;
            while end < lines.len() && (end == start || size + lines[end].len() + 1 <= RECORD_SEGMENT_BYTES) {
                size += lines[end].len() + 1;
                end += 1;
            }
            let first = oplog::parse_line(&lines[start])?.record.seq;
            let last = oplog::parse_line(&lines[end - 1])?.record.seq;
            let mut bytes = lines[start..end].join("\n").into_bytes();
            bytes.push(b'\n');
            // Publish ordered, bounded segments. A retry resumes after the last complete segment.
            let name = format::segment_name(&machine, first, last);
            if !store.backend.put_new(&name, keys::encrypt(&descriptor.recipient, &bytes)?)? && machine == store.machine {
                own_history(&name, last)?;
            }
            if machine == store.machine {
                std::fs::write(&own_file, format!("{scope} {name}"))?;
            }
            report.records += end - start;
            start = end;
        }
    }
    drop(stage);
    Ok((descriptor, blind_layer, report))
}

fn content(root: &Path, path: &str, mode: &str) -> Result<Vec<u8>> {
    if mode == "120000" {
        use std::os::unix::ffi::OsStrExt;
        return Ok(std::fs::read_link(fsutil::safe_join(root, path)?)?.as_os_str().as_bytes().to_vec());
    }
    let path = fsutil::safe_join(root, path)?;
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() {
        bail!("state contains a special file");
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

/// Why the store format cannot restore this tree exactly, or None. The format keeps file content,
/// symbolic links and git-like modes (`format.rs`). It keeps no extended attributes, ACLs, hard
/// links, empty folders or nested subvolumes, so retention must not evict a tree that has them.
pub fn format_gap(root: &Path) -> Result<Option<&'static str>> {
    use std::os::unix::fs::MetadataExt;
    let mut pending = vec![root.to_path_buf()];
    while let Some(folder) = pending.pop() {
        let mut entries = 0;
        for entry in std::fs::read_dir(&folder)? {
            let path = entry?.path();
            entries += 1;
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                if crate::fs::host().namespace.managed(&path) {
                    return Ok(Some("a nested subvolume"));
                }
                pending.push(path.clone());
            } else if meta.is_file() && meta.nlink() > 1 {
                return Ok(Some("a hard link"));
            }
            if !meta.file_type().is_symlink() && has_xattrs(&path)? {
                return Ok(Some("extended attributes or an ACL"));
            }
        }
        if entries == 0 && folder != root {
            return Ok(Some("an empty folder"));
        }
    }
    Ok(None)
}

/// macOS sets `com.apple.provenance` on every new file. It is not user data, so it does not count.
const OS_MARKER: &[u8] = b"com.apple.provenance";

#[cfg(target_os = "macos")]
fn has_xattrs(path: &Path) -> Result<bool> {
    Ok(crate::sys::apfs_xattrs(path)?.iter().any(|(name, _)| name != OS_MARKER) || crate::sys::apfs_acl_text(path)?.is_some())
}

#[cfg(not(target_os = "macos"))]
fn has_xattrs(path: &Path) -> Result<bool> {
    let mut size = 1024;
    let names = loop {
        let mut buf = vec![0u8; size];
        match rustix::fs::listxattr(path, &mut buf[..]) {
            Ok(len) => {
                buf.truncate(len);
                break buf;
            }
            Err(rustix::io::Errno::RANGE) if size < 1 << 20 => size *= 4,
            Err(e) => return Err(e.into()),
        }
    };
    // Overlay trees mark every file with `trusted.layr.origin` (its file identity). Layr writes it
    // on each copy, so a restored tree gets it again: it does not count.
    Ok(names.split(|b| *b == 0).any(|name| !name.is_empty() && name != OS_MARKER && name != b"trusted.layr.origin"))
}

/// Fail on unreadable folders or non-UTF-8 names rather than omit their files.
fn walk_state(root: &Path, visit: &mut dyn FnMut(&str, &fsutil::Meta) -> Result<bool>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(folder) = pending.pop() {
        let mut entries = std::fs::read_dir(&folder)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_str().context("state path is not UTF-8")?;
            let metadata = std::fs::symlink_metadata(&path)?;
            let meta = fsutil::meta_of(&metadata);
            if meta.is_dir() && crate::fs::host().namespace.managed(&path) {
                continue;
            }
            if visit(relative, &meta)? && meta.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

/// The tree of `attempt` did not publish: its base tree went from the remote while its parts went up (`parts` is
/// empty), or the data parts `parts` that it links and that it did not write went.
#[derive(Debug)]
struct Unpublished {
    attempt: String,
    parts: BTreeSet<String>,
}
impl std::fmt::Display for Unpublished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.parts.len() {
            0 => write!(f, "the base went from the remote before attempt {} published", self.attempt),
            n => write!(f, "{n} linked part(s) went from the remote before attempt {} published", self.attempt),
        }
    }
}
impl std::error::Error for Unpublished {}

/// `remote` asks the remote right before the tree publishes, after the parts are up: another machine's sweep can
/// delete the base tree (asked when `probe`) or, in a layer, the parts of trees that it deletes. A sweep deletes no
/// part that a tree it keeps links, but a chain that this push reuses (`hashes`) can come from a tree that went, so
/// in a layer (or on a partial replica, `Attempts::check_parts`) every part that the tree links and that this attempt
/// did not write must still be listed. The parts that
/// a tree of this push linked are not asked for again (`ContentHashes::published`): a push of interleaved lines pays no
/// request per tree for them, and another tree pays one listing or a probe per state of the parts it reuses.
#[allow(clippy::too_many_arguments)]
fn upload_state_journal(
    store: &Store,
    descriptor: &Descriptor,
    state: &str,
    root: &Path,
    base: Option<&(String, Pushed)>,
    hashes: &mut ContentHashes,
    chunk_hashes: &mut BTreeMap<String, Vec<String>>,
    base_root: Option<&Path>,
    btrfs: Option<format::BtrfsIdentity>,
    mut journal: Option<&mut super::push_journal::Journal>,
    remote: Option<(&mut Attempts<'_>, bool)>,
) -> Result<(Pushed, usize, u64)> {
    let started = std::time::Instant::now();
    let started_at = push_now();
    let mut random = [0u8; 4];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let attempt = hex::encode(random);
    #[cfg(test)]
    let attempt = if PUSH_DETERMINISTIC.with(|enabled| enabled.get()) {
        hex::encode(ContentAddress::digest(state.as_bytes()))[..8].to_string()
    } else {
        attempt
    };
    let delta_root =
        base_root.filter(|base| crate::fs::host().namespace.managed(base) && crate::fs::host().namespace.managed(root));
    let mut files =
        if delta_root.is_some() { base.map(|(_, p)| p.tree.files.clone()).unwrap_or_default() } else { BTreeMap::new() };
    let mut times =
        if delta_root.is_some() { base.map(|(_, p)| p.tree.times.clone()).unwrap_or_default() } else { BTreeMap::new() };
    let mut chunks =
        if delta_root.is_some() { base.map(|(_, p)| p.tree.chunks.clone()).unwrap_or_default() } else { BTreeMap::new() };
    let mut part = 0;
    let mut frames = 0;
    let mut bytes = 0;
    let blind = blind_layer(store);
    let stage = crate::trace::span("store walk and frames");
    let put = |(part, data, entries): PartJob| put_part(store, descriptor, state, &attempt, part, data, &entries);
    // Parts and chunk packs go up on the transfer pool while the walk compresses the next ones; the scope ends after all
    // of them.
    std::thread::scope(|scope| -> Result<()> {
        let mut parts = PartPool::new(scope, super::download::JOBS, super::download::TRANSFER_BUFFER_BUDGET as u64, put);
        // A cache layer that this machine can open and list goes up as chunks (`chunked`); others as frames. A blind
        // writer with a locally recovered layer key still cannot list the packs.
        let layer =
            store.layer.as_ref().filter(|_| !blind).and_then(|layer| Some((layer.identity.as_deref()?, layer.chunk_key?)));
        let mut chunker = match layer {
            Some((identity, key)) => Some(super::chunked::Writer::open(
                scope,
                store,
                &descriptor.recipient,
                identity,
                key,
                std::mem::take(chunk_hashes),
            )?),
            None => None,
        };
        let mut current = Vec::new();
        let mut part_entries = Vec::new();
        let (changed, touched) = match delta_root {
            Some(base) => {
                let (changes, touched) = crate::changes::file_changes_and_touched(base, root)?;
                (Some(changes), touched)
            }
            None => (None, Vec::new()),
        };
        // Equal content with a new time: the tree keeps the file and takes the new time.
        for path in &touched {
            if let Some(time) = file_time(root, path) {
                times.insert(path.clone(), time);
            }
        }
        if let Some(changes) = &changed {
            for change in changes {
                if let Some(old) = change.old_path() {
                    files.remove(old);
                    times.remove(old);
                    chunks.remove(old);
                }
                files.remove(change.path());
                times.remove(change.path());
                chunks.remove(change.path());
            }
        }
        // A delta keeps the entries of the base. A framed entry of the base in a layer that goes up as chunks (a cache
        // pushed before chunks) is read again, so it moves to chunks once, as in a full walk below.
        let framed: Vec<String> = match (&changed, chunker.is_some()) {
            (Some(_), true) => {
                files.iter().filter(|(p, e)| e.1 > 0 && !chunks.contains_key(*p)).map(|(p, _)| p.clone()).collect()
            }
            _ => Vec::new(),
        };
        for path in &framed {
            files.remove(path);
            times.remove(path);
        }
        // Read and SHA-256 time of the walk for `LAYR_TRACE`.
        let mut walk_time = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let mut visit = |path: &str, meta: &fsutil::Meta| -> Result<bool> {
            if crate::repo::skip_path(path) {
                return Ok(false);
            }
            if meta.is_dir() {
                return Ok(true);
            }
            if !format::safe_path(path) {
                bail!("state has an unsafe path");
            }
            if meta.ftype == fsutil::FType::Special {
                bail!("state has a special file: {path}");
            }
            let mode = format!("{:o}", meta.git_mode());
            let time = file_time(root, path);
            // The file and its frames are held beside the uploads: their room is made before the file is read. Work
            // larger than the budget waits until no upload travels.
            match chunker.as_mut() {
                Some(chunker) => chunker.reserve(meta.size)?,
                None => {
                    let charge = building_charge().saturating_add(walk_charge(meta.size, base_root.is_some()));
                    parts.wait_for(charge, &mut |_, ()| Ok(()))?;
                }
            }
            let started = std::time::Instant::now();
            let data = content(root, path, &mode)?;
            let read = started.elapsed();
            let sha = hex::encode(ContentAddress::digest(&data));
            walk_time.0 += read;
            walk_time.1 += started.elapsed() - read;
            if let Some(time) = time {
                times.insert(path.to_string(), time);
            }
            if let Some(chunker) = chunker.as_mut().filter(|_| !data.is_empty()) {
                let ids = chunker.file(&sha, &data)?;
                files.insert(path.to_string(), Entry(mode, data.len() as u64, sha, Vec::new()));
                chunks.insert(path.to_string(), ids);
                return Ok(true);
            }
            let size = data.len() as u64;
            // A blind writer cannot see whether a part of an earlier push is still there: it reuses only the parts of
            // this attempt and of the trees that this push published.
            let held = |link: &Link| (link.0 == state && link.1 == attempt) || hashes.published.contains(&link.object());
            if let Some(chain) = hashes.get(&sha).filter(|chain| !blind || chain.iter().all(held)) {
                files.insert(path.to_string(), Entry(mode, size, sha, chain.clone()));
                return Ok(true);
            }
            let full = zst::compress(&data, None)?;
            let mut frame = full;
            let mut chain = Vec::new();
            if let (Some((_, pushed)), Some(base_root)) = (base, base_root) {
                if let Some(previous) = pushed.tree.files.get(path) {
                    if previous.3.len() <= format::MAX_FILE_DELTAS
                        && data.len() <= format::DELTA_LIMIT
                        && previous.1 <= format::DELTA_LIMIT as u64
                    {
                        if let Ok(old) = content(base_root, path, &previous.0) {
                            if !old.is_empty() && hex::encode(ContentAddress::digest(&old)) == previous.2 {
                                let delta = zst::compress(&data, Some(&old))?;
                                if delta.len() < frame.len() {
                                    frame = delta;
                                    chain = previous.3.clone();
                                }
                            }
                        }
                    }
                }
            }
            drop(data);
            let names = (state, attempt.as_str());
            if current.len() + frame.len() > format::PART_LIMIT {
                flush_part(&mut parts, journal.as_deref_mut(), names, &mut part, &mut current, &mut part_entries)?;
            }
            if frame.len() > format::PART_LIMIT {
                // A part of its own, built only when no other part travels (`send_part`).
                parts.wait_for(u64::MAX, &mut |_, ()| Ok(()))?;
                #[cfg(test)]
                part_event(state, format!("build {part}"));
            }
            let link = Link(
                state.to_string(),
                attempt.clone(),
                part,
                current.len() as u64,
                frame.len() as u64,
                if chain.is_empty() { "full" } else { "delta" }.into(),
            );
            bytes += frame.len() as u64;
            if current.is_empty() {
                // A compressible file leaves a small frame in a buffer of the file's size: the part keeps only its
                // bytes (`building_charge`).
                frame.shrink_to_fit();
                current = frame;
            } else {
                current.extend(frame);
            }
            chain.insert(0, link);
            hashes.insert(sha.clone(), chain.clone());
            let entry = Entry(mode, size, sha, chain);
            part_entries.push(entry.clone());
            files.insert(path.to_string(), entry);
            frames += 1;
            // An oversized frame goes now, not beside the next file.
            if current.len() > format::PART_LIMIT {
                flush_part(&mut parts, journal.as_deref_mut(), names, &mut part, &mut current, &mut part_entries)?;
            }
            Ok(true)
        };
        if let Some(changes) = &changed {
            for change in changes {
                if let Some(meta) = change.new() {
                    visit(change.path(), meta)?;
                }
            }
            for path in &framed {
                let meta = std::fs::symlink_metadata(fsutil::safe_join(root, path)?)?;
                visit(path, &fsutil::meta_of(&meta))?;
            }
        } else {
            walk_state(root, &mut visit)?;
        }
        drop(visit);
        if std::env::var_os("LAYR_TRACE").is_some() {
            eprintln!("trace {:>8.1} ms  store walk read", walk_time.0.as_secs_f64() * 1000.0);
            eprintln!("trace {:>8.1} ms  store walk sha256", walk_time.1.as_secs_f64() * 1000.0);
        }
        flush_part(&mut parts, journal.as_deref_mut(), (state, &attempt), &mut part, &mut current, &mut part_entries)?;
        parts.finish(&mut |_, ()| Ok(()))?;
        if let Some(mut chunker) = chunker {
            chunker.finish(root, &chunks, &|root: &Path, path: &str| content(root, path, &files[path].0))?;
            bytes += chunker.bytes;
            frames += chunker.packs;
            *chunk_hashes = chunker.into_hashes();
        }
        Ok(())
    })?;
    drop(stage);
    let stage = crate::trace::span("store tree encode");
    let tree = Tree { files, times, chunks };
    let canonical_tree = tree.canonical()?;
    // A blind writer's base without a probe is the tree that this push published just before (`held`): a reader of what
    // this push published decodes a delta on it. On another base, as on a partial replica, the tree goes up complete.
    let complete = remote.as_ref().is_some_and(|(attempts, probe)| attempts.complete_trees || (blind && *probe));
    let delta_base = base.filter(|(_, p)| !complete && p.depth + 1 < format::FULL_TREE_EVERY);
    let encoded_base;
    let base_bytes = if let Some((_, parent)) = delta_base {
        if parent.canonical.is_empty() {
            encoded_base = parent.tree.canonical()?;
            Some(encoded_base.as_slice())
        } else {
            Some(parent.canonical.as_slice())
        }
    } else {
        None
    };
    let body = zst::compress(&canonical_tree, base_bytes)?;
    let mut header = Header {
        v: 2,
        state: state.into(),
        attempt: attempt.clone(),
        tree: if delta_base.is_some() { "delta" } else { "full" }.into(),
        base: delta_base.map(|(id, p)| (id.clone(), p.attempt.clone())),
        parts: part,
        machine: store.machine.clone(),
        btrfs,
        sig: String::new(),
    };
    header.sign(&canonical_tree, &store.key)?;
    let mut object = canonical(&serde_json::to_value(&header)?)?;
    object.push(b'\n');
    object.extend(&body);
    push_failpoint("before_publish", state);
    #[cfg(test)]
    if std::env::var("LAYR_TEST_GC_STATE").ok().as_deref() == Some(state) {
        std::fs::write(std::env::var_os("LAYR_TEST_PUSH_CLOCK").unwrap(), chrono::Utc::now().timestamp().to_string()).unwrap();
        store.gc(false).unwrap();
    }
    // GC may collect unpublished parts at server modified +24h. Pending links keep
    // that server timestamp across crashes; publish at least one hour before it.
    if started.elapsed().as_secs() >= 23 * 3600 || push_now() >= hashes.deadline(&tree, started_at) {
        bail!("state upload or recovered part reached its safe publication deadline; retry with a new attempt");
    }
    drop(stage);
    let _publish = crate::trace::span("store tree publish");
    #[cfg(test)]
    PUSH_BEFORE_TREE.with(|hook| hook.borrow_mut().as_mut().map(|hook| hook(state, base.map_or("", |(id, _)| id))));
    if let Some((attempts, probe)) = remote {
        if let Some((id, pushed)) = base.filter(|_| probe) {
            if !attempts.has_tree(id, &pushed.attempt, true)? {
                return Err(Unpublished { attempt, parts: BTreeSet::new() }.into());
            }
        }
        if attempts.check_parts {
            let own = |link: &&Link| link.0 == state && link.1 == attempt;
            let linked: BTreeSet<String> = tree
                .files
                .values()
                .flat_map(|entry| &entry.3)
                .filter(|link| !own(link))
                .map(Link::object)
                .filter(|object| !hashes.published.contains(object))
                .collect();
            let parts = if linked.is_empty() { BTreeSet::new() } else { attempts.missing(linked)? };
            if !parts.is_empty() {
                return Err(Unpublished { attempt, parts }.into());
            }
        }
    }
    if !store.backend.put_new(&format::tree_name(state, &attempt), keys::encrypt(&descriptor.recipient, &object)?)? {
        bail!("state attempt already exists; retry with a new attempt");
    }
    push_failpoint("after_publish", state);
    bytes += body.len() as u64;
    Ok((Pushed { attempt, tree, depth: delta_base.map_or(0, |(_, p)| p.depth + 1), canonical: canonical_tree }, frames, bytes))
}

#[cfg(test)]
fn upload_state(
    store: &Store,
    descriptor: &Descriptor,
    state: &str,
    root: &Path,
    base: Option<&(String, Pushed)>,
    hashes: &mut BTreeMap<String, Vec<Link>>,
    base_root: Option<&Path>,
    btrfs: Option<format::BtrfsIdentity>,
) -> Result<(Pushed, usize, u64)> {
    let mut index = ContentHashes { entries: std::mem::take(hashes), ..Default::default() };
    let result = upload_state_journal(
        store,
        descriptor,
        state,
        root,
        base,
        &mut index,
        &mut BTreeMap::new(),
        base_root,
        btrfs,
        None,
        None,
    );
    *hashes = index.entries;
    result
}

/// Records which chunks the files of a pushed tree have.
fn seed_chunks(chunk_hashes: &mut BTreeMap<String, Vec<String>>, tree: &Tree) {
    for (path, ids) in &tree.chunks {
        if let Some(entry) = tree.files.get(path) {
            chunk_hashes.entry(entry.2.clone()).or_insert_with(|| ids.clone());
        }
    }
}

fn btrfs_identity(root: &Path, signed_uuid: &str) -> Option<format::BtrfsIdentity> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        let (uuid, received_transid) = crate::fs::host().transfer.clone_identity(root).ok()?;
        if uuid != signed_uuid {
            return None;
        }
        let transid = received_transid.or_else(|| crate::fs::host().transfer.send_identity(root).ok().map(|(_, t)| t))?;
        return Some(format::BtrfsIdentity { uuid, transid });
    }
    #[cfg(not(all(target_os = "linux", not(feature = "overlayfs"))))]
    {
        let _ = (root, signed_uuid);
        None
    }
}

fn push_failpoint(point: &str, state: &str) {
    #[cfg(test)]
    if std::env::var("LAYR_TEST_KILL_POINT").ok().as_deref() == Some(point)
        && std::env::var("LAYR_TEST_KILL_STATE").ok().as_deref() == Some(state)
    {
        std::process::Command::new("kill").args(["-KILL", &std::process::id().to_string()]).status().unwrap();
    }
    let _ = (point, state);
}

/// Registers this machine's signing key with Layr Cloud once per key (docs/remotes.md, "Machine key"). The
/// `machine-key` file of the remote folder remembers the key that the server took.
fn register_machine(store: &Store, api: &super::api::Api) -> Result<()> {
    let public = hex::encode(store.key.verifying_key().to_bytes());
    let marker = store.dir.join("machine-key");
    if std::fs::read_to_string(&marker).is_ok_and(|text| text.trim() == public) {
        return Ok(());
    }
    api.register_machine(&store.machine, &store.key.verifying_key())?;
    std::fs::write(marker, &public)?;
    Ok(())
}

fn put_part(
    store: &Store,
    descriptor: &Descriptor,
    state: &str,
    attempt: &str,
    part: u32,
    data: Vec<u8>,
    entries: &[Entry],
) -> Result<()> {
    push_failpoint("before_part", state);
    let _publish = crate::trace::span("store part publish");
    if part >= format::MAX_PARTS {
        bail!("state exceeds 10000 data parts");
    }
    #[cfg(test)]
    part_event(state, format!("start {part}"));
    let body = keys::encrypt(&descriptor.recipient, &data)?;
    // Only the ciphertext travels (`part_charge`).
    drop(data);
    let created = store.backend.put_new(&format::data_name(state, attempt, part), body);
    #[cfg(test)]
    part_event(state, format!("end {part}"));
    if !created? {
        bail!("state attempt has an existing part; retry with a new attempt");
    }
    #[cfg(test)]
    if let Ok(path) = std::env::var("LAYR_TEST_PUT_LOG") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(file, "{}", serde_json::json!({"object":format::data_name(state,attempt,part),"hashes":entries.iter().map(|entry|entry.2.clone()).collect::<Vec<_>>()})).unwrap();
        file.sync_all().unwrap();
    }
    let _ = entries;
    push_failpoint("after_part", state);
    Ok(())
}

/// Part uploads and oversized-part builds by state, in order: tests check which parts overlapped.
#[cfg(test)]
static PART_EVENTS: std::sync::Mutex<BTreeMap<String, Vec<String>>> = std::sync::Mutex::new(BTreeMap::new());
#[cfg(test)]
fn part_event(state: &str, event: String) {
    PART_EVENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(state.to_string()).or_default().push(event);
}

/// The modification time of `path` in the tree `root`, in nanoseconds since the epoch (`Tree::times`).
fn file_time(root: &Path, path: &str) -> Option<u64> {
    let meta = std::fs::symlink_metadata(fsutil::safe_join(root, path).ok()?).ok()?;
    let since = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::ctx::{Caller, Ctx};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    pub(in crate::store_remote) fn context(root: &Path) -> Ctx {
        Ctx {
            store: crate::store::Store::open(&root.join("local")).unwrap(),
            caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.to_path_buf(),
            env: HashMap::new(),
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        }
    }
    pub(in crate::store_remote) fn remote(root: &Path, ctx: &Ctx) -> Store {
        Store {
            backend: super::super::backend::Backend::file(root.join("remote")),
            url: format!("file://{}", root.join("remote").display()),
            project: "01a12290-0000-7000-8000-000000000002".into(),
            machine: ctx.store.machine.id.clone(),
            key: ctx.store.machine.key.clone(),
            dir: root.join("local/meta"),
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
    #[test]
    fn initialization_recovers_after_each_publication_and_keeps_the_key_private() {
        for stop in ["paper", "machine", "store"] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            let path = root.join("recovery.txt");
            let opts = super::super::ConfigureOptions { paper_key_out: Some(path.clone()), ..Default::default() };
            INIT_STOP.with(|value| *value.borrow_mut() = Some(stop));
            assert!(store.configure(&ctx, &opts).is_err());
            INIT_STOP.with(|value| *value.borrow_mut() = None);
            let result = store.configure(&ctx, &opts).unwrap();
            assert_eq!(result.created, stop != "store");
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            let words = std::fs::read_to_string(&path).unwrap();
            assert_eq!(words.split_whitespace().count(), 24);
            let descriptor = store.descriptor().unwrap();
            let identity = store.identity(&descriptor, Some(&words)).unwrap();
            assert_eq!(keys::recipient_of(&identity).unwrap(), descriptor.recipient);
            for object in store.backend.list("").unwrap() {
                let bytes = store.backend.get(&object.key).unwrap().unwrap();
                assert!(!bytes.windows(words.trim().len()).any(|part| part == words.trim().as_bytes()));
            }
            assert!(!serde_json::to_string(&result).unwrap().contains(words.trim()));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn live_refs_keep_auto_states_and_auxiliary_states_obey_the_cache_option() {
        use crate::model::StateRec;
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        std::fs::create_dir_all(dir.join("states")).unwrap();
        let project = crate::store::Project {
            lease: Default::default(),
            store: &ctx.store,
            name: "p".into(),
            dir: dir.clone(),
            id: store.project.clone(),
            db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let mut record = crate::model::DraftRecord::new("project");
        record.data = serde_json::json!({"id":store.project,"name":"p","config":{}});
        project.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
        let mut source = None;
        let mut cache = String::new();
        for kind in ["commit", "auto", "derived", "cache"] {
            let id = crate::ids::new_id();
            std::fs::create_dir(project.state_path(&id)).unwrap();
            std::fs::write(project.state_path(&id).join("content"), kind).unwrap();
            let state: StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":kind,
                "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
                "derived_from":if kind=="derived"||kind=="cache" {source.clone()}else{None},
                "parents":if kind=="auto" {source.clone().into_iter().collect::<Vec<_>>()}else{vec![]},
                "time":crate::ids::now_ms()}))
            .unwrap();
            let mut record = crate::model::DraftRecord::new("state");
            record.states.push(state);
            project.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
            if kind == "commit" {
                source = Some(id);
                continue;
            }
            let name = format!(
                "{}/test",
                if kind == "auto" {
                    "pins"
                } else if kind == "cache" {
                    "caches"
                } else {
                    "derived"
                }
            );
            let mut record = crate::model::DraftRecord::new("ref.set");
            record.data = serde_json::json!({"ref_change":{"name":name,"before":null,"after":{"name":name,"target":id}}});
            project.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
            if kind == "cache" {
                cache = id;
            }
        }
        let repo = Repo { restore_from_remote: true, p: project, ctx: None };
        let report = push(&store, &ctx, &repo, &Options::default()).unwrap();
        assert_eq!(report.states, 3);
        assert!(report.omitted.contains(&format!("cache state {cache} of ref caches/test: fetch --overlay <overlay> takes it")));
        assert!(!reader::attempts(&store.backend).unwrap().contains_key(&cache));
        let report = push(&store, &ctx, &repo, &Options { caches: true, ..Default::default() }).unwrap();
        assert_eq!(report.states, 1);
        assert!(reader::attempts(&store.backend).unwrap().contains_key(&cache));
        store.verify(true).unwrap();
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dropping_a_read_view_keeps_the_command_pin() -> Result<()> {
        use layr_store::sync::LocalReplica;
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&root)?;
        let cleanup = crate::repo::Temp::plain(root.clone());
        let ctx = context(&root);
        let store = remote(&root, &ctx);
        let repo = branch_fixture(&ctx, &store);
        let state = repo.p.db.present_states()?.remove(0).0;
        let mut keep = crate::model::DraftRecord::new("ref.set");
        keep.data = serde_json::json!({"ref_change":{"name":"pins/wp0b","before":null,
            "after":{"name":"pins/wp0b","target":state}}});
        repo.p.lock_maintenance()?.append(keep, &ctx.actor())?;
        let local = crate::sync_local::Local::read(&ctx, &repo);
        local.pin_state(&state)?;
        assert!(repo.p.db.is_pinned(&state)?, "fixture must own a real command pin");
        let first = local.open_state(&state)?;
        let second = local.open_state(&state)?;
        drop(first);
        assert!(
            repo.p.db.reserve_trees(std::slice::from_ref(&state), "wp0b-reader")?.is_empty(),
            "a live command/read view lost its pin"
        );
        drop(second);
        assert!(repo.p.db.reserve_trees(&[state], "wp0b-reader")?.is_empty(), "the command pin must outlive every view");
        crate::free::finish(&ctx);
        drop(repo);
        cleanup.discard()
    }

    fn branch_fixture<'a>(ctx: &'a Ctx, store: &Store) -> Repo<'a> {
        use crate::model::StateRec;
        let dir = ctx.store.project_dir("p").unwrap();
        let existing = dir.join("meta/db/layr.sqlite").exists();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        std::fs::create_dir_all(dir.join("states")).unwrap();
        let project = crate::store::Project {
            lease: Default::default(),
            store: &ctx.store,
            name: "p".into(),
            dir: dir.clone(),
            id: store.project.clone(),
            db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        if !existing {
            let mut record = crate::model::DraftRecord::new("project");
            record.data = serde_json::json!({"id":store.project,"name":"p","config":{}});
            project.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
            let mut parents = Vec::<String>::new();
            let mut first = None;
            for n in 0..4 {
                let id = crate::ids::new_id();
                std::fs::create_dir(project.state_path(&id)).unwrap();
                let version = if n == 3 { 1 } else { n };
                let payload = project.state_path(&id).join("payload");
                std::fs::write(&payload, format!("version {version}\n")).unwrap();
                // Model each content version's mtime explicitly, independently of the
                // filesystem clock's resolution when these states are created.
                std::fs::File::options()
                    .write(true)
                    .open(&payload)
                    .unwrap()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(version))
                    .unwrap();
                std::fs::write(project.state_path(&id).join("stable"), b"unchanged parent content\n").unwrap();
                let state: StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":"commit",
                    "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
                    "parents":parents,"time":crate::ids::now_ms()}))
                .unwrap();
                let mut record = crate::model::DraftRecord::new("state");
                record.states.push(state);
                project.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
                if n == 0 {
                    first = Some(id.clone());
                }
                parents = vec![if n == 1 { first.clone().unwrap() } else { id }];
            }
        }
        Repo { restore_from_remote: true, p: project, ctx: None }
    }

    #[test]
    fn push_reads_changed_content_with_the_same_size_and_mtime() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let repo = branch_fixture(&ctx, &store);
        let mut expected = BTreeMap::new();
        for (id, _) in repo.p.db.present_states().unwrap() {
            let payload = repo.p.state_path(&id).join("payload");
            let bytes = std::fs::read(&payload).unwrap();
            // All versions have equal sizes. Model a rewrite that preserves the parent's
            // timestamp, as a copy with preserved metadata or a same-tick write can do.
            std::fs::write(&payload, &bytes).unwrap();
            std::fs::File::options().write(true).open(&payload).unwrap().set_modified(std::time::UNIX_EPOCH).unwrap();
            expected.insert(id, bytes);
        }
        assert_eq!(push(&store, &ctx, &repo, &Options::default()).unwrap().states, expected.len());
        let descriptor = store.descriptor().unwrap();
        let identity = store.identity(&descriptor, None).unwrap();
        let records = reader::records(&store.backend, &identity, &descriptor.project).unwrap();
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        for (id, attempt) in reader::attempts(&store.backend).unwrap() {
            let (_, _, tree) = source.tree(&id, &attempt).unwrap();
            assert_eq!(source.read(&tree.files["payload"]).unwrap(), expected[&id]);
        }
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enospc_after_publication_resumes_with_missing_or_corrupt_cache() {
        for corrupt in [false, true] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            // With no cache, the third state must hydrate a parent uploaded in this same push.
            PUSH_DETERMINISTIC.with(|enabled| enabled.set(true));
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(corrupt));
            PUSH_STOP_AFTER.with(|stop| stop.set(Some(3)));
            let error = push(&store, &ctx, &repo, &Options::default()).unwrap_err();
            PUSH_STOP_AFTER.with(|stop| stop.set(None));
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
            assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().raw_os_error(), Some(libc::ENOSPC));
            assert!(!store.dir.join("pushed-v2.json.zst").exists());
            let before: BTreeMap<_, _> = store
                .backend
                .list("")
                .unwrap()
                .into_iter()
                .map(|object| {
                    let bytes = store.backend.get(&object.key).unwrap().unwrap();
                    (object.key, ContentAddress::digest(bytes))
                })
                .collect();
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 3);
            if corrupt {
                std::fs::write(store.dir.join("push-cache.sqlite3"), b"damaged cache").unwrap();
            }
            let report = push(&store, &ctx, &repo, &Options::default()).unwrap();
            assert_eq!(report.states, 1);
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
            assert_eq!(store.backend.list("states/").unwrap().iter().filter(|o| o.key.ends_with(".tree.age")).count(), 4);
            for (key, digest) in before {
                assert_eq!(ContentAddress::digest(store.backend.get(&key).unwrap().unwrap()), digest);
            }
            store.verify(true).unwrap();
            // A zero-capacity cache must publish byte-for-byte the same compressed object payloads as
            // the warm-cache push, including the signed tree compression/checkpoint cadence.
            let mut empty_cache = remote(&root, &ctx);
            empty_cache.backend = super::super::backend::Backend::file(root.join("uncached-remote"));
            empty_cache.dir = root.join("uncached-meta");
            empty_cache
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("uncached-paper")), ..Default::default() },
                )
                .unwrap();
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(true));
            assert_eq!(push(&empty_cache, &ctx, &repo, &Options::default()).unwrap().states, 4);
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
            PUSH_DETERMINISTIC.with(|enabled| enabled.set(false));
            let payloads = |store: &Store| -> BTreeMap<String, Vec<u8>> {
                let descriptor = store.descriptor().unwrap();
                let identity = store.identity(&descriptor, None).unwrap();
                store
                    .backend
                    .list("states/")
                    .unwrap()
                    .into_iter()
                    .map(|object| {
                        let payload = store.backend.decrypted(&object.key, &identity).unwrap();
                        (object.key, payload)
                    })
                    .collect()
            };
            // age adds randomized header stanzas whose wire lengths can differ even
            // for identical input. Compare the exact bytes passed to encryption.
            assert_eq!(payloads(&store), payloads(&empty_cache));
            // Losing both caches must only rediscover immutable uploads, never republish them.
            std::fs::remove_file(store.dir.join("push-cache.sqlite3")).unwrap();
            std::fs::remove_file(store.dir.join("pushed-v2.json.zst")).unwrap();
            assert_eq!(push(&store, &ctx, &repo, &Options::default()).unwrap().states, 0);
            drop(repo);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    #[ignore = "subprocess helper for crash_content_hashes"]
    fn crash_push_process() {
        // CI runs ignored tests too (--include-ignored). Without the variable this is not a
        // subprocess run, so do nothing. The crash tests fail if their subprocess does not push.
        let Some(root) = std::env::var_os("LAYR_TEST_CRASH_ROOT").map(std::path::PathBuf::from) else {
            return;
        };
        let ctx = context(&root);
        let store = remote(&root, &ctx);
        let repo = branch_fixture(&ctx, &store);
        PUSH_CACHE_DISABLED.with(|disabled| disabled.set(true));
        push(&store, &ctx, &repo, &Options::default()).unwrap();
        super::super::backend::trace_requests();
    }

    #[test]
    fn parent_hydration_reuses_verified_record_segments() {
        use std::process::Command;
        let mut measurements = Vec::new();
        for recovery in [false, true] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            assert_eq!(push(&store, &ctx, &repo, &Options::default()).unwrap().states, 4);
            let descriptor = store.descriptor().unwrap();
            // Model a remote with many small historical pushes, already checked by fetch.
            let mut records: Vec<_> = (0..128).map(|_| crate::model::DraftRecord::new("state")).collect();
            let appended = repo.p.lock_maintenance().unwrap().append_batch(&mut records, &ctx.actor()).unwrap();
            for record in appended {
                let key = format::segment_name(&store.machine, record.seq, record.seq);
                let line = repo.p.db.record_line(&store.machine, record.seq).unwrap().unwrap();
                store
                    .backend
                    .put_new(&key, keys::encrypt(&descriptor.recipient, format!("{line}\n").as_bytes()).unwrap())
                    .unwrap();
                std::fs::write(store.dir.join("own-records"), format!("root {key}")).unwrap();
            }
            let identity = store.identity(&descriptor, None).unwrap();
            let local: Vec<_> = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect();
            reader::records_with_local(&store.backend, &identity, &store.project, &local, Some(&reader::seen_file(&store.dir)))
                .unwrap();
            let parent = repo.p.db.present_states().unwrap()[0].0.clone();
            add_state(&ctx, &repo, std::slice::from_ref(&parent), &[("payload", b"incremental payload\n")]).unwrap();
            // Exercise both a lost parent cache and discovery after an interrupted push.
            std::fs::remove_file(store.dir.join("push-cache.sqlite3")).unwrap();
            if recovery {
                let mut index = load_index(&store.dir).unwrap();
                index.remove(&parent);
                checkpoint(&store.dir, &index).unwrap();
                drop(super::super::push_journal::Journal::open(&store.dir).unwrap());
            }
            drop(repo);
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "store_remote::writer::tests::crash_push_process", "--nocapture"])
                .env("LAYR_TEST_CRASH_ROOT", &root)
                .env("LAYR_TRACE", "1")
                .env("LAYR_ROOT", root.join("local"))
                .env("LAYR_DIRECT", "1")
                .output()
                .unwrap();
            let log = String::from_utf8(output.stderr).unwrap();
            assert!(output.status.success(), "{log}");
            let gets: usize = log
                .lines()
                .find_map(|line| line.split_once("remote get records: "))
                .map(|(_, tail)| tail.split_whitespace().next().unwrap().parse().unwrap())
                .unwrap_or(0);
            eprintln!("parent hydration recovery={recovery}: {gets} record GETs");
            measurements.push((recovery, gets));
            store.verify(true).unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
        for (recovery, gets) in measurements {
            // Only the segment just published by this push is new to seen-records.
            assert_eq!(gets, 1, "historical segments must stay cached (recovery={recovery})");
        }
    }

    #[test]
    fn push_rejects_cached_conflicting_history() {
        use crate::model::Record;
        let mut outcomes = Vec::new();
        for recovery in [false, true] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            let descriptor = store.descriptor().unwrap();
            let other = crate::ids::new_id();
            let key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
            let original: Vec<String> = (1..=3)
                .map(|seq| {
                    let mut record = Record::untrusted_preview(if seq == 1 { "machine" } else { "config" });
                    record.id = crate::ids::new_id();
                    record.machine = other.clone();
                    record.project = store.project.clone();
                    record.seq = seq;
                    if seq == 1 {
                        record.data =
                            serde_json::json!({"name":"third", "public_key":hex::encode(key.verifying_key().to_bytes())});
                    }
                    oplog::sign_line(&record, &key).unwrap()
                })
                .collect();
            let publish = |first, last, lines: &[String]| {
                let name = format::segment_name(&other, first, last);
                let bytes = format!("{}\n", lines.join("\n"));
                assert!(store.backend.put_new(&name, keys::encrypt(&descriptor.recipient, bytes.as_bytes()).unwrap()).unwrap());
            };
            publish(1, 2, &original[..2]);
            // Force actual parent hydration, which observes but never imports the other machine.
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(true));
            assert_eq!(push(&store, &ctx, &repo, &Options::default()).unwrap().states, 4);
            assert!(repo.p.db.record_line(&other, 1).unwrap().is_none());
            let mut fork = original;
            let mut record = oplog::parse_line(&fork[1]).unwrap().record;
            record.actor.name = "a different signed record".into();
            fork[1] = oplog::sign_line(&record, &key).unwrap();
            repo.p.lock_maintenance().unwrap().import_lines(&fork, crate::store::Trust::All).unwrap();
            publish(1, 3, &fork);
            let parent = repo.p.db.present_states().unwrap()[0].0.clone();
            add_state(&ctx, &repo, std::slice::from_ref(&parent), &[("payload", b"incremental payload\n")]).unwrap();
            let identity = store.identity(&descriptor, None).unwrap();
            assert!(reader::records(&store.backend, &identity, &store.project)
                .unwrap_err()
                .to_string()
                .contains("conflicting history"));
            if recovery {
                let mut index = load_index(&store.dir).unwrap();
                index.remove(&parent);
                checkpoint(&store.dir, &index).unwrap();
                drop(super::super::push_journal::Journal::open(&store.dir).unwrap());
            }
            let result = push(&store, &ctx, &repo, &Options::default());
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
            let result = result.map(|report| report.states).map_err(|error| error.to_string());
            drop(repo);
            std::fs::remove_dir_all(root).unwrap();
            eprintln!("conflicting history recovery={recovery}: {result:?}");
            outcomes.push((recovery, result));
        }
        for (recovery, result) in outcomes {
            assert!(
                result.as_ref().is_err_and(|error| error.contains("conflicting history")),
                "push must reject conflicting histories (recovery={recovery}), got {result:?}"
            );
        }
    }

    #[test]
    fn crash_content_hashes_are_reused_at_each_publication_boundary() {
        use std::{
            io::Write,
            os::unix::process::ExitStatusExt,
            process::{Command, Stdio},
        };
        for (point, corrupt) in [("before_part", false), ("after_part", false), ("after_publish", false), ("after_part", true)] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            let states =
                parents_first(repo.p.db.present_states().unwrap().into_iter().map(|(id, _)| repo.state(&id).unwrap()).collect())
                    .unwrap();
            let kill_state = states[2].id.clone();
            drop(repo);
            let puts = root.join("puts.jsonl");
            let invoke = |kill: bool, log: &str| {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--ignored", "--exact", "store_remote::writer::tests::crash_push_process", "--nocapture"])
                    .env("LAYR_TEST_CRASH_ROOT", &root)
                    .env("LAYR_TEST_PUT_LOG", &puts)
                    .env("LAYR_TRACE", "1")
                    .env_remove("LAYR_TEST_KILL_POINT")
                    .env_remove("LAYR_TEST_KILL_STATE")
                    .stdout(Stdio::null())
                    .stderr(std::fs::File::create(root.join(log)).unwrap());
                if kill {
                    command.env("LAYR_TEST_KILL_POINT", point).env("LAYR_TEST_KILL_STATE", &kill_state);
                }
                command.status().unwrap()
            };
            assert_eq!(invoke(true, "killed.log").signal(), Some(libc::SIGKILL));
            let read_puts = || -> Vec<serde_json::Value> {
                std::fs::read_to_string(&puts).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
            };
            let before_puts = read_puts();
            let damaged = corrupt.then(|| before_puts.last().unwrap()["object"].as_str().unwrap().to_string());
            if let Some(key) = &damaged {
                let path = root.join("remote").join(key);
                let mut bytes = std::fs::read(&path).unwrap();
                *bytes.last_mut().unwrap() ^= 1;
                std::fs::write(path, bytes).unwrap();
            }
            let existing_hashes: BTreeSet<String> = before_puts
                .iter()
                .filter(|row| damaged.as_deref() != row["object"].as_str())
                .flat_map(|row| row["hashes"].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_string()))
                .collect();
            let before: BTreeMap<String, _> = store
                .backend
                .list("")
                .unwrap()
                .into_iter()
                .filter(|object| damaged.as_deref() != Some(object.key.as_str()))
                .map(|object| {
                    let digest = ContentAddress::digest(store.backend.get(&object.key).unwrap().unwrap());
                    (object.key, digest)
                })
                .collect();
            assert!(invoke(false, "resumed.log").success());
            let after_puts = read_puts();
            for row in &after_puts[before_puts.len()..] {
                for hash in row["hashes"].as_array().unwrap() {
                    assert!(!existing_hashes.contains(hash.as_str().unwrap()), "{point}: duplicate content {hash}");
                }
            }
            for (key, digest) in &before {
                assert_eq!(&ContentAddress::digest(store.backend.get(key).unwrap().unwrap()), digest);
            }
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
            store.verify(true).unwrap();
            let descriptor = store.descriptor().unwrap();
            let identity = store.identity(&descriptor, None).unwrap();
            let records = reader::records(&store.backend, &identity, &descriptor.project).unwrap();
            let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
            let mut referenced = BTreeSet::new();
            for (state, attempt) in reader::attempts(&store.backend).unwrap() {
                for entry in source.tree(&state, &attempt).unwrap().2.files.into_values() {
                    referenced.extend(entry.3.iter().map(|link| link.object()));
                }
            }
            for object in store.backend.list("states/").unwrap() {
                if format::parse_data_name(&object.key).is_some() {
                    if damaged.as_deref() == Some(object.key.as_str()) {
                        assert!(!referenced.contains(&object.key), "corrupt pending part was reused");
                        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90000);
                        std::fs::File::options()
                            .write(true)
                            .open(root.join("remote").join(&object.key))
                            .unwrap()
                            .set_modified(old)
                            .unwrap();
                    } else {
                        assert!(referenced.contains(&object.key), "{point}: orphan {}", object.key);
                    }
                }
            }
            let collected = store.gc(false).unwrap();
            assert_eq!(collected, damaged.into_iter().collect::<Vec<_>>());
            assert!(!store.dir.join("push-journal.sqlite3").exists());
            let log = std::fs::read_to_string(root.join("resumed.log")).unwrap();
            let stats: serde_json::Value =
                serde_json::from_str(log.lines().find_map(|line| line.strip_prefix("trace push-journal ")).unwrap()).unwrap();
            assert!(stats["seed_states"].as_u64().unwrap() <= 1);
            let report = serde_json::json!({"point":point,"corrupt_pending":corrupt,"preserved_objects":before.len(),"duplicate_content_uploads":0,"orphan_parts":0,"gc_parts":collected.len(),"journal":stats});
            println!("crash recovery {report}");
            if let Some(path) = std::env::var_os("LAYR_TEST_CRASH_REPORT") {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
                writeln!(file, "{report}").unwrap();
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// A part of an attempt that has not published yet, 23 and a half hours old: its writer publishes within 23 hours of
    /// it, after the listing of a gc. The gc's day counts from its listing, not from the end of a slow scan of the trees.
    #[test]
    fn a_gc_whose_scan_is_slow_keeps_a_part_younger_than_a_day_at_its_listing() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let part = format::data_name(&crate::ids::new_id(), "0123abcd", 0);
        assert!(store.backend.put_new(&part, b"frames".to_vec())?);
        let now = chrono::Utc::now().timestamp();
        let modified = std::time::SystemTime::now() - std::time::Duration::from_secs(23 * 3600 + 1800);
        std::fs::File::options().write(true).open(root.join("remote").join(&part))?.set_modified(modified)?;
        reader::GC_CLOCK.with(|clock| clock.set(Some(now)));
        // The scan of the trees takes two hours.
        reader::GC_AFTER_SCAN
            .with(|hook| *hook.borrow_mut() = Some(Box::new(move || reader::GC_CLOCK.with(|clock| clock.set(Some(now + 7200))))));
        let collected = store.gc(false);
        reader::GC_AFTER_SCAN.with(|hook| hook.borrow_mut().take());
        reader::GC_CLOCK.with(|clock| clock.set(None));
        assert!(!collected?.contains(&part));
        assert!(store.backend.get(&part)?.is_some());
        finish_fixture(&ctx, repo, &root)
    }

    #[test]
    fn pending_part_age_survives_double_crash_and_gc_before_publication() {
        use std::{
            os::unix::process::ExitStatusExt,
            process::{Command, Stdio},
        };
        for mode in ["expired", "double_crash", "gc_before_publish"] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            let states =
                parents_first(repo.p.db.present_states().unwrap().into_iter().map(|(id, _)| repo.state(&id).unwrap()).collect())
                    .unwrap();
            let target = states[2].id.clone();
            drop(repo);
            let puts = root.join("puts.jsonl");
            let clock = root.join("clock");
            let invoke = |point: Option<&str>, simulated_clock: bool, gc: bool, log: &str| {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--ignored", "--exact", "store_remote::writer::tests::crash_push_process", "--nocapture"])
                    .env("LAYR_TEST_CRASH_ROOT", &root)
                    .env("LAYR_TEST_PUT_LOG", &puts)
                    .env_remove("LAYR_TEST_KILL_POINT")
                    .env_remove("LAYR_TEST_KILL_STATE")
                    .env_remove("LAYR_TEST_PUSH_CLOCK")
                    .env_remove("LAYR_TEST_GC_STATE")
                    .stdout(Stdio::null())
                    .stderr(std::fs::File::create(root.join(log)).unwrap());
                if let Some(point) = point {
                    command.env("LAYR_TEST_KILL_POINT", point).env("LAYR_TEST_KILL_STATE", &target);
                }
                if simulated_clock {
                    command.env("LAYR_TEST_PUSH_CLOCK", &clock);
                }
                if gc {
                    command.env("LAYR_TEST_GC_STATE", &target);
                }
                command.status().unwrap()
            };
            assert_eq!(invoke(Some("after_part"), false, false, "first.log").signal(), Some(libc::SIGKILL));
            let initial: Vec<serde_json::Value> =
                std::fs::read_to_string(&puts).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            let part = initial.last().unwrap()["object"].as_str().unwrap().to_string();
            let sha = initial.last().unwrap()["hashes"][0].as_str().unwrap();
            if mode == "double_crash" {
                assert_eq!(invoke(Some("before_publish"), false, false, "second.log").signal(), Some(libc::SIGKILL));
                let journal = super::super::push_journal::Journal::open(&store.dir).unwrap();
                assert!(!journal.hashes().unwrap().contains_key(sha), "unpublished hash escaped its pending object");
                assert!(journal.pending().unwrap().iter().any(|pending| pending.object == part));
            }
            let age = if mode == "expired" { 13 * 3600 } else { 25 * 3600 };
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
            std::fs::File::options().write(true).open(root.join("remote").join(&part)).unwrap().set_modified(old).unwrap();
            if mode == "gc_before_publish" {
                std::fs::write(&clock, (chrono::Utc::now().timestamp() - 24 * 3600).to_string()).unwrap();
                assert!(!invoke(None, true, true, "gc.log").success());
                assert!(std::fs::read_to_string(root.join("gc.log")).unwrap().contains("safe publication deadline"));
                assert!(store.backend.get(&part).unwrap().is_none());
                assert!(!reader::attempts(&store.backend).unwrap().contains_key(&target));
                store.verify(true).unwrap();
            }
            assert!(invoke(None, false, false, "final.log").success());
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
            store.verify(true).unwrap();
            let descriptor = store.descriptor().unwrap();
            let identity = store.identity(&descriptor, None).unwrap();
            let records = reader::records(&store.backend, &identity, &descriptor.project).unwrap();
            let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
            for (state, attempt) in reader::attempts(&store.backend).unwrap() {
                for entry in source.tree(&state, &attempt).unwrap().2.files.values() {
                    assert!(entry.3.iter().all(|link| link.object() != part), "{mode}: expired part reused");
                }
            }
            if store.backend.get(&part).unwrap().is_some() {
                std::fs::File::options()
                    .write(true)
                    .open(root.join("remote").join(&part))
                    .unwrap()
                    .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600))
                    .unwrap();
                assert_eq!(store.gc(false).unwrap(), vec![part]);
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rolling_back_journal_rows_only_causes_authenticated_reseeding() {
        use std::{
            os::unix::process::ExitStatusExt,
            process::{Command, Stdio},
        };
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let repo = branch_fixture(&ctx, &store);
        let states =
            parents_first(repo.p.db.present_states().unwrap().into_iter().map(|(id, _)| repo.state(&id).unwrap()).collect())
                .unwrap();
        drop(repo);
        let invoke = |point: Option<(&str, &str)>, log: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--ignored", "--exact", "store_remote::writer::tests::crash_push_process", "--nocapture"])
                .env("LAYR_TEST_CRASH_ROOT", &root)
                .env("LAYR_TRACE", "1")
                .env_remove("LAYR_TEST_KILL_POINT")
                .env_remove("LAYR_TEST_KILL_STATE")
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(root.join(log)).unwrap());
            if let Some((point, state)) = point {
                command.env("LAYR_TEST_KILL_POINT", point).env("LAYR_TEST_KILL_STATE", state);
            }
            command.status().unwrap()
        };
        assert_eq!(invoke(Some(("before_part", &states[1].id)), "first.log").signal(), Some(libc::SIGKILL));
        let path = store.dir.join("push-journal.sqlite3");
        let older = root.join("older.sqlite3");
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute("VACUUM INTO ?1", [older.to_str().unwrap()]).unwrap();
        }
        assert_eq!(invoke(Some(("after_publish", &states[2].id)), "second.log").signal(), Some(libc::SIGKILL));
        let before: BTreeMap<_, _> = store
            .backend
            .list("")
            .unwrap()
            .into_iter()
            .map(|object| {
                let digest = ContentAddress::digest(store.backend.get(&object.key).unwrap().unwrap());
                (object.key, digest)
            })
            .collect();
        let parts_before = before.keys().filter(|key| format::parse_data_name(key).is_some()).count();
        // Drop the newer WAL as well: restore a coherent older committed database.
        for suffix in ["-wal", "-shm"] {
            let path = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
        std::fs::copy(older, &path).unwrap();
        assert!(invoke(None, "resumed.log").success());
        for (key, digest) in before {
            assert_eq!(ContentAddress::digest(store.backend.get(&key).unwrap().unwrap()), digest);
        }
        store.verify(true).unwrap();
        assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
        let parts_after =
            store.backend.list("states/").unwrap().iter().filter(|object| format::parse_data_name(&object.key).is_some()).count();
        assert_eq!(parts_before, parts_after);
        let log = std::fs::read_to_string(root.join("resumed.log")).unwrap();
        let stats: serde_json::Value =
            serde_json::from_str(log.lines().find_map(|line| line.strip_prefix("trace push-journal ")).unwrap()).unwrap();
        assert_eq!(stats["seed_states"], 2);
        let report =
            serde_json::json!({"case":"older_journal_copy","extra_uploaded_parts":parts_after-parts_before,"journal":stats});
        if let Some(path) = std::env::var_os("LAYR_TEST_CRASH_REPORT") {
            use std::io::Write;
            writeln!(std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap(), "{report}").unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_push_discards_wal_sidecars_without_their_main_database() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let repo = branch_fixture(&ctx, &store);
        store.private_dir().unwrap();
        let path = store.dir.join("push-journal.sqlite3");
        {
            let mut old = super::super::push_journal::Journal::open(&store.dir).unwrap();
            old.checkpoint("old-state", "deadbeef", 0, &BTreeMap::new(), &[]).unwrap();
            old.prepare_part("old-state", "deadbeef", 0, &[]).unwrap();
            for suffix in ["-wal", "-shm"] {
                std::fs::copy(format!("{}{suffix}", path.display()), root.join(format!("old{suffix}"))).unwrap();
            }
        }
        std::fs::remove_file(&path).unwrap();
        for suffix in ["-wal", "-shm"] {
            std::fs::copy(root.join(format!("old{suffix}")), format!("{}{suffix}", path.display())).unwrap();
        }
        {
            let fresh = super::super::push_journal::Journal::open(&store.dir).unwrap();
            assert!(!fresh.resuming);
            assert!(fresh.completed().unwrap().is_empty());
            assert!(fresh.pending().unwrap().is_empty());
            fresh.finish().unwrap();
        }
        assert_eq!(push(&store, &ctx, &repo, &Options::default()).unwrap().states, 4);
        store.verify(true).unwrap();
        for name in ["push-journal.sqlite3", "push-journal.sqlite3-wal", "push-journal.sqlite3-shm"] {
            assert!(!store.dir.join(name).exists());
        }
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Records a commit state with `files` on `parents` and returns its id.
    fn add_state(ctx: &Ctx, repo: &Repo, parents: &[String], files: &[(&str, &[u8])]) -> Result<String> {
        let id = crate::ids::new_id();
        std::fs::create_dir(repo.p.state_path(&id))?;
        for (path, bytes) in files {
            std::fs::write(repo.p.state_path(&id).join(path), bytes)?;
        }
        let state: crate::model::StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":"commit",
            "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
            "parents":parents,"time":crate::ids::now_ms()}))?;
        let mut record = crate::model::DraftRecord::new("state");
        record.states.push(state);
        repo.p.lock_maintenance()?.append(record, &ctx.actor())?;
        Ok(id)
    }

    fn borrowed(files: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
        files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect()
    }

    /// A configured test remote whose File backend logs its requests from `log_requests` on.
    fn counted_remote(root: &Path) -> Result<(Ctx, Store)> {
        std::fs::create_dir_all(root)?;
        let ctx = context(root);
        let mut store = remote(root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })?;
        Ok((ctx, store))
    }
    fn request_log() -> std::sync::MutexGuard<'static, BTreeMap<std::path::PathBuf, Vec<(&'static str, String)>>> {
        super::super::backend::FILE_REQUESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn log_requests(root: &Path) {
        request_log().insert(root.join("remote"), Vec::new());
    }
    /// The logged requests `(operation, key)` of `operation` (all with "").
    fn requests(root: &Path, operation: &str) -> Vec<String> {
        let log = request_log().remove(&root.join("remote")).unwrap_or_default();
        log.into_iter().filter(|(op, _)| operation.is_empty() || *op == operation).map(|(_, key)| key).collect()
    }
    fn finish_fixture(ctx: &Ctx, repo: Repo, root: &Path) -> Result<()> {
        crate::free::finish(ctx);
        drop(repo);
        Ok(std::fs::remove_dir_all(root)?)
    }

    #[test]
    fn many_tips_share_one_state_listing_and_a_base_is_probed_once() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let mut tips = Vec::new();
        for n in 0..40 {
            let id = add_state(&ctx, &repo, &[], &[("tip", format!("tip {n}\n").as_bytes())])?;
            let name = format!("pins/tip-{n}");
            let mut keep = crate::model::DraftRecord::new("ref.set");
            keep.data = serde_json::json!({"ref_change":{"name":name,"before":null,"after":{"name":name,"target":id}}});
            repo.p.lock_maintenance()?.append(keep, &ctx.actor())?;
            tips.push(id);
        }
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 44);
        add_state(&ctx, &repo, &[tips[0].clone()], &[("tip", b"changed\n")])?;
        log_requests(&root);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        let lists: Vec<String> = requests(&root, "list").into_iter().filter(|key| key.starts_with("states/")).collect();
        eprintln!("40 tips, one new state: {} state listings", lists.len());
        // Before: one listing per tip, then the new state and its base again (42). The base is probed at its
        // selection and again right before the tree publishes.
        let base = format!("states/{}/", tips[0]);
        assert_eq!(lists, ["states/".to_string(), base.clone(), base]);
        store.verify(true)?;
        finish_fixture(&ctx, repo, &root)
    }

    #[test]
    fn bases_on_one_parent_chain_read_each_remote_tree_once() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let mut chain: Vec<String> = Vec::new();
        for n in 0..6 {
            let payload = format!("chain {n}\n");
            chain.push(add_state(&ctx, &repo, chain.last().cloned().as_slice(), &[("payload", payload.as_bytes())])?);
        }
        // Without tree caches every base is hydrated from its remote delta chain.
        PUSH_CACHE_DISABLED.with(|disabled| disabled.set(true));
        let first = push(&store, &ctx, &repo, &Options::default()).map(|report| report.states);
        for (n, base) in chain.iter().enumerate() {
            add_state(&ctx, &repo, std::slice::from_ref(base), &[("payload", format!("sibling {n}\n").as_bytes())])?;
        }
        log_requests(&root);
        let second = push(&store, &ctx, &repo, &Options::default()).map(|report| report.states);
        PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
        assert_eq!((first?, second?), (10, 6));
        let gets = requests(&root, "get");
        let trees: Vec<&String> = gets.iter().filter(|key| key.ends_with(".tree.age")).collect();
        let slots = gets.iter().filter(|key| key.starts_with("slots/")).count();
        eprintln!("six bases on one chain: {} tree GETs, {slots} slot GETs", trees.len());
        // Before: a reader per base read 1 + 2 + ... + 6 = 21 trees and opened the slot once per base (7).
        assert_eq!(trees.len(), chain.len());
        assert_eq!(trees.iter().collect::<BTreeSet<_>>().len(), chain.len());
        assert_eq!(slots, 2, "prepare_push and the one hydration reader");
        store.verify(true)?;
        finish_fixture(&ctx, repo, &root)
    }

    #[test]
    fn cold_bases_load_in_batches_through_the_gateway() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let mut chain: Vec<String> = Vec::new();
        for n in 0..6 {
            let payload = format!("chain {n}\n");
            chain.push(add_state(&ctx, &repo, chain.last().cloned().as_slice(), &[("payload", payload.as_bytes())])?);
        }
        // Without tree caches every base is cold: its delta chain is on the remote only.
        PUSH_CACHE_DISABLED.with(|disabled| disabled.set(true));
        let result = (|| -> Result<Vec<(String, serde_json::Value)>> {
            assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 10);
            for (n, base) in chain.iter().enumerate() {
                add_state(&ctx, &repo, std::slice::from_ref(base), &[("payload", format!("sibling {n}\n").as_bytes())])?;
            }
            let (backend, peer) = super::super::gateway::tests::file_transport(store.backend.clone());
            let files = std::mem::replace(&mut store.backend, backend);
            let pushed = push(&store, &ctx, &repo, &Options::default());
            store.backend = files;
            assert_eq!(pushed?.states, 6);
            Ok(peer.finish())
        })();
        PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
        let log = result?;
        let trees = |value: &serde_json::Value| {
            value["keys"]
                .as_array()
                .map_or(0, |keys| keys.iter().filter(|k| k.as_str().is_some_and(|k| k.ends_with(".tree.age"))).count())
        };
        let batches: Vec<usize> =
            log.iter().filter(|(r, _)| r.ends_with("/batch")).map(|(_, v)| trees(v)).filter(|n| *n > 0).collect();
        let singles = log.iter().filter(|(r, _)| r.starts_with("get ") && r.ends_with(".tree.age")).count();
        let probes = log.iter().filter(|(r, _)| r.starts_with("get ") && r.contains("prefix=states%2f")).count();
        eprintln!("six cold bases on one chain: tree batches {batches:?}, single tree GETs {singles}, state probes {probes}");
        // Before: one GET per tree (six); the chain's bases are all requested in one round.
        assert_eq!((batches, singles), (vec![chain.len()], 0));
        assert_eq!(probes, 2 * chain.len() + 1, "one discovery listing, and a probe per base at selection and publication");
        store.verify(true)?;
        finish_fixture(&ctx, repo, &root)
    }

    #[test]
    fn a_one_file_change_sends_only_that_file() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let mut files: Vec<(String, Vec<u8>)> =
            (0..20).map(|n| (format!("f{n}"), format!("file {n}: {}\n", "x".repeat(n * 100)).into_bytes())).collect();
        let parent = add_state(&ctx, &repo, &[], &borrowed(&files))?;
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 5);
        files[7].1 = b"changed content\n".to_vec();
        let child = add_state(&ctx, &repo, &[parent], &borrowed(&files))?;
        log_requests(&root);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        let puts = requests(&root, "put");
        eprintln!("one changed file of 20: PUTs {puts:?}");
        assert_eq!(puts.len(), 3, "{puts:?}");
        assert!(puts[0].starts_with("records/"));
        assert!(puts[1].starts_with(&format!("states/{child}/")) && puts[1].ends_with(".0000.data.age"));
        assert!(puts[2].starts_with(&format!("states/{child}/")) && puts[2].ends_with(".tree.age"));
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        let attempt = reader::attempts(&store.backend)?.remove(&child).context("child tree")?;
        for (path, entry) in &source.tree(&child, &attempt)?.2.files {
            assert_eq!(entry.3[0].0 == child, path == "f7", "{path} links {:?}", entry.3);
        }
        finish_fixture(&ctx, repo, &root)
    }

    #[test]
    fn every_part_is_acknowledged_before_its_tree_and_a_failed_part_publishes_nothing() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        // Three incompressible 9 MiB files make three parts of at most 16 MiB.
        let big = || -> Vec<(String, Vec<u8>)> {
            (0..3)
                .map(|n| {
                    let mut bytes = vec![0; 9 << 20];
                    rand::rngs::OsRng.fill_bytes(&mut bytes);
                    (format!("big{n}"), bytes)
                })
                .collect()
        };
        let id = add_state(&ctx, &repo, &[], &borrowed(&big()))?;
        log_requests(&root);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        let prefix = format!("states/{id}/");
        let puts: Vec<String> = requests(&root, "put").into_iter().filter(|key| key.starts_with(&prefix)).collect();
        eprintln!("three-part state: PUT completion order {puts:?}");
        assert_eq!(puts.len(), 4, "{puts:?}");
        assert!(puts[..3].iter().all(|key| key.ends_with(".data.age")) && puts[3].ends_with(".tree.age"));
        // A create-only conflict on the second part fails the state; its tree must not go up.
        let id = add_state(&ctx, &repo, &[], &borrowed(&big()))?;
        let attempt = hex::encode(ContentAddress::digest(id.as_bytes()))[..8].to_string();
        assert!(store.backend.put_new(&format::data_name(&id, &attempt, 1), b"taken".to_vec())?);
        PUSH_DETERMINISTIC.with(|enabled| enabled.set(true));
        let failed = push(&store, &ctx, &repo, &Options::default()).map(|report| report.states);
        PUSH_DETERMINISTIC.with(|enabled| enabled.set(false));
        assert!(failed.is_err_and(|error| error.to_string().contains("existing part")));
        assert!(!reader::attempts(&store.backend)?.contains_key(&id));
        // The retry reuses the parts that did go up and publishes the state.
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        store.verify(true)?;
        finish_fixture(&ctx, repo, &root)
    }

    #[test]
    fn a_small_push_against_a_long_history_probes_only_its_prefixes() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let backend = super::super::backend::Backend::file(root.join("remote"));
        let states: Vec<String> = (0..3).map(|_| crate::ids::new_id()).collect();
        let tree = |id: &str| format::tree_name(id, "0123abcd");
        assert!(backend.put_new(&tree(&states[0]), b"tree".to_vec())?);
        log_requests(&root);
        // Three probes cost less than the listing pages of 16,384 known states.
        let mut long = Attempts::new(&backend, INDEX_STATES);
        long.discover(states.clone(), false)?;
        assert_eq!(long.complete(&states[0])?.as_deref(), Some("0123abcd"));
        assert_eq!(long.complete(&states[1])?, None);
        let probes: Vec<String> = states.iter().map(|id| format!("states/{id}/")).collect();
        let discovered: BTreeSet<String> = requests(&root, "list").into_iter().collect();
        assert_eq!(discovered, probes.iter().cloned().collect());
        // A base decision always asks again, even right after an exact probe.
        log_requests(&root);
        assert!(long.has_tree(&states[0], "0123abcd", true)?);
        assert_eq!(requests(&root, ""), [probes[0].clone()]);
        // A bulk listing replaces an earlier exact probe, and a base decision still asks again.
        log_requests(&root);
        let mut short = Attempts::new(&backend, 10);
        assert_eq!(short.complete(&states[2])?, None);
        assert!(backend.put_new(&tree(&states[2]), b"tree".to_vec())?);
        short.discover(states.clone(), false)?;
        assert_eq!(short.complete(&states[2])?.as_deref(), Some("0123abcd"));
        std::fs::remove_file(root.join("remote").join(tree(&states[0])))?;
        assert!(short.complete(&states[0])?.is_some(), "discovery keeps what the listing saw");
        assert!(!short.has_tree(&states[0], "0123abcd", true)?, "a base decision sees the deletion");
        assert_eq!(requests(&root, "list"), [probes[2].clone(), "states/".to_string(), probes[0].clone()]);
        Ok(std::fs::remove_dir_all(root)?)
    }

    /// Two URLs of one store keep their own local state, and share the lock of its pushes and sweeps (and of those of its
    /// layers).
    #[test]
    fn two_urls_of_one_store_share_its_push_lock() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let archive = root.join("archive");
        std::fs::create_dir_all(&archive)?;
        let aliases = |urls: &[String]| -> Result<()> {
            let stores: Vec<Store> =
                urls.iter().map(|url| super::super::open_with_pin(url, &repo, None)).collect::<Result<_>>()?;
            let dirs: BTreeSet<&Path> = stores.iter().map(|store| store.dir.as_path()).collect();
            assert_eq!(dirs.len(), urls.len(), "{urls:?}");
            let locks: BTreeSet<std::path::PathBuf> = stores.iter().map(Store::lock_dir).collect();
            assert_eq!(locks.len(), 1, "{urls:?}");
            let layer =
                |store: &Store| Store { url: store.url.clone(), dir: store.dir.join("overlays/o/e"), ..remote(&root, &ctx) };
            let layers: BTreeSet<std::path::PathBuf> = stores.iter().map(|store| layer(store).lock_dir()).collect();
            assert_eq!(layers.len(), 1, "{urls:?}");
            assert!(layers.iter().all(|dir| dir.ends_with("overlays/o/e") && !locks.contains(dir)));
            Ok(())
        };
        let path = archive.display();
        aliases(&[format!("file://{path}"), format!("file://{path}/"), format!("file://{path}/../archive/.")])?;
        aliases(&["layr+https://Cloud.Example:443/p".into(), "layr+https://cloud.example/p".into()])?;
        aliases(&["s3://bucket/archive".into(), "s3://bucket/archive/".into()])?;
        finish_fixture(&ctx, repo, &root)
    }

    /// The header of the published tree of `state`, read back from the remote with all its files by a fresh reader.
    fn read_back(store: &Store, state: &str) -> Result<Header> {
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        let attempt = reader::attempts(&store.backend)?.remove(state).context("state tree")?;
        let (header, _, tree) = source.tree(state, &attempt)?;
        for entry in tree.files.values() {
            source.read(entry)?;
        }
        Ok(header)
    }

    /// Every published tree on the remote, each attempt of a state on its own.
    fn tree_count(store: &Store) -> Result<usize> {
        Ok(store.backend.list("states/")?.iter().filter(|object| format::parse_tree_name(&object.key).is_some()).count())
    }

    /// Pushes, deleting the remote tree of `base` and its parts, as a sweep does, at `hook` of `child` (just before the
    /// push selects the base, or after the child's parts are up); returns the header of the child's published tree,
    /// read back from the remote with all its files.
    fn push_with_base_deleted(
        root: &Path,
        ctx: &Ctx,
        store: &Store,
        repo: &Repo,
        base: &str,
        child: &str,
        hook: &'static std::thread::LocalKey<std::cell::RefCell<Option<BaseHook>>>,
    ) -> Result<Header> {
        let remote = root.join("remote").join("states").join(base);
        let (base, deleted_for) = (base.to_string(), child.to_string());
        hook.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |state, selected| {
                if state == deleted_for && selected == base && remote.exists() {
                    std::fs::remove_dir_all(&remote).unwrap_or_else(|error| eprintln!("{error}"));
                }
            }))
        });
        let pushed = push(store, ctx, repo, &Options::default());
        hook.with(|hook| hook.borrow_mut().take());
        pushed?;
        read_back(store, child)
    }

    /// With siblings, the first sibling seeds the frames of the base before the base goes: the child, a complete tree
    /// now, must not link them.
    #[test]
    fn a_base_deleted_before_its_selection_is_not_used_for_a_delta() -> Result<()> {
        for siblings in [false, true] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            let (ctx, store) = counted_remote(&root)?;
            let repo = branch_fixture(&ctx, &store);
            let base = add_state(&ctx, &repo, &[], &[("payload", b"base\n"), ("same", b"unchanged\n")])?;
            // A tip: discovery probes it exactly, and the local tree cache holds its tree. With siblings, there is no
            // tree cache: the first sibling hydrates the base into the push's reader before the second selects it.
            let mut keep = crate::model::DraftRecord::new("ref.set");
            keep.data =
                serde_json::json!({"ref_change":{"name":"pins/base","before":null,"after":{"name":"pins/base","target":base}}});
            repo.p.lock_maintenance()?.append(keep, &ctx.actor())?;
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(siblings));
            let result = (|| -> Result<Header> {
                assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 5);
                if siblings {
                    add_state(&ctx, &repo, std::slice::from_ref(&base), &[("payload", b"first\n")])?;
                }
                let files: &[(&str, &[u8])] = &[("payload", b"child\n"), ("same", b"unchanged\n")];
                let child = add_state(&ctx, &repo, std::slice::from_ref(&base), files)?;
                push_with_base_deleted(&root, &ctx, &store, &repo, &base, &child, &PUSH_BEFORE_BASE)
            })();
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
            let header = result?;
            assert_eq!((header.tree.as_str(), header.base), ("full", None), "siblings={siblings}");
            finish_fixture(&ctx, repo, &root)?;
        }
        Ok(())
    }

    /// A sweep elsewhere can delete the base while the parts of a slow upload go up: the tree on it is not published,
    /// and the state goes up as a complete tree that links no frame of the base.
    #[test]
    fn a_base_deleted_during_the_upload_is_not_used_for_a_delta() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let base = add_state(&ctx, &repo, &[], &[("payload", b"base\n"), ("same", b"unchanged\n")])?;
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 5);
        let trees = tree_count(&store)?;
        let files: &[(&str, &[u8])] = &[("payload", b"child\n"), ("same", b"unchanged\n")];
        let child = add_state(&ctx, &repo, std::slice::from_ref(&base), files)?;
        let header = push_with_base_deleted(&root, &ctx, &store, &repo, &base, &child, &PUSH_BEFORE_TREE)?;
        assert_eq!((header.tree.as_str(), header.base), ("full", None));
        assert_eq!(tree_count(&store)?, trees, "the base went, and the attempt on it never published: one tree of the child");
        finish_fixture(&ctx, repo, &root)
    }

    /// The chains of a base that went during an upload leave the journal too: a push that resumes after a later stop
    /// does not link their frames again.
    #[test]
    fn a_resumed_push_does_not_reuse_the_frames_of_a_base_that_went() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let base = add_state(&ctx, &repo, &[], &[("payload", b"base\n"), ("same", b"unchanged\n")])?;
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 5);
        // The sibling's checkpoint writes the chains of the base, which its selection seeded, to the journal.
        add_state(&ctx, &repo, std::slice::from_ref(&base), &[("payload", b"first\n")])?;
        let files: &[(&str, &[u8])] = &[("payload", b"child\n"), ("same", b"unchanged\n")];
        let child = add_state(&ctx, &repo, std::slice::from_ref(&base), files)?;
        let orphan = add_state(&ctx, &repo, &[], &[("payload", b"orphan\n"), ("same", b"unchanged\n")])?;
        PUSH_STOP_AFTER.with(|stop| stop.set(Some(2)));
        let stopped = push_with_base_deleted(&root, &ctx, &store, &repo, &base, &child, &PUSH_BEFORE_TREE);
        PUSH_STOP_AFTER.with(|stop| stop.set(None));
        assert!(stopped.is_err(), "the push stops after the child");
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        assert_eq!(read_back(&store, &orphan)?.tree, "full");
        finish_fixture(&ctx, repo, &root)
    }

    /// A push stopped after a tip completed, and the remote lost the tip (a partial replica). The resumed push sends
    /// the tip again; the chains of its lost attempt, which the journal holds, must not be linked again.
    #[test]
    fn a_resumed_push_does_not_link_the_parts_of_a_completed_tip_that_went() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let tip = add_state(&ctx, &repo, &[], &[("payload", b"tip\n")])?;
        let mut keep = crate::model::DraftRecord::new("ref.set");
        keep.data = serde_json::json!({"ref_change":{"name":"pins/tip","before":null,"after":{"name":"pins/tip","target":tip}}});
        repo.p.lock_maintenance()?.append(keep, &ctx.actor())?;
        // The part of the next state commits the journal row of the tip.
        add_state(&ctx, &repo, &[], &[("payload", b"next\n")])?;
        PUSH_STOP_AFTER.with(|stop| stop.set(Some(2)));
        let stopped = push(&store, &ctx, &repo, &Options::default());
        PUSH_STOP_AFTER.with(|stop| stop.set(None));
        assert!(stopped.is_err(), "the push stops after the next state");
        std::fs::remove_dir_all(root.join("remote").join("states").join(&tip))?;
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        read_back(&store, &tip)?;
        finish_fixture(&ctx, repo, &root)
    }

    /// The remote lost a completed tip and the tree at the root of its delta chain, and kept the tree between them (a
    /// partial replica). The tip sent again on that tree, which no longer decodes, is a complete tree.
    #[test]
    fn a_tip_sent_again_to_a_partial_replica_is_a_complete_tree() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        let first = add_state(&ctx, &repo, &[], &[("payload", b"first\n")])?;
        let middle = add_state(&ctx, &repo, std::slice::from_ref(&first), &[("payload", b"middle\n")])?;
        let tip = add_state(&ctx, &repo, std::slice::from_ref(&middle), &[("payload", b"tip\n")])?;
        let mut keep = crate::model::DraftRecord::new("ref.set");
        keep.data = serde_json::json!({"ref_change":{"name":"pins/tip","before":null,"after":{"name":"pins/tip","target":tip}}});
        repo.p.lock_maintenance()?.append(keep, &ctx.actor())?;
        push(&store, &ctx, &repo, &Options::default())?;
        assert_eq!(read_back(&store, &tip)?.base.map(|base| base.0), Some(middle.clone()));
        for state in [&first, &tip] {
            for object in store.backend.list(&format!("states/{state}/"))? {
                if format::parse_tree_name(&object.key).is_some() {
                    store.backend.delete(&object.key)?;
                }
            }
        }
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        assert_eq!(read_back(&store, &tip)?.tree, "full");
        finish_fixture(&ctx, repo, &root)
    }

    /// A push stopped after a tip completed, and the remote lost the tip and an unrelated earlier state whose part the
    /// tip reused (a partial replica). The journal still holds the chain of that part: the tip sent again checks the
    /// parts that it reuses, and uploads the file again.
    #[test]
    fn a_resumed_push_does_not_link_a_lost_part_of_another_state_into_a_lost_tip() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let other = add_state(&ctx, &repo, &[], &[("shared", b"shared content\n"), ("payload", b"other\n")])?;
        let tip = add_state(&ctx, &repo, &[], &[("shared", b"shared content\n"), ("payload", b"tip\n")])?;
        let mut keep = crate::model::DraftRecord::new("ref.set");
        keep.data = serde_json::json!({"ref_change":{"name":"pins/tip","before":null,"after":{"name":"pins/tip","target":tip}}});
        repo.p.lock_maintenance()?.append(keep, &ctx.actor())?;
        // The part of the next state commits the journal row of the tip.
        add_state(&ctx, &repo, &[], &[("payload", b"next\n")])?;
        PUSH_STOP_AFTER.with(|stop| stop.set(Some(3)));
        let stopped = push(&store, &ctx, &repo, &Options::default());
        PUSH_STOP_AFTER.with(|stop| stop.set(None));
        assert!(stopped.is_err(), "the push stops after the next state");
        for state in [&other, &tip] {
            std::fs::remove_dir_all(root.join("remote").join("states").join(state))?;
        }
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 1);
        read_back(&store, &tip)?;
        finish_fixture(&ctx, repo, &root)
    }

    /// In a layer, a sweep deletes the parts that only the trees it deletes link. A chain that this push seeded from a
    /// base can link such a part of an older tree: the tree that reuses it does not publish, and the file goes up again.
    #[test]
    fn a_reused_chain_whose_part_a_sweep_deleted_goes_up_again() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        // The records, so the layer trees below verify.
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        // A layer of frames: this machine reads it, and it has no chunk key.
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        store.layer = Some(super::super::overlays::LayerAccess {
            recipient: descriptor.recipient.clone(),
            identity: Some(identity),
            chunk_key: None,
        });
        let old = add_state(&ctx, &repo, &[], &[("same", b"unchanged\n")])?;
        let base = add_state(&ctx, &repo, std::slice::from_ref(&old), &[("payload", b"base\n"), ("same", b"unchanged\n")])?;
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 2);
        // A sweep deleted the tree of `old` and, with the base later, the part of `old` that only the base linked.
        std::fs::remove_dir_all(root.join("remote").join("states").join(&old))?;
        add_state(&ctx, &repo, std::slice::from_ref(&base), &[("payload", b"first\n")])?;
        let files: &[(&str, &[u8])] = &[("payload", b"child\n"), ("same", b"unchanged\n")];
        let child = add_state(&ctx, &repo, std::slice::from_ref(&base), files)?;
        let header = push_with_base_deleted(&root, &ctx, &store, &repo, &base, &child, &PUSH_BEFORE_BASE)?;
        assert_eq!((header.tree.as_str(), header.base), ("full", None));
        finish_fixture(&ctx, repo, &root)
    }

    /// A sweep deleted a base and, with a dead tree, a part that an earlier tree of the same push reused. The next tree
    /// on that base, a complete tree now, asks for that part again rather than linking it.
    #[test]
    fn a_base_gone_at_its_probe_asks_again_for_the_parts_of_earlier_trees() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: Some(identity), chunk_key: None });
        let old = add_state(&ctx, &repo, &[], &[("shared", b"shared content\n")])?;
        let base = add_state(&ctx, &repo, &[], &[("payload", b"base\n")])?;
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 2);
        add_state(&ctx, &repo, std::slice::from_ref(&old), &[("shared", b"shared content\n"), ("payload", b"first\n")])?;
        let files: &[(&str, &[u8])] = &[("shared", b"shared content\n"), ("payload", b"child\n")];
        let child = add_state(&ctx, &repo, std::slice::from_ref(&base), files)?;
        let states = root.join("remote").join("states");
        let (gone, selected) = ([states.join(&base), states.join(&old)], (child.clone(), base.clone()));
        PUSH_BEFORE_BASE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |state, id| {
                if (state, id) == (selected.0.as_str(), selected.1.as_str()) {
                    gone.iter().for_each(|dir| std::fs::remove_dir_all(dir).unwrap());
                }
            }))
        });
        let pushed = push(&store, &ctx, &repo, &Options::default());
        PUSH_BEFORE_BASE.with(|hook| hook.borrow_mut().take());
        assert_eq!(pushed?.states, 2);
        assert_eq!(read_back(&store, &child)?.tree, "full");
        finish_fixture(&ctx, repo, &root)
    }

    /// A machine that writes a layer through a gateway but cannot read it lists none of the layer's parts: each tree
    /// links only the parts of its own push, and is complete or a delta on a tree of its push, so the second state of a
    /// push publishes without a listing too.
    #[test]
    fn a_write_only_layer_push_sends_self_contained_trees() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let first = add_state(&ctx, &repo, &[], &[("payload", b"first\n"), ("same", b"unchanged\n")])?;
        let second = add_state(&ctx, &repo, std::slice::from_ref(&first), &[("payload", b"second\n"), ("same", b"unchanged\n")])?;
        let (backend, peer) = super::super::gateway::tests::write_only_transport(store.backend.clone());
        let files = std::mem::replace(&mut store.backend, backend.scoped("overlays/layer")?);
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: None, chunk_key: None });
        let pushed = push(&store, &ctx, &repo, &Options::default());
        store.backend = files.scoped("overlays/layer")?;
        peer.finish();
        assert_eq!(pushed?.states, 2);
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        let attempts = reader::attempts(&store.backend)?;
        for (state, base) in [(&first, None), (&second, Some((first.clone(), attempts[&first].clone())))] {
            let (header, _, tree) = source.tree(state, &attempts[state])?;
            assert_eq!((header.tree.as_str(), header.base), (if base.is_some() { "delta" } else { "full" }, base));
            for entry in tree.files.values() {
                assert!(entry.3.iter().all(|link| link.0 == *state || link.0 == first), "{state} links {:?}", entry.3);
                source.read(entry)?;
            }
        }
        finish_fixture(&ctx, repo, &root)
    }

    /// A write-only layer push of small edits to a large file set sends each large file once: each later tree reuses the
    /// parts that an earlier tree of the same push published, and a file edited in a state goes up as a delta on the
    /// state published before it, as does its tree.
    #[test]
    fn a_write_only_layer_push_of_small_edits_sends_each_file_once() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let (mut large, mut same) = (vec![0; 1 << 20], vec![0; 1 << 20]);
        rand::rngs::OsRng.fill_bytes(&mut large);
        rand::rngs::OsRng.fill_bytes(&mut same);
        let mut versions = Vec::new();
        const STATES: usize = 4;
        let mut parent = Vec::new();
        let mut states = Vec::new();
        for n in 0..STATES {
            let edit = format!("edit {n}\n");
            large[n * 1000] ^= 1;
            versions.push(large.clone());
            let files: &[(&str, &[u8])] = &[("large", &large), ("same", &same), ("small", edit.as_bytes())];
            parent = vec![add_state(&ctx, &repo, &parent, files)?];
            states.push(parent[0].clone());
        }
        let (backend, peer) = super::super::gateway::tests::write_only_transport(store.backend.clone());
        let files = std::mem::replace(&mut store.backend, backend.scoped("overlays/layer")?);
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: None, chunk_key: None });
        let pushed = push(&store, &ctx, &repo, &Options::default());
        store.backend = files.scoped("overlays/layer")?;
        peer.finish();
        let pushed = pushed?;
        assert_eq!(pushed.states, STATES);
        assert!(pushed.bytes < 3 * large.len() as u64, "{} bytes for {STATES} small edits", pushed.bytes);
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        let attempts = reader::attempts(&store.backend)?;
        for (n, (state, large)) in states.iter().zip(&versions).enumerate() {
            let (header, _, tree) = source.tree(state, &attempts[state])?;
            let base = n.checked_sub(1).map(|previous| (states[previous].clone(), attempts[&states[previous]].clone()));
            assert_eq!((header.tree.as_str(), header.base), (if n == 0 { "full" } else { "delta" }, base));
            assert_eq!(&source.read(&tree.files["large"])?, large);
            assert_eq!(source.read(&tree.files["same"])?, same);
        }
        finish_fixture(&ctx, repo, &root)
    }

    /// A write-only layer push of states that each edit one file of many: each tree after the first is a delta on the
    /// tree published just before it, and a reader of only what the push published restores every state.
    #[test]
    fn a_write_only_layer_push_of_many_files_sends_delta_trees() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        const FILES: usize = 2000;
        const STATES: usize = 6;
        let mut files: Vec<(String, Vec<u8>)> =
            (0..FILES).map(|n| (format!("f{n:05}"), format!("file {n}\n").into_bytes())).collect();
        let (mut parent, mut states) = (Vec::new(), Vec::new());
        for n in 0..STATES {
            files[n].1 = format!("edit {n}\n").into_bytes();
            parent = vec![add_state(&ctx, &repo, &parent, &borrowed(&files))?];
            states.push(parent[0].clone());
        }
        let (backend, peer) = super::super::gateway::tests::write_only_transport(store.backend.clone());
        let source = std::mem::replace(&mut store.backend, backend.scoped("overlays/layer")?);
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: None, chunk_key: None });
        let pushed = push(&store, &ctx, &repo, &Options::default());
        store.backend = source.scoped("overlays/layer")?;
        peer.finish();
        assert_eq!(pushed?.states, STATES);
        let attempts = reader::attempts(&store.backend)?;
        let size = |state: &String| -> Result<u64> {
            let name = format::tree_name(state, &attempts[state]);
            store.backend.list(&name)?.into_iter().find(|o| o.key == name).and_then(|o| o.size).context("tree size")
        };
        let sizes = states.iter().map(size).collect::<Result<Vec<_>>>()?;
        assert!(sizes[1..].iter().sum::<u64>() < sizes[0], "tree bytes {sizes:?}");
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        for (n, state) in states.iter().enumerate() {
            let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
            let (_, _, tree) = source.tree(state, &attempts[state])?;
            assert_eq!(tree.files.len(), FILES);
            for (k, (path, entry)) in tree.files.iter().enumerate() {
                let expected = if k <= n { format!("edit {k}\n") } else { format!("file {k}\n") };
                assert_eq!(source.read(entry)?, expected.into_bytes(), "{path} of state {n}");
            }
        }
        finish_fixture(&ctx, repo, &root)
    }

    /// A write-only layer push that stopped after a part went up leaves its pending row in the journal. The writer cannot
    /// read that part to check it: the next push discards the row and sends a fresh attempt.
    #[test]
    fn a_write_only_layer_push_retries_after_an_interrupted_one() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let state = add_state(&ctx, &repo, &[], &[("payload", b"interrupted\n")])?;
        let link = Link(state.clone(), "0123abcd".into(), 0, 0, 20, "full".into());
        let entry = Entry("100644".into(), 12, hex::encode(ContentAddress::digest(b"interrupted\n")), vec![link]);
        super::super::push_journal::Journal::open(&store.dir)?.prepare_part(&state, "0123abcd", 0, &[entry])?;
        let (backend, peer) = super::super::gateway::tests::write_only_transport(store.backend.clone());
        let files = std::mem::replace(&mut store.backend, backend.scoped("overlays/layer")?);
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: None, chunk_key: None });
        let pushed = push(&store, &ctx, &repo, &Options::default());
        store.backend = files.scoped("overlays/layer")?;
        peer.finish();
        assert_eq!(pushed?.states, 1);
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        let (_, _, tree) = source.tree(&state, &reader::attempts(&store.backend)?[&state])?;
        assert_eq!(source.read(&tree.files["payload"])?, b"interrupted\n");
        finish_fixture(&ctx, repo, &root)
    }

    /// A write-only layer writer that recovered the layer key locally still cannot list the layer: its files go up as
    /// frames, not as chunks, whose writer lists the packs.
    #[test]
    fn a_write_only_layer_push_with_a_local_layer_key_sends_frames() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let state = add_state(&ctx, &repo, &[], &[("payload", b"framed\n")])?;
        let (backend, peer) = super::super::gateway::tests::write_only_transport(store.backend.clone());
        let files = std::mem::replace(&mut store.backend, backend.scoped("overlays/layer")?);
        let (recipient, chunk_key) = (descriptor.recipient.clone(), Some(layr_store::chunks::key(&identity, "epoch")));
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: Some(identity.clone()), chunk_key });
        let pushed = push(&store, &ctx, &repo, &Options::default());
        store.backend = files.scoped("overlays/layer")?;
        peer.finish();
        assert_eq!(pushed?.states, 1);
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        let (_, _, tree) = source.tree(&state, &reader::attempts(&store.backend)?[&state])?;
        assert!(tree.chunks.is_empty());
        assert_eq!(source.read(&tree.files["payload"])?, b"framed\n");
        finish_fixture(&ctx, repo, &root)
    }

    /// A long layer push of one line: each tree links the parts of every earlier state. The check before each tree asks
    /// only for the parts that the tree uploaded just before does not link: no listing per tree, none of `states/`.
    #[test]
    fn a_long_layer_push_checks_linked_parts_without_a_listing_per_tree() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: Some(identity), chunk_key: None });
        const STATES: usize = 12;
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut parent: Vec<String> = Vec::new();
        for n in 0..STATES {
            files.push((format!("f{n}"), format!("file {n}\n").into_bytes()));
            parent = vec![add_state(&ctx, &repo, &parent, &borrowed(&files))?];
        }
        log_requests(&root);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, STATES);
        let lists = requests(&root, "list");
        eprintln!("{STATES} states of one line in a layer: listings {lists:?}");
        assert!(lists.iter().filter(|prefix| *prefix == "states/").count() <= 1, "{lists:?}");
        assert!(lists.len() < STATES, "{lists:?}");
        finish_fixture(&ctx, repo, &root)
    }

    /// A layer push of two interleaved lines: the base of each tree is not the tree uploaded just before, but every
    /// part that it links went up or was checked earlier in this push. No tree asks for them again.
    #[test]
    fn a_layer_push_of_interleaved_lines_checks_no_part_twice() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: Some(identity), chunk_key: None });
        const STATES: usize = 12;
        let mut lines: [(Vec<(String, Vec<u8>)>, Vec<String>); 2] = Default::default();
        for n in 0..STATES {
            let (files, parent) = &mut lines[n % 2];
            files.push((format!("f{n}"), format!("file {n}\n").into_bytes()));
            *parent = vec![add_state(&ctx, &repo, parent, &borrowed(files))?];
        }
        log_requests(&root);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, STATES);
        let lists = requests(&root, "list");
        eprintln!("{STATES} states of two interleaved lines in a layer: listings {lists:?}");
        assert!(lists.iter().filter(|prefix| *prefix == "states/").count() <= 1, "{lists:?}");
        // A probe of each base at its selection and before its tree publishes.
        assert!(lists.len() <= 2 * STATES, "{lists:?}");
        store.verify(true)?;
        finish_fixture(&ctx, repo, &root)
    }

    /// A bulk listing that returned more objects than the index estimates is not chosen again over fewer probes.
    #[test]
    fn a_bulk_listing_is_not_repeated_when_its_pages_cost_more_than_the_probes() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let backend = super::super::backend::Backend::file(root.join("remote"));
        let states: Vec<String> = (0..3).map(|_| crate::ids::new_id()).collect();
        let held = root.join("remote").join("states").join(&states[0]);
        std::fs::create_dir_all(&held)?;
        for n in 0..=LISTING_PAGE {
            std::fs::write(root.join("remote").join(format::tree_name(&states[0], &format!("{n:08x}"))), b"tree")?;
        }
        let mut attempts = Attempts::new(&backend, 0);
        log_requests(&root);
        attempts.discover(states[1..].to_vec(), true)?;
        attempts.discover(states[1..].to_vec(), true)?;
        let mut probes: Vec<String> = states[1..].iter().map(|id| format!("states/{id}/")).collect();
        probes.sort();
        let mut lists = requests(&root, "list");
        lists[1..].sort();
        assert_eq!(lists[0], "states/", "two unknown states cost more probes than one estimated page");
        assert_eq!(lists[1..], probes, "the listing's {} objects cost two pages", LISTING_PAGE + 1);
        Ok(std::fs::remove_dir_all(root)?)
    }

    /// A sweep keeps a tree that a kept tree uses as its base, but not the chunks that only it names. A delta on that
    /// base copies the chunk ids of its unchanged files without chunking them; they go up again from the local state.
    #[test]
    fn a_tree_kept_as_a_base_loses_its_own_chunks_and_a_child_uploads_them_again() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        // The records, so the sweep checks the signatures of the trees below.
        push(&store, &ctx, &repo, &Options::default())?;
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let key = layr_store::chunks::key(&identity, "epoch");
        store.layer = Some(super::super::overlays::LayerAccess {
            recipient: descriptor.recipient.clone(),
            identity: Some(identity.clone()),
            chunk_key: Some(key),
        });
        let noise = |n: usize| {
            let mut bytes = vec![0; n];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            bytes
        };
        let (shared, old) = (noise(200_000), noise(200_000));
        let base = add_state(&ctx, &repo, &[], &[("shared", &shared), ("old", &old)])?;
        let kept = add_state(&ctx, &repo, std::slice::from_ref(&base), &[("shared", &shared), ("old", &noise(1000))])?;
        let path = |id: &str| repo.p.state_path(id);
        let (pushed, _, _) = upload_state(&store, &descriptor, &base, &path(&base), None, &mut BTreeMap::new(), None, None)?;
        let base = (base, pushed);
        let base_root = path(&base.0);
        upload_state(&store, &descriptor, &kept, &path(&kept), Some(&base), &mut BTreeMap::new(), Some(&base_root), None)?;
        std::env::set_var("LAYR_TEST_CHUNK_GRACE", "0");
        let swept = super::super::sweep::sweep(&store, &[], &BTreeSet::from([kept]), &|_| true, &|_| None, false)?;
        assert!(reader::attempts(&store.backend)?.contains_key(&base.0), "the base tree of a kept tree stays");
        let places = || super::super::chunked::places(&store.backend, None, &identity);
        let inherited = &base.1.tree.chunks;
        assert!(!inherited["old"].iter().all(|id| places().is_ok_and(|p| p.contains_key(id))), "released: {swept:?}");
        // A child of the base that keeps both files: the delta walk copies their ids and chunks nothing.
        std::thread::scope(|scope| -> Result<()> {
            let mut chunker =
                super::super::chunked::Writer::open(scope, &store, &descriptor.recipient, &identity, key, BTreeMap::new())?;
            chunker.finish(&base_root, inherited, &|root: &Path, path: &str| Ok(std::fs::read(root.join(path))?))
        })?;
        let now = places()?;
        assert!(inherited.values().flatten().all(|id| now.contains_key(id)), "every chunk of the child is in a pack");
        finish_fixture(&ctx, repo, &root)
    }

    /// A kept tree that a pass cannot read may link any part: the pass deletes nothing, rather than the part of a dead
    /// tree that the kept one reuses. A listed tree that a read finds absent, or whose signer has no key here, may read
    /// on the next pass too.
    #[test]
    fn a_sweep_that_cannot_read_a_kept_tree_deletes_nothing() -> Result<()> {
        for fault in ["get", "absent", "signer"] {
            a_sweep_that_cannot_read_a_kept_tree_deletes_nothing_on(fault)?;
        }
        Ok(())
    }

    fn a_sweep_that_cannot_read_a_kept_tree_deletes_nothing_on(fault: &'static str) -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let repo = branch_fixture(&ctx, &store);
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 4);
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let recipient = descriptor.recipient.clone();
        store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: Some(identity), chunk_key: None });
        let dead = add_state(&ctx, &repo, &[], &[("same", b"unchanged\n")])?;
        let kept = add_state(&ctx, &repo, std::slice::from_ref(&dead), &[("payload", b"kept\n"), ("same", b"unchanged\n")])?;
        assert_eq!(push(&store, &ctx, &repo, &Options::default())?.states, 2);
        let objects = || -> Result<Vec<String>> { Ok(store.backend.list("states/")?.into_iter().map(|o| o.key).collect()) };
        let before = objects()?;
        let faults = || super::super::backend::FILE_FAULTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tree = root.join("remote").join(format::tree_name(&kept, &reader::attempts(&store.backend)?[&kept]));
        let original = std::fs::read(&tree)?;
        if fault == "signer" {
            std::fs::write(&tree, unknown_signer(&store, &original, None)?)?;
        } else {
            faults().insert(root.join("remote"), vec![(fault, format!("states/{kept}/"))]);
        }
        let swept = super::super::sweep::sweep(&store, &[], &BTreeSet::from([kept.clone()]), &|_| true, &|_| None, false);
        faults().remove(&root.join("remote"));
        std::fs::write(&tree, &original)?;
        let swept = swept?;
        assert!(swept.trees.is_empty() && swept.parts.is_empty(), "{fault}: {swept:?}");
        assert_eq!(objects()?, before);
        read_back(&store, &kept)?;
        finish_fixture(&ctx, repo, &root)
    }

    /// The tree object `object` with a signer that no machine record names, and `frame` in place of its frame when
    /// given.
    fn unknown_signer(store: &Store, object: &[u8], frame: Option<&[u8]>) -> Result<Vec<u8>> {
        let descriptor = store.descriptor()?;
        let plain = keys::decrypt(&store.identity(&descriptor, None)?, object)?;
        let (mut header, old) = format::split_tree(&plain)?;
        header.machine = crate::ids::new_id();
        let mut out = serde_json::to_vec(&header)?;
        out.push(b'\n');
        out.extend_from_slice(frame.unwrap_or(old));
        keys::encrypt(&descriptor.recipient, &out)
    }

    /// A kept tree that never decodes (bytes that do not decrypt, a delta whose base tree is gone, or a frame that does
    /// not decompress, also with a signer that has no key here) restores nothing: the pass leaves it and still deletes
    /// the dead trees beside it.
    #[test]
    fn a_sweep_beside_a_kept_tree_that_never_decodes_deletes_dead_trees() -> Result<()> {
        for broken in ["bytes", "base", "frame"] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            let (ctx, mut store) = counted_remote(&root)?;
            let repo = branch_fixture(&ctx, &store);
            push(&store, &ctx, &repo, &Options::default())?;
            let descriptor = store.descriptor()?;
            let identity = store.identity(&descriptor, None)?;
            let recipient = descriptor.recipient.clone();
            store.layer =
                Some(super::super::overlays::LayerAccess { recipient, identity: Some(identity.clone()), chunk_key: None });
            let expired = add_state(&ctx, &repo, &[], &[("old", b"expired\n")])?;
            let base = add_state(&ctx, &repo, &[], &[("same", b"unchanged\n")])?;
            let kept = add_state(&ctx, &repo, std::slice::from_ref(&base), &[("same", b"unchanged\n"), ("new", b"kept\n")])?;
            push(&store, &ctx, &repo, &Options::default())?;
            let attempts = reader::attempts(&store.backend)?;
            let tree = |state: &str| format::tree_name(state, &attempts[state]);
            let (header, _) = format::split_tree(&store.backend.decrypted(&tree(&kept), &identity)?)?;
            assert_eq!(header.base.map(|base| base.0), Some(base.clone()), "the kept tree is a delta");
            let path = root.join("remote").join(if broken == "base" { tree(&base) } else { tree(&kept) });
            match broken {
                "base" => std::fs::remove_file(path)?,
                "bytes" => std::fs::write(path, b"not a tree")?,
                _ => std::fs::write(&path, unknown_signer(&store, &std::fs::read(&path)?, Some(b"not a frame"))?)?,
            }
            let swept = super::super::sweep::sweep(&store, &[], &BTreeSet::from([kept.clone()]), &|_| true, &|_| None, false)?;
            assert!(swept.trees.contains(&tree(&expired)), "{broken}: {swept:?}");
            assert!(!store.backend.list("states/")?.iter().any(|o| o.key == tree(&expired)));
            finish_fixture(&ctx, repo, &root)?;
        }
        Ok(())
    }

    /// A sweep released the chunks of a file, and a push uploads them again in a new pack. A later sweep, while that
    /// push has not published its tree yet, must not take the expired mark of the release for the new pack: the
    /// published tree restores.
    #[test]
    fn a_chunk_uploaded_again_after_its_release_survives_a_sweep_before_its_tree() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let key = layr_store::chunks::key(&identity, "epoch");
        let mut data = vec![0; 300_000];
        rand::rngs::OsRng.fill_bytes(&mut data);
        let source = root.join("state");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("file"), &data)?;
        // A pass released these chunks three hours ago, and its marks stay. A pass needs an hour of grace.
        let marked = chrono::Utc::now().timestamp() - 3 * 3600;
        let marks: String = layr_store::chunks::split(&data)
            .iter()
            .map(|chunk| format!("{} {marked}\n", layr_store::chunks::id(&key, chunk)))
            .collect();
        std::fs::write(store.dir.join("dead-chunks"), marks)?;
        std::env::set_var("LAYR_TEST_CHUNK_GRACE", "3600");
        let layer = || super::super::overlays::LayerAccess {
            recipient: descriptor.recipient.clone(),
            identity: Some(identity.clone()),
            chunk_key: Some(key),
        };
        store.layer = Some(layer());
        let sweeper = Store { layer: Some(layer()), ..remote(&root, &ctx) };
        PUSH_BEFORE_TREE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_, _| {
                let unknown = |_: &str| false;
                super::super::sweep::sweep(&sweeper, &[], &BTreeSet::new(), &unknown, &|_| None, false).unwrap();
            }))
        });
        let state = crate::ids::new_id();
        let pushed = upload_state(&store, &descriptor, &state, &source, None, &mut BTreeMap::new(), None, None);
        PUSH_BEFORE_TREE.with(|hook| hook.borrow_mut().take());
        let (pushed, _, _) = pushed?;
        let mut chunks = super::super::chunked::Reader::new(&store.backend, &identity);
        assert_eq!(chunks.file(&pushed.tree.chunks["file"], data.len() as u64)?, data);
        Ok(std::fs::remove_dir_all(root)?)
    }

    /// A pass releases the chunks of a pack that an earlier pass found dead. A push uploads them again in a new pack
    /// after the pass looked at the packs and before it releases them: the release leaves the new pack alone.
    #[test]
    fn a_pack_published_between_a_sweep_scan_and_its_release_stays() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (ctx, mut store) = counted_remote(&root)?;
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        let key = layr_store::chunks::key(&identity, "epoch");
        let mut data = vec![0; 300_000];
        rand::rngs::OsRng.fill_bytes(&mut data);
        let layer = || super::super::overlays::LayerAccess {
            recipient: descriptor.recipient.clone(),
            identity: Some(identity.clone()),
            chunk_key: Some(key),
        };
        store.layer = Some(layer());
        // A pack of these chunks that no tree names yet.
        fn pack(store: &Store, data: &[u8]) -> Result<Vec<String>> {
            use layr_store::chunks;
            let layer = store.layer.as_ref().context("layer")?;
            let mut builder = chunks::Builder::default();
            let mut ids = Vec::new();
            for chunk in chunks::split(data) {
                ids.push(chunks::id(&layer.chunk_key.context("key")?, chunk));
                builder.add(ids.last().unwrap().clone(), chunk);
            }
            let (frame, index) = builder.finish()?;
            let new = crate::ids::new_id();
            store.backend.put_new(&chunks::pack_name(&new), keys::encrypt(&layer.recipient, &frame)?)?;
            store.backend.put_new(&chunks::index_name(&new), keys::encrypt(&layer.recipient, &index.encode())?)?;
            Ok(ids)
        }
        // A push that stopped before its tree four hours ago, whose chunks a pass found dead three hours ago. Another
        // push uploads them again: its own pack goes up while this pass runs.
        let ids = pack(&store, &data)?;
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(4 * 3600);
        let scanned = store.backend.list("chunks/")?;
        for object in &scanned {
            std::fs::File::options().write(true).open(root.join("remote").join(&object.key))?.set_modified(old)?;
        }
        let marked = chrono::Utc::now().timestamp() - 3 * 3600;
        let marks: String = ids.iter().map(|id| format!("{id} {marked}\n")).collect();
        std::fs::write(store.dir.join("dead-chunks"), marks)?;
        std::env::set_var("LAYR_TEST_CHUNK_GRACE", "3600");
        let pusher = Store { layer: Some(layer()), ..remote(&root, &ctx) };
        let (again, copy) = (std::rc::Rc::new(std::cell::RefCell::new(Vec::new())), data.clone());
        let uploaded = again.clone();
        super::super::sweep::BEFORE_RELEASE
            .with(|hook| *hook.borrow_mut() = Some(Box::new(move || *uploaded.borrow_mut() = pack(&pusher, &copy).unwrap())));
        let swept = super::super::sweep::sweep(&store, &[], &BTreeSet::new(), &|_| false, &|_| None, false);
        super::super::sweep::BEFORE_RELEASE.with(|hook| hook.borrow_mut().take());
        let swept = swept?;
        assert_eq!(swept.packs.len(), scanned.len(), "the scanned pack and its index go: {swept:?}");
        assert_eq!(*again.borrow(), ids);
        let mut chunks = super::super::chunked::Reader::new(&store.backend, &identity);
        assert_eq!(chunks.file(&ids, data.len() as u64)?, data);
        Ok(std::fs::remove_dir_all(root)?)
    }

    /// Live bytes in heap blocks of at least 1 MiB while `ON`: the part, ciphertext and request buffers of uploads.
    #[allow(unsafe_code)] // A test-only global allocator: it forwards every call to the system allocator and counts.
    pub(crate) mod large_blocks {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::SeqCst};
        pub static ON: AtomicBool = AtomicBool::new(false);
        pub static LIVE: AtomicI64 = AtomicI64::new(0);
        pub static MOST: AtomicI64 = AtomicI64::new(0);
        fn count(size: usize, sign: i64) {
            if size >= 1 << 20 && ON.load(SeqCst) {
                let change = sign * i64::try_from(size).unwrap_or(i64::MAX);
                MOST.fetch_max(LIVE.fetch_add(change, SeqCst) + change, SeqCst);
            }
        }
        struct Counting;
        // SAFETY: each method passes its arguments unchanged to `System`, which upholds the GlobalAlloc contract.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                // SAFETY: the caller's arguments, unchanged, under the same GlobalAlloc contract.
                let block = unsafe { System.alloc(layout) };
                if !block.is_null() {
                    count(layout.size(), 1);
                }
                block
            }
            unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
                // SAFETY: the caller's arguments, unchanged, under the same GlobalAlloc contract.
                unsafe { System.dealloc(block, layout) };
                count(layout.size(), -1);
            }
            unsafe fn realloc(&self, block: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                // SAFETY: the caller's arguments, unchanged, under the same GlobalAlloc contract.
                let moved = unsafe { System.realloc(block, layout, size) };
                if !moved.is_null() {
                    count(layout.size(), -1);
                    count(size, 1);
                }
                moved
            }
        }
        #[global_allocator]
        static ALLOCATOR: Counting = Counting;
    }

    const PART_MEMORY_BUDGET: u64 = 96 << 20;

    #[test]
    #[ignore = "subprocess helper for parts_hold_at_most_their_charge_through_the_gateway"]
    fn part_memory_process() -> Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        // CI runs ignored tests too; without the variable this is not the measuring subprocess.
        if std::env::var_os("LAYR_TEST_PART_MEMORY").is_none() {
            return Ok(());
        }
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root)?;
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        // Four PUTs answered in pairs: two parts always travel together.
        let (gateway, _endpoint, server) = super::super::gateway::tests::put_fixture(4, 2);
        store.backend = super::super::backend::Backend::Gateway(gateway);
        let recipient = keys::recipient_of(&keys::generate_identity())?;
        let descriptor = Descriptor { format: 2, project: store.project.clone(), recipient, mode: Default::default() };
        let state = crate::ids::new_id();
        let put = |(part, data, entries): PartJob| put_part(&store, &descriptor, &state, "0123abcd", part, data, &entries);
        large_blocks::ON.store(true, SeqCst);
        let sent = std::thread::scope(|scope| -> Result<()> {
            let mut parts = PartPool::new(scope, super::super::download::JOBS, PART_MEMORY_BUDGET, put);
            for part in 0..4 {
                send_part(&mut parts, part, vec![7; format::PART_LIMIT], Vec::new())?;
            }
            parts.finish(&mut |_, ()| Ok(()))
        });
        large_blocks::ON.store(false, SeqCst);
        sent?;
        server.join().map_err(|_| anyhow::anyhow!("server failed"))?;
        println!("part memory {}", large_blocks::MOST.load(SeqCst));
        Ok(std::fs::remove_dir_all(root)?)
    }

    /// The most bytes of large blocks that the ignored test `name` measured in a subprocess: the allocator counts the
    /// whole process, and only that test runs there.
    pub(crate) fn large_blocks_of(test: &str) -> Result<u64> {
        large_blocks_with(test, "1")
    }

    /// `large_blocks_of`, with `LAYR_TEST_PART_MEMORY` set to `kind`.
    fn large_blocks_with(test: &str, kind: &str) -> Result<u64> {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--ignored", "--exact", test, "--nocapture"])
            .env("LAYR_TEST_PART_MEMORY", kind)
            .output()?;
        let log = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{log}{}", String::from_utf8_lossy(&output.stderr));
        Ok(log.lines().find_map(|line| line.strip_prefix("part memory ")).context("no measurement")?.parse()?)
    }

    const LARGE_FILE: usize = 40 << 20;

    #[test]
    #[ignore = "subprocess helper for the_walk_holds_one_large_file_at_a_time"]
    fn walk_memory_process() -> Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        if std::env::var_os("LAYR_TEST_PART_MEMORY").is_none() {
            return Ok(());
        }
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (_ctx, mut store) = counted_remote(&root)?;
        let descriptor = store.descriptor()?;
        let source = root.join("state");
        std::fs::create_dir(&source)?;
        let kind = std::env::var("LAYR_TEST_PART_MEMORY")?;
        if kind == "chunked" {
            let identity = store.identity(&descriptor, None)?;
            let chunk_key = Some(layr_store::chunks::key(&identity, "epoch"));
            let recipient = descriptor.recipient.clone();
            store.layer = Some(super::super::overlays::LayerAccess { recipient, identity: Some(identity), chunk_key });
        }
        for name in ["a", "b"] {
            let bytes = if kind == "compressible" {
                vec![name.as_bytes()[0]; COMPRESSIBLE_FILE]
            } else if kind == "chunked" {
                // Compressible, and no chunk twice: random hex digits.
                let mut bytes = vec![0; COMPRESSIBLE_FILE];
                rand::rngs::OsRng.fill_bytes(&mut bytes);
                bytes.iter_mut().for_each(|byte| *byte = b"0123456789abcdef"[usize::from(*byte & 15)]);
                bytes
            } else {
                let mut bytes = vec![0; LARGE_FILE];
                rand::rngs::OsRng.fill_bytes(&mut bytes);
                bytes
            };
            std::fs::write(source.join(name), bytes)?;
        }
        large_blocks::ON.store(true, SeqCst);
        let uploaded = upload_state(&store, &descriptor, &crate::ids::new_id(), &source, None, &mut BTreeMap::new(), None, None);
        large_blocks::ON.store(false, SeqCst);
        uploaded?;
        println!("part memory {}", large_blocks::MOST.load(SeqCst));
        Ok(std::fs::remove_dir_all(root)?)
    }

    /// Two incompressible files, each above `PART_LIMIT`: the frame of the first goes up before the second is read.
    #[test]
    fn the_walk_holds_one_large_file_at_a_time() -> Result<()> {
        let most = large_blocks_of("store_remote::writer::tests::walk_memory_process")?;
        // One file and its frame while it is read, or its part while it travels alone.
        let frame = zst::compress_bound(LARGE_FILE);
        let bound = walk_charge(LARGE_FILE as u64, false).max(part_charge(frame, frame));
        eprintln!("two incompressible files of 40 MiB: {} MiB of large blocks at most ({} MiB bound)", most >> 20, bound >> 20);
        // Before, the second file and its frame came beside the first frame: three files' worth.
        assert!(most <= bound, "{most} bytes held");
        Ok(())
    }

    const COMPRESSIBLE_FILE: usize = 96 << 20;

    /// Two large compressible files: the small frame of the first starts a part, and the part must not keep the
    /// buffer of the file's size while the second file and its frame are held.
    #[test]
    fn a_small_frame_of_a_large_file_keeps_no_room_of_the_file() -> Result<()> {
        let most = large_blocks_with("store_remote::writer::tests::walk_memory_process", "compressible")?;
        let bound = building_charge() + walk_charge(COMPRESSIBLE_FILE as u64, false);
        eprintln!("two compressible files of 96 MiB: {} MiB of large blocks at most ({} MiB bound)", most >> 20, bound >> 20);
        assert!(most <= bound, "{most} bytes held");
        Ok(())
    }

    /// The same files as chunks of a layer: the file, the pack being built and the packs that travel stay within the
    /// upload budget.
    #[test]
    fn chunked_files_and_their_packs_stay_within_the_upload_budget() -> Result<()> {
        let most = large_blocks_with("store_remote::writer::tests::walk_memory_process", "chunked")?;
        let bound = super::super::download::TRANSFER_BUFFER_BUDGET as u64;
        eprintln!(
            "two compressible files of 96 MiB as chunks: {} MiB of large blocks at most ({} MiB bound)",
            most >> 20,
            bound >> 20
        );
        assert!(most <= bound, "{most} bytes held");
        Ok(())
    }

    #[test]
    fn parts_hold_at_most_their_charge_through_the_gateway() -> Result<()> {
        let most = large_blocks_of("store_remote::writer::tests::part_memory_process")?;
        let ciphertext = keys::ciphertext_len(format::PART_LIMIT) as u64;
        eprintln!("4 parts of 16 MiB through the gateway, 96 MiB budget: {} MiB of large blocks at most", most >> 20);
        // Before, each try kept two more copies of the ciphertext and the plaintext: 64 MiB a part.
        assert!(most <= PART_MEMORY_BUDGET, "{most} bytes held");
        assert!(most >= 2 * ciphertext, "two parts travel together: {most}");
        Ok(())
    }

    #[test]
    fn an_oversized_part_travels_alone_and_a_failed_part_stops_the_uploads() -> Result<()> {
        use std::sync::{Barrier, Mutex};
        // Its charge exceeds the whole budget; the ordinary parts around it travel two at a time.
        let (events, together) = (Mutex::new(Vec::new()), Barrier::new(2));
        let log = |event: String| events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
        let put = |(part, data, _): PartJob| -> Result<()> {
            log(format!("start {part}"));
            if data.len() <= format::PART_LIMIT {
                together.wait();
            }
            log(format!("end {part}"));
            Ok(())
        };
        let budget = 100 << 20;
        std::thread::scope(|scope| -> Result<()> {
            let mut parts = PartPool::new(scope, super::super::download::JOBS, budget, put);
            for (part, size) in [(0, 1 << 10), (1, 1 << 10), (2, 60 << 20), (3, 1 << 10), (4, 1 << 10)] {
                assert!(part != 2 || part_charge(size, size) > budget);
                send_part(&mut parts, part, vec![0; size], Vec::new())?;
            }
            parts.finish(&mut |_, ()| Ok(()))
        })?;
        let events = events.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = |event: &str| events.iter().position(|e| e == event).unwrap_or(usize::MAX);
        assert!(at("start 1") < at("end 0"), "ordinary parts overlap: {events:?}");
        assert_eq!(at("start 2") + 1, at("end 2"), "the oversized part travels alone: {events:?}");
        assert!(at("end 0").max(at("end 1")) < at("start 2") && at("end 2") < at("start 3"), "{events:?}");
        let fail = |(part, _, _): PartJob| if part == 1 { bail!("part 1 refused") } else { Ok(()) };
        let failed = std::thread::scope(|scope| -> Result<()> {
            let mut parts = PartPool::new(scope, super::super::download::JOBS, budget, fail);
            for part in 0..4 {
                send_part(&mut parts, part, vec![0; 4], Vec::new())?;
            }
            parts.finish(&mut |_, ()| Ok(()))
        });
        assert!(failed.is_err_and(|error| error.to_string() == "part 1 refused"));
        Ok(())
    }

    #[test]
    fn the_walk_builds_an_oversized_part_only_when_no_part_travels() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let (_ctx, store) = counted_remote(&root)?;
        let descriptor = store.descriptor()?;
        let source = root.join("state");
        std::fs::create_dir(&source)?;
        // Incompressible: a1+a2 make part 0, a3 part 1, the 20 MiB frame of b part 2 alone, c part 3.
        for (name, size) in [("a1", 6 << 20), ("a2", 6 << 20), ("a3", 6 << 20), ("b", 20 << 20), ("c", 1 << 10)] {
            let mut bytes = vec![0; size];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            std::fs::write(source.join(name), bytes)?;
        }
        let id = crate::ids::new_id();
        upload_state(&store, &descriptor, &id, &source, None, &mut BTreeMap::new(), None, None)?;
        let events = PART_EVENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id).unwrap_or_default();
        let at = |event: &str| events.iter().position(|e| e == event).unwrap_or(usize::MAX);
        eprintln!("ordinary then oversized parts: {events:?}");
        assert!(at("end 0").max(at("end 1")) < at("build 2"), "built while parts travelled: {events:?}");
        assert_eq!((at("build 2") + 1, at("build 2") + 2), (at("start 2"), at("end 2")), "{events:?}");
        assert!(at("end 2") < at("start 3") && at("end 3") != usize::MAX, "{events:?}");
        Ok(std::fs::remove_dir_all(root)?)
    }

    #[test]
    fn completion_metadata_is_bounded_and_invalid_metadata_rebuilds() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let index: BTreeMap<_, _> = (0..INDEX_STATES + 2)
            .map(|_| (crate::ids::new_id(), Cached { attempt: "0123abcd".into(), depth: 0, root: None }))
            .collect();
        checkpoint(&root, &index).unwrap();
        let restored = load_index(&root).unwrap();
        assert_eq!(restored.len(), INDEX_STATES);
        assert!(!restored.contains_key(index.first_key_value().unwrap().0));
        assert!(restored.contains_key(index.last_key_value().unwrap().0));
        for bytes in [b"damaged".as_slice(), b"{\"format\":2,\"states\":false}".as_slice()] {
            std::fs::write(root.join("pushed-v2.json.zst"), zst::compress(bytes, None).unwrap()).unwrap();
            assert!(load_index(&root).unwrap().is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_old_push_index_is_ignored_and_removed() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let id = crate::ids::new_id();
        let pushed = Pushed { attempt: "0123abcd".into(), tree: Tree::default(), depth: 0, canonical: Vec::new() };
        let old = BTreeMap::from([(id.clone(), pushed)]);
        std::fs::write(root.join("pushed.json"), serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(load_index(&root).unwrap().is_empty());
        let index = BTreeMap::from([(id.clone(), Cached { attempt: "0123abcd".into(), depth: 0, root: None })]);
        checkpoint(&root, &index).unwrap();
        assert_eq!(load_index(&root).unwrap()[&id].attempt, "0123abcd");
        assert!(!root.join("pushed.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn an_existing_key_file_is_not_reused_without_an_openable_unfinished_slot() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        let path = root.join("keep.txt");
        fsutil::atomic_write(&path, b"do not replace", 0o600).unwrap();
        assert!(store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(path.clone()), ..Default::default() })
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"do not replace");
        assert!(store.backend.get("store.json").unwrap().is_none());
        assert!(store
            .configure(
                &ctx,
                &super::super::ConfigureOptions { paper_key_out: Some(ctx.store.root.join("bad.txt")), ..Default::default() }
            )
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn two_initializers_never_publish_different_slot_identities() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        // The shared local store must be initialized before a configure writes its read marker (`mark_read`).
        let _initialized = context(&root);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|number| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let own = root.join(format!("writer-{number}"));
                    std::fs::create_dir(&own).unwrap();
                    let ctx = context(&own);
                    let mut store = remote(&root, &ctx);
                    barrier.wait();
                    let result = store.configure(
                        &ctx,
                        &super::super::ConfigureOptions { paper_key_out: Some(own.join("paper.txt")), ..Default::default() },
                    );
                    result.map(|r| r.created)
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| matches!(r, Ok(true))).count(), 1);
        let ctx = context(&root);
        let store = remote(&root, &ctx);
        let descriptor = store.descriptor().unwrap();
        let slot = store.backend.get("slots/paper.age").unwrap().unwrap();
        let opened: Vec<_> = (0..2)
            .filter_map(|n| std::fs::read_to_string(root.join(format!("writer-{n}/paper.txt"))).ok())
            .filter_map(|words| keys::identity_from_paper(&words).ok())
            .filter_map(|key| keys::open_slot(&slot, &key).ok())
            .collect();
        assert_eq!(opened.len(), 1);
        assert_eq!(keys::recipient_of(&opened[0]).unwrap(), descriptor.recipient);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn state_deltas_have_bounded_chains_and_reuse_content_across_lines() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let descriptor = store.descriptor().unwrap();
        let mut base = None;
        let mut base_root = None;
        let mut content =
            (0..3000).map(|n| format!("some sufficiently varied file data {n}: {:x}\n", n * 16127)).collect::<String>();
        let mut checkpoints = 0;
        let mut saw_delta = false;
        for number in 0..34 {
            let source = root.join(format!("state-{number}"));
            std::fs::create_dir(&source).unwrap();
            content.push_str(&format!("change number {number}\n"));
            std::fs::write(source.join("a"), &content).unwrap();
            let id = crate::ids::new_id();
            let (pushed, _, _) =
                upload_state(&store, &descriptor, &id, &source, base.as_ref(), &mut BTreeMap::new(), base_root.as_deref(), None)
                    .unwrap();
            assert!(pushed.tree.files["a"].3.len() <= 17);
            saw_delta |= pushed.tree.files["a"].3.len() > 1;
            if pushed.depth == 0 {
                checkpoints += 1;
            }
            base = Some((id, pushed));
            base_root = Some(source);
        }
        assert!(saw_delta);
        assert_eq!(checkpoints, 2);
        let (id, last) = base.unwrap();
        let mut hashes = last.tree.files.values().map(|e| (e.2.clone(), e.3.clone())).collect();
        let duplicate = crate::ids::new_id();
        let (dedup, frames, _) =
            upload_state(&store, &descriptor, &duplicate, base_root.as_deref().unwrap(), None, &mut hashes, None, None).unwrap();
        assert_eq!(frames, 0);
        assert_eq!(dedup.tree.files["a"].3, last.tree.files["a"].3);
        // A second complete upload after a lost response still represents one logical state.
        upload_state(&store, &descriptor, &id, base_root.as_deref().unwrap(), None, &mut hashes, None, None).unwrap();
        assert_eq!(reader::attempts(&store.backend).unwrap().len(), 35);
        let identity = store.identity(&descriptor, None).unwrap();
        let machine_keys = BTreeMap::from([(store.machine.clone(), store.key.verifying_key())]);
        let mut reader = reader::Reader::new(&store.backend, &identity, &machine_keys);
        let (_, _, tree) = reader.tree(&id, &last.attempt).unwrap();
        assert_eq!(reader.read(&tree.files["a"]).unwrap(), content.as_bytes());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_private_store_keeps_plain_objects_without_slots_and_reads_them_back() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        // A Private store has no paper key.
        let refused = store.configure(
            &ctx,
            &super::super::ConfigureOptions {
                paper_key_out: Some(root.join("paper")),
                mode: format::Mode::Private,
                ..Default::default()
            },
        );
        assert!(refused.unwrap_err().to_string().contains("no paper key"));
        let configured =
            store.configure(&ctx, &super::super::ConfigureOptions { mode: format::Mode::Private, ..Default::default() }).unwrap();
        assert!(configured.created && configured.recovery_key_file.is_none());
        let descriptor = store.descriptor().unwrap();
        assert_eq!((descriptor.mode, descriptor.recipient.as_str()), (format::Mode::Private, keys::PLAIN));
        assert!(store.backend.list("slots/").unwrap().is_empty(), "a Private store has no slots");
        let source = root.join("state");
        std::fs::create_dir(&source).unwrap();
        let content = "plain project bytes that the server may read\n".repeat(200);
        std::fs::write(source.join("a"), &content).unwrap();
        let id = crate::ids::new_id();
        let (pushed, _, _) = upload_state(&store, &descriptor, &id, &source, None, &mut BTreeMap::new(), None, None).unwrap();
        // Every object is plain: no age header, and the tree object starts with its signed JSON header.
        for object in store.backend.list("states/").unwrap() {
            let body = store.backend.get(&object.key).unwrap().unwrap();
            assert!(!body.starts_with(b"age-encryption.org/"), "{} is encrypted", object.key);
        }
        let tree = store.backend.get(&format::tree_name(&id, &pushed.attempt)).unwrap().unwrap();
        assert!(tree.starts_with(b"{"), "the tree object is plain JSON");
        let identity = store.identity(&descriptor, None).unwrap();
        assert_eq!(identity, keys::PLAIN);
        let machine_keys = BTreeMap::from([(store.machine.clone(), store.key.verifying_key())]);
        let mut reader = reader::Reader::new(&store.backend, &identity, &machine_keys);
        let (_, _, tree) = reader.tree(&id, &pushed.attempt).unwrap();
        assert_eq!(reader.read(&tree.files["a"]).unwrap(), content.as_bytes());
        // A remote added as Secret refuses this plain store, and a Private remote refuses a Secret store.
        store.require_mode(Some(format::Mode::Secret)).unwrap();
        assert!(store.descriptor().err().unwrap().to_string().contains("store mode differs"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_secret_store_refuses_a_plain_descriptor_and_a_plain_store_an_age_recipient() {
        let identity = keys::generate_identity();
        let mut descriptor =
            Descriptor { format: 2, project: crate::ids::new_id(), recipient: keys::PLAIN.into(), mode: format::Mode::Secret };
        assert!(descriptor.check_mode().is_err());
        descriptor.mode = format::Mode::Public;
        descriptor.check_mode().unwrap();
        descriptor.recipient = keys::recipient_of(&identity).unwrap();
        assert!(descriptor.check_mode().is_err());
        descriptor.mode = format::Mode::Secret;
        descriptor.check_mode().unwrap();
        // Secret descriptors keep today's bytes: no mode field.
        assert!(!String::from_utf8(serde_json::to_vec(&descriptor).unwrap()).unwrap().contains("mode"));
    }
    #[test]
    fn collection_keeps_complete_reused_and_young_parts() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let descriptor = store.descriptor().unwrap();
        let mut record = crate::model::Record::untrusted_preview("machine");
        record.id = crate::ids::new_id();
        record.seq = 1;
        record.machine = store.machine.clone();
        record.project = store.project.clone();
        record.data = serde_json::json!({"public_key":hex::encode(store.key.verifying_key().to_bytes())});
        let line = crate::oplog::sign_line(&record, &store.key).unwrap();
        store
            .backend
            .put_new(
                &format::segment_name(&store.machine, 1, 1),
                keys::encrypt(&descriptor.recipient, format!("{line}\n").as_bytes()).unwrap(),
            )
            .unwrap();
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a"), b"live bytes").unwrap();
        let id = crate::ids::new_id();
        let (pushed, _, _) = upload_state(&store, &descriptor, &id, &source, None, &mut BTreeMap::new(), None, None).unwrap();
        let complete = pushed.tree.files["a"].3[0].object();
        let orphan = format::data_name(&crate::ids::new_id(), "1234abcd", 0);
        let young = format::data_name(&crate::ids::new_id(), "1234abcd", 0);
        store.backend.put_new(&orphan, b"unfinished old upload".to_vec()).unwrap();
        store.backend.put_new(&young, b"concurrent young upload".to_vec()).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90000);
        for key in [&complete, &orphan] {
            std::fs::File::options().write(true).open(root.join("remote").join(key)).unwrap().set_modified(old).unwrap();
        }
        assert_eq!(store.gc(true).unwrap(), vec![orphan.clone()]);
        assert!(store.backend.get(&orphan).unwrap().is_some());
        assert_eq!(store.gc(false).unwrap(), vec![orphan.clone()]);
        assert!(store.backend.get(&orphan).unwrap().is_none());
        assert!(store.backend.get(&young).unwrap().is_some());
        assert!(store.backend.get(&complete).unwrap().is_some());
        let barrier = std::sync::Barrier::new(2);
        let concurrent = crate::ids::new_id();
        let pushed = std::thread::scope(|scope| {
            let upload = scope.spawn(|| {
                barrier.wait();
                upload_state(&store, &descriptor, &concurrent, &source, None, &mut BTreeMap::new(), None, None).unwrap().0
            });
            barrier.wait();
            for _ in 0..10 {
                store.gc(false).unwrap();
            }
            upload.join().unwrap()
        });
        let identity = store.identity(&descriptor, None).unwrap();
        let machine_keys = BTreeMap::from([(store.machine.clone(), store.key.verifying_key())]);
        let mut reader = reader::Reader::new(&store.backend, &identity, &machine_keys);
        let (_, _, restored) = reader.tree(&concurrent, &pushed.attempt).unwrap();
        assert_eq!(reader.read(&restored.files["a"]).unwrap(), b"live bytes");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "portable-state fixture invoked by t23-store-portable-peer"]
    fn emit_portable_v2_fixture() {
        use crate::model::{LineChange, LineState, StateRec};
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // The fixture writer runs only from t23, which sets its folders.
        let Ok(destination) = std::env::var("LAYR_TEST_PORTABLE_DIR") else { return };
        let paper_file = std::env::var("LAYR_TEST_PORTABLE_KEY").expect("fixture key path");
        let local = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&local).unwrap();
        let ctx = context(&local);
        let mut store = remote(&local, &ctx);
        store.backend = super::super::backend::Backend::file(destination.into());
        let identity = keys::generate_identity();
        let paper = keys::generate_identity();
        let descriptor = Descriptor {
            format: 2,
            project: store.project.clone(),
            recipient: keys::recipient_of(&identity).unwrap(),
            mode: Default::default(),
        };
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(paper_file).unwrap();
        writeln!(output, "{}", keys::words_of_identity(&paper).unwrap()).unwrap();
        store
            .backend
            .put_new("slots/paper.age", keys::seal_slot(&identity, &keys::recipient_of(&paper).unwrap()).unwrap())
            .unwrap();
        store.backend.put_new("store.json", serde_json::to_vec(&descriptor).unwrap()).unwrap();
        let state_id = crate::ids::new_id();
        let line_id = crate::ids::new_id();
        let state:StateRec=serde_json::from_value(serde_json::json!({"id":state_id,"kind":"commit","parents":[],"line":line_id,
            "subvol":crate::ids::new_id(),"author":{"name":"Fixture","email":"fixture@example.test"},"message":"portable source","time":1})).unwrap();
        let mut first = crate::model::Record::untrusted_preview("machine");
        first.data = serde_json::json!({"name":"portable","public_key":hex::encode(store.key.verifying_key().to_bytes())});
        let mut project = crate::model::Record::untrusted_preview("project");
        project.data = serde_json::json!({"id":store.project,"name":"project","config":{}});
        let mut commit = crate::model::Record::untrusted_preview("commit");
        commit.states.push(state);
        commit.lines.insert(
            line_id,
            LineChange {
                transition: crate::model::LineTransition::Create {
                    after: LineState {
                        name: "main".into(),
                        head: state_id.clone(),
                        owner: ctx.caller.uid,
                        machine: store.machine.clone(),
                    },
                },
                working_before: None,
                working_after: None,
            },
        );
        let mut lines = Vec::new();
        for (index, mut record) in [first, project, commit].into_iter().enumerate() {
            record.id = crate::ids::new_id();
            record.machine = store.machine.clone();
            record.project = store.project.clone();
            record.seq = index as u64 + 1;
            lines.push(crate::oplog::sign_line(&record, &store.key).unwrap());
        }
        store
            .backend
            .put_new(
                &format::segment_name(&store.machine, 1, 3),
                keys::encrypt(&descriptor.recipient, format!("{}\n", lines.join("\n")).as_bytes()).unwrap(),
            )
            .unwrap();
        let source = local.join("files");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("notes.txt"), b"portable file bytes\n").unwrap();
        upload_state(&store, &descriptor, &state_id, &source, None, &mut BTreeMap::new(), None, None).unwrap();
        std::fs::remove_dir_all(local).unwrap();
    }
}

#[cfg(test)]
mod format_gap_tests {
    use super::format_gap;

    #[test]
    fn a_plain_tree_has_no_format_gap() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-gap-plain-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("src/sub"))?;
        std::fs::write(root.join("src/sub/file.txt"), b"content")?;
        std::os::unix::fs::symlink("src/sub/file.txt", root.join("link"))?;
        let gap = format_gap(&root);
        std::fs::remove_dir_all(&root)?;
        assert_eq!(gap?, None);
        Ok(())
    }

    #[test]
    fn an_empty_folder_or_a_hard_link_is_a_gap() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-gap-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("empty"))?;
        std::fs::write(root.join("file"), b"content")?;
        let empty = format_gap(&root)?;
        std::fs::remove_dir(root.join("empty"))?;
        std::fs::hard_link(root.join("file"), root.join("alias"))?;
        let linked = format_gap(&root);
        std::fs::remove_dir_all(&root)?;
        assert_eq!(empty, Some("an empty folder"));
        assert_eq!(linked?, Some("a hard link"));
        Ok(())
    }
}
