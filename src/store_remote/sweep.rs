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

use super::format::{parse_data_name, parse_tree_name, tree_name};
use super::reader::Reader;
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
    /// Dead chunks that wait for a later pass (`CHUNK_GRACE`).
    pub chunks_waiting: usize,
}

/// A pack index younger than this may belong to a push in flight.
const PACK_GRACE: i64 = 24 * 3600;

/// A dead chunk is released only when a pass of this machine found it dead at least this long before and finds it
/// dead again. A writer that reused the chunk in a push in flight (its tree is not listed yet) names it in a tree
/// before then, and the chunk is live at the next pass. The test hook `LAYR_TEST_CHUNK_GRACE` (seconds) sets it.
const CHUNK_GRACE: i64 = 2 * 3600;

fn chunk_grace() -> i64 {
    std::env::var("LAYR_TEST_CHUNK_GRACE").ok().and_then(|v| v.parse().ok()).unwrap_or(CHUNK_GRACE)
}

/// The dead chunks that passes of this machine found, with the time each was first found dead (`dead-chunks` in
/// the layer store, `<chunk> <unix time>` lines).
fn dead_marks(dir: &std::path::Path) -> BTreeMap<String, i64> {
    std::fs::read_to_string(dir.join("dead-chunks"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' ').and_then(|(id, t)| Some((id.to_string(), t.parse().ok()?))))
        .collect()
}

