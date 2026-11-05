//! The deletion of dead cache states in a store remote (docs/cache-overlays.md, section 7). The data
//! of a cache overlay goes when the operation that makes it dead runs: a push of the overlay, and the
//! daemon's pass for a remote that nobody pushes to. Nothing else deletes it, and no gc is needed.
//!
//! A tree stays when its state is kept by the records (`Repo::cache_kept`: the newest `history`
//! states of each live line's overlay line on every platform, the states that lines show, the root),
//! when this machine does not know its state yet (another machine pushed it after our last fetch),
//! when a kept tree is a delta of it, or when it is the nearest pushed ancestor of a kept state whose
//! tree is not in the remote yet: a push in flight on another machine reuses the parts of that tree. The data parts of the deleted trees go when no kept tree
//! links them. Parts first, then the tree: a stop between them leaves a dead tree, which the next
//! pass deletes.
//!
//! Chunks (`chunked`): the chunks that the deleted trees name and no kept tree names are dead.
//! `chunked::release` rewrites each pack that holds one with its live chunks, after the trees go,
//! so that no dead chunk stays in a pack. A pack whose chunks no tree names at all (a push that
//! stopped before its tree) goes when its index is older than a day and none of its chunks is live.

use super::format::{parse_tree_name, tree_name};
use super::reader::{records, Reader};
use super::Store;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// What a pass deleted (or, for a dry run, would delete).
#[derive(Default, Debug)]
pub struct Swept {
    pub trees: Vec<String>,
    pub parts: Vec<String>,
    /// The frame bytes of the deleted parts (their links' lengths).
    pub bytes: u64,
    /// Trees kept because this machine does not know their state yet.
    pub unknown: usize,
    /// Pack and index objects deleted, the dead chunks in them and their plaintext bytes.
    pub packs: Vec<String>,
    pub chunks: usize,
    pub chunk_bytes: u64,
    /// Kept trees that name a chunk that no pack holds (a deletion raced their push).
    pub missing: usize,
}

/// A pack index younger than this may belong to a push in flight.
const PACK_GRACE: i64 = 24 * 3600;

/// Delete the dead cache trees of the layer `store` of a cache overlay: the trees of states that the
/// overlay's records know (`known`) and do not keep (`kept`), and their parts that no kept tree links.
pub(super) fn sweep(
    store: &Store,
    kept: &BTreeSet<String>,
    known: &dyn Fn(&str) -> bool,
    parent: &dyn Fn(&str) -> Option<String>,
    dry_run: bool,
) -> Result<Swept> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    let records = records(&store.backend, &identity, &descriptor.project)?;
    let trees: Vec<(String, String)> = store.backend.list("states/")?.iter().filter_map(|o| parse_tree_name(&o.key)).collect();
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    let mut swept = Swept::default();
    let mut keep: BTreeSet<(String, String)> = BTreeSet::new();
    for (state, attempt) in &trees {
        if !known(state) {
            swept.unknown += 1;
            keep.insert((state.clone(), attempt.clone()));
        } else if kept.contains(state) {
            keep.insert((state.clone(), attempt.clone()));
        }
    }
    // A kept state without a tree is in flight: its nearest ancestor with a tree stays.
    let pushed: BTreeSet<&str> = trees.iter().map(|(s, _)| s.as_str()).collect();
    for state in kept.iter().filter(|s| !pushed.contains(s.as_str())) {
        let mut at = parent(state);
        for _ in 0..1000 {
            let Some(id) = at else { break };
            if pushed.contains(id.as_str()) {
                keep.extend(trees.iter().filter(|(s, _)| *s == id).cloned());
                break;
            }
            at = parent(&id);
        }
    }
    // A kept delta tree needs its base trees.
    let mut pending: Vec<(String, String)> = keep.iter().cloned().collect();
    while let Some((state, attempt)) = pending.pop() {
        let (header, _, _) = reader.tree(&state, &attempt)?;
        if let Some(base) = header.base {
            if keep.insert(base.clone()) {
                pending.push(base);
            }
        }
    }
    let mut linked = BTreeSet::new();
    let mut live_chunks: BTreeSet<String> = BTreeSet::new();
    let mut kept_chunks: Vec<BTreeSet<String>> = Vec::new();
    for (state, attempt) in &keep {
        let (_, _, tree) = reader.tree(state, attempt)?;
        linked.extend(tree.files.values().flat_map(|e| e.3.iter().map(|l| l.object())));
        let named: BTreeSet<String> = tree.chunks.values().flatten().cloned().collect();
        live_chunks.extend(named.iter().cloned());
        kept_chunks.push(named);
    }
    let dead: Vec<&(String, String)> = trees.iter().filter(|t| !keep.contains(*t)).collect();
    let mut parts = BTreeSet::new();
    let mut frames = BTreeSet::new();
    let mut dead_chunks: BTreeSet<String> = BTreeSet::new();
    for (state, attempt) in &dead {
        let (_, _, tree) = reader.tree(state, attempt)?;
        dead_chunks.extend(tree.chunks.values().flatten().filter(|id| !live_chunks.contains(*id)).cloned());
        for link in tree.files.values().flat_map(|e| e.3.iter()) {
            let object = link.object();
            if !linked.contains(&object) {
                if frames.insert((object.clone(), link.3)) {
                    swept.bytes += link.4;
                }
                parts.insert(object);
            }
        }
    }
    for part in parts {
        if !dry_run {
            store.backend.delete(&part)?;
        }
        swept.parts.push(part);
    }
    for (state, attempt) in dead {
        let name = tree_name(state, attempt);
        if !dry_run {
            store.backend.delete(&name)?;
        }
        swept.trees.push(name);
    }
    sweep_chunks(store, &identity, &descriptor.recipient, &live_chunks, &kept_chunks, dead_chunks, dry_run, &mut swept)?;
    Ok(swept)
}

/// The chunk side of a pass, after the dead trees are gone (see the module comment).
#[allow(clippy::too_many_arguments)]
fn sweep_chunks(
    store: &Store,
    identity: &str,
    recipient: &str,
    live: &BTreeSet<String>,
    kept: &[BTreeSet<String>],
    mut dead: BTreeSet<String>,
    dry_run: bool,
    swept: &mut Swept,
) -> Result<()> {
    let objects = store.backend.list("chunks/")?;
    if objects.is_empty() {
        swept.missing = kept.iter().filter(|named| !named.is_empty()).count();
        return Ok(());
    }
    let modified: BTreeMap<String, i64> = objects
        .iter()
        .filter_map(|o| layr_store::chunks::parse_name(&o.key).filter(|(_, index)| *index).map(|(pack, _)| (pack, o.modified)))
        .collect();
    let indexes = super::chunked::indexes(&store.backend, Some(&store.dir), identity)?;
    let now = chrono::Utc::now().timestamp();
    let mut held: BTreeSet<&str> = BTreeSet::new();
    for (pack, index) in &indexes {
        held.extend(index.chunks.iter().map(|(id, _, _)| id.as_str()));
        // A pack of a push that stopped before its tree: old, and nothing live in it.
        let old = modified.get(pack).is_some_and(|time| now - time > PACK_GRACE);
        if old && !index.chunks.iter().any(|(id, _, _)| live.contains(id)) {
            dead.extend(index.chunks.iter().map(|(id, _, _)| id.clone()));
        }
    }
    swept.missing = kept.iter().filter(|named| named.iter().any(|id| !held.contains(id.as_str()))).count();
    for index in indexes.values() {
        for (id, _, length) in &index.chunks {
            if dead.contains(id) {
                swept.chunks += 1;
                swept.chunk_bytes += length;
            }
        }
    }
    if dead.is_empty() {
        return Ok(());
    }
    if dry_run {
        swept.packs.extend(
            indexes
                .iter()
                .filter(|(_, index)| index.chunks.iter().any(|(id, _, _)| dead.contains(id)))
                .flat_map(|(pack, _)| [layr_store::chunks::index_name(pack), layr_store::chunks::pack_name(pack)]),
        );
        return Ok(());
    }
    for key in super::chunked::release(store, identity, recipient, &dead)? {
        store.backend.delete(&key)?;
        swept.packs.push(key);
    }
    Ok(())
}