/// Delete the dead cache trees of the layer `store` of a cache overlay: the trees of states that the
/// overlay's records know (`known`) and do not keep (`kept`), and their parts that no kept tree links.
pub(super) fn sweep(
    store: &Store,
    local: &[String],
    kept: &BTreeSet<String>,
    known: &dyn Fn(&str) -> bool,
    parent: &dyn Fn(&str) -> Option<String>,
    dry_run: bool,
) -> Result<Swept> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    // The keys of the signers: the segments that a fetch or an earlier sweep checked are not read again.
    let records = super::reader::records_with_local(
        &store.backend,
        &identity,
        &descriptor.project,
        local,
        Some(&super::reader::seen_file(&store.dir)),
    )?;
    let objects = store.backend.list("states/")?;
    let trees: Vec<(String, String)> = objects.iter().filter_map(|o| parse_tree_name(&o.key)).collect();
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    let mut swept = Swept::default();
    let mut keep: BTreeSet<(String, String)> = BTreeSet::new();
    // `LAYR_TRACE=1`: why each tree stays.
    let why = |state: &str, reason: String| {
        if std::env::var_os("LAYR_TRACE").is_some() {
            eprintln!("trace sweep keeps {}: {reason}", crate::ids::short(state));
        }
    };
    for (state, attempt) in &trees {
        if !known(state) {
            swept.unknown += 1;
            why(state, "not known here".into());
            keep.insert((state.clone(), attempt.clone()));
        } else if kept.contains(state) {
            why(state, "live".into());
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
                why(&id, format!("nearest pushed ancestor of live state {} whose tree is not on the remote", crate::ids::short(state)));
                keep.extend(trees.iter().filter(|(s, _)| *s == id).cloned());
                break;
            }
            at = parent(&id);
        }
    }
    // A kept delta tree needs its base trees to decode (a tree is a zstd delta on its base tree), but not their
    // data: the parts and chunks that stay are the ones that the trees kept for their own state link.
    // A tree that cannot be decoded (a delta whose base tree is gone) stays as it is: the sweep cannot tell what it
    // links, so it deletes nothing for it, and it never fails a push for it.
    let mut broken: BTreeSet<(String, String)> = BTreeSet::new();
    let own = keep.clone();
    let mut pending: Vec<(String, String)> = keep.iter().cloned().collect();
    while let Some((state, attempt)) = pending.pop() {
        let header = match reader.tree(&state, &attempt) {
            Ok((header, _, _)) => header,
            Err(e) => {
                eprintln!("layr: remote tree {} cannot be read ({e:#}); the sweep leaves it", tree_name(&state, &attempt));
                broken.insert((state, attempt));
                continue;
            }
        };
        if let Some(base) = header.base {
            if keep.insert(base.clone()) {
                why(&base.0, format!("base tree of {}", crate::ids::short(&state)));
                pending.push(base);
            }
        }
    }
    let mut linked = BTreeSet::new();
    let mut live_chunks: BTreeSet<String> = BTreeSet::new();
    let mut kept_chunks: Vec<BTreeSet<String>> = Vec::new();
    for (state, attempt) in own.iter().filter(|t| !broken.contains(*t)) {
        let Ok((_, _, tree)) = reader.tree(state, attempt) else { continue };
        linked.extend(tree.files.values().flat_map(|e| e.3.iter().map(|l| l.object())));
        let named: BTreeSet<String> = tree.chunks.values().flatten().cloned().collect();
        live_chunks.extend(named.iter().cloned());
        kept_chunks.push(named);
    }
    // The data of a tree that stays only as a base: its parts go unless a tree of `own` links them.
    let bases: Vec<&(String, String)> = keep.iter().filter(|t| !own.contains(*t)).collect();
    let dead: Vec<&(String, String)> = trees.iter().filter(|t| !keep.contains(*t)).collect();
    let mut parts = BTreeSet::new();
    let mut frames = BTreeSet::new();
    let mut dead_chunks: BTreeSet<String> = BTreeSet::new();
    for (state, attempt) in dead.iter().copied().chain(bases.iter().copied()) {
        let tree = match reader.tree(state, attempt) {
            Ok((_, _, tree)) => tree,
            Err(e) => {
                eprintln!("layr: remote tree {} cannot be read ({e:#}); the sweep leaves it and its data", tree_name(state, attempt));
                broken.insert(((*state).clone(), (*attempt).clone()));
                continue;
            }
        };
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
    // Parts of an attempt that published no tree (a push that stopped) and that no kept tree links. After 24 hours no
    // writer can still publish a tree that links them: writers stop at +23 h, as in `gc --remote`.
    let cutoff = chrono::Utc::now().timestamp() - 86400;
    let with_tree: BTreeSet<&(String, String)> = trees.iter().collect();
    for object in &objects {
        let Some((state, attempt, _)) = parse_data_name(&object.key) else { continue };
        if object.modified < cutoff && !linked.contains(&object.key) && !with_tree.contains(&(state, attempt)) {
            parts.insert(object.key.clone());
        }
    }
    for part in parts {
        if !dry_run {
            store.backend.delete(&part)?;
        }
        swept.parts.push(part);
    }
    for (state, attempt) in dead.into_iter().filter(|t| !broken.contains(*t)) {
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
    // An index whose pack data is gone names chunks that nobody can read: it goes now. An upload writes the data
    // before the index, so no push in flight has such an index.
    for pack in super::chunked::listing(&store.backend)?.1 {
        let name = layr_store::chunks::index_name(&pack);
        eprintln!("layr: chunk pack {pack} has an index but no data; the index goes");
        if !dry_run {
            store.backend.delete(&name)?;
        }
        swept.packs.push(name);
    }
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
    // Only the chunks that an earlier pass found dead `chunk_grace` ago go now; the others are marked for a later
    // pass. A chunk that is live again loses its mark.
    if !dry_run {
        let marks = dead_marks(&store.dir);
        // A marked chunk whose tree an earlier pass deleted is still dead while no live tree names it and a pack
        // holds it.
        dead.extend(marks.keys().filter(|id| !live.contains(*id) && held.contains(id.as_str())).cloned());
        let now_marks: BTreeMap<String, i64> = dead.iter().map(|id| (id.clone(), marks.get(id).copied().unwrap_or(now))).collect();
        if now_marks != marks {
            let text: String = now_marks.iter().map(|(id, t)| format!("{id} {t}\n")).collect();
            crate::fsutil::atomic_write(&store.dir.join("dead-chunks"), text.as_bytes(), 0o600)?;
        }
        let grace = chunk_grace();
        let young = now_marks.iter().filter(|(_, t)| now - **t < grace).count();
        swept.chunks_waiting = young;
        dead.retain(|id| now_marks.get(id).is_some_and(|t| now - t >= grace));
        swept.chunks = 0;
        swept.chunk_bytes = 0;
        for index in indexes.values() {
            for (id, _, length) in &index.chunks {
                if dead.contains(id) {
                    swept.chunks += 1;
                    swept.chunk_bytes += length;
                }
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
