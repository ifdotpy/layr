//! `layr objects`: the experimental content-addressed object store (docs/objects.md). It reads
//! states and writes only `meta/objects` of the project and the folder of `checkout`; no other
//! command reads the object store yet.

use super::{exit, here, Here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use anyhow::{bail, Result};
use layr_objects::{Id, StateRef, Store, Temperature, WriteOptions, WriteReport};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Instant;

const USAGE: &str = "usage: layr objects import <rev> [--cold] [--full] [--parent <tree>] [--json]
       layr objects verify <rev> [--cold] [--full] [--json]
       layr objects repack [--json]
       layr objects stat [--json]
       layr objects checkout <tree or rev> <folder>
Experimental: a content-addressed store of trees, blobs, deltas and chunks (docs/objects.md).";

pub fn objects(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let (sub, rest) = match args.split_first() {
        Some((sub, rest)) => (sub.as_str(), rest),
        None => return Err(exit(128, USAGE)),
    };
    match sub {
        "import" => import(ctx, rest),
        "verify" => verify(ctx, rest),
        "stat" => stat(ctx, rest),
        "checkout" => checkout(ctx, rest),
        "repack" | "pack" => repack(ctx, rest),
        _ => Err(exit(128, USAGE)),
    }
}

fn open(h: &Here) -> Result<Store> {
    crate::objstore::open(&h.repo.p)
}

pub(crate) struct Imported {
    pub state: String,
    pub tree: Id,
    pub times: Id,
    pub report: WriteReport,
    pub previous: Option<Id>,
    /// The paths read again from the first parent's tree (`None`: a full write).
    pub changed: Option<usize>,
    pub seconds: f64,
}

/// Write the tree of the state `rev`. When the state's first parent was imported, only the paths
/// that changed since are read (`write_changed`; `--full` reads all). A cold write takes the
/// parent's tree as the delta base (`--parent` names another).
pub(crate) fn import_state(h: &Here, store: &Store, rev: &str, cold: bool, full: bool, parent: Option<Id>) -> Result<Imported> {
    let started = Instant::now();
    let id = crate::revs::resolve(&h.repo, h.line.as_ref(), rev)?;
    let state = h.repo.source_state(&id)?;
    let first = state.parents.first().cloned();
    let base = match &first {
        Some(p) if !full => store.state_ref(p)?.map(|r| (p.clone(), r)),
        _ => None,
    };
    let pin = h.repo.pin(&[&id])?;
    let source = h.repo.state_src(&id)?;
    let root = crate::objstore::SnapshotSource(&source);
    let previous = match parent {
        Some(tree) => Some(tree),
        None if cold => base.as_ref().map(|(_, r)| r.tree).or(match &first {
            Some(p) => store.state_tree(p)?,
            None => None,
        }),
        None => None,
    };
    let temperature = if cold { Temperature::Cold } else { Temperature::Hot };
    let options = WriteOptions { temperature, previous, exclude: Vec::new(), similar: false };
    let incremental = if full { None } else { crate::objstore::write_base(&h.repo, store, &id, first.as_deref(), &source)? };
    let (report, changed) = match incremental {
        Some((r, paths)) => {
            let base = layr_objects::write::Base { tree: r.tree, times: r.times };
            let n = paths.len();
            (layr_objects::write::write_changed_from(store, &root, &base, &paths, &options)?, Some(n))
        }
        None => (layr_objects::write::write_tree_from(store, &root, &options)?, None),
    };
    drop(pin);
    let tree = report.tree.expect("a write gives a tree");
    let times = report.times.expect("a write gives times");
    store.set_state(&id, &StateRef { tree, times })?;
    Ok(Imported { state: id, tree, times, report, previous, changed, seconds: started.elapsed().as_secs_f64() })
}

fn imported_json(store: &Store, i: &Imported) -> serde_json::Value {
    json!({
        "state": i.state,
        "tree": i.tree.hex(),
        "times": i.times.hex(),
        "previous": i.previous.map(|p| p.hex()),
        "changed_paths": i.changed,
        "files": i.report.files,
        "folders": i.report.dirs,
        "symlinks": i.report.symlinks,
        "bytes": i.report.bytes,
        "hard_link_groups": i.report.hard_link_groups,
        "new_objects": store.added.objects.load(Ordering::Relaxed),
        "new_bytes": store.added.bytes.load(Ordering::Relaxed),
        "clones": store.added.clones.load(Ordering::Relaxed),
        "seconds": i.seconds,
    })
}

/// Import and verify write into the store of the project (all of a state on a file system
/// without clones): the experimental commands are for admins.
fn require_admin(ctx: &Ctx, h: &Here) -> Result<()> {
    if !h.repo.is_admin(ctx) {
        bail!("permission denied: layr objects import and verify are experimental and need the admin role");
    }
    Ok(())
}

fn import(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--cold").flag("--full").flag("--json").value("--parent"), args)?;
    if a.pos.len() != 1 {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    require_admin(ctx, &h)?;
    let store = open(&h)?;
    let parent = a.get("--parent").map(Id::parse).transpose()?;
    let i = import_state(&h, &store, &a.pos[0], a.has("--cold"), a.has("--full"), parent)?;
    if a.has("--json") {
        outln!(ctx, "{}", imported_json(&store, &i));
    } else {
        outln!(ctx, "{}", i.tree);
        outln!(
            ctx,
            "state {}{}: {} files read, {} folders, {} links, {} bytes; {} new objects ({} bytes, {} clones) in {:.3} s",
            ids::short(&i.state),
            i.changed.map(|n| format!(" ({n} changed paths since the parent)")).unwrap_or_default(),
            i.report.files,
            i.report.dirs,
            i.report.symlinks,
            i.report.bytes,
            store.added.objects.load(Ordering::Relaxed),
            store.added.bytes.load(Ordering::Relaxed),
            store.added.clones.load(Ordering::Relaxed),
            i.seconds
        );
    }
    Ok(0)
}

/// Import a state, make a folder from its tree next to the store, and compare every entry and
/// byte of that folder with the state.
fn verify(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--cold").flag("--full").flag("--json"), args)?;
    if a.pos.len() != 1 {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    require_admin(ctx, &h)?;
    let store = open(&h)?;
    let i = import_state(&h, &store, &a.pos[0], a.has("--cold"), a.has("--full"), None)?;
    let (made, cmp, checkout_seconds, compare_seconds) =
        compare_checkout(&h, &store, &i.state, &StateRef { tree: i.tree, times: i.times })?;
    if a.has("--json") {
        let mut v = imported_json(&store, &i);
        v["checkout"] = json!({"files": made.files, "clones": made.clones, "copies": made.copies,
            "decoded": made.decoded, "seconds": checkout_seconds});
        v["compared"] = json!({"entries": cmp.entries, "bytes": cmp.bytes, "seconds": compare_seconds});
        v["differences"] = json!(cmp.differences);
        outln!(ctx, "{v}");
    } else {
        outln!(ctx, "{}", i.tree);
        outln!(
            ctx,
            "import {:.2} s, checkout {:.2} s ({} clones, {} copies, {} decoded), compare {:.2} s: {} entries, {} bytes",
            i.seconds,
            checkout_seconds,
            made.clones,
            made.copies,
            made.decoded,
            compare_seconds,
            cmp.entries,
            cmp.bytes
        );
        for d in &cmp.differences {
            outln!(ctx, "\t{d}");
        }
        if cmp.differences.is_empty() {
            outln!(
                ctx,
                "verified: the tree and times give state {} entry for entry, byte for byte and time for time",
                ids::short(&i.state)
            );
        }
    }
    Ok(if cmp.differences.is_empty() { 0 } else { 1 })
}

/// Check out the tree and times of `state` into a new folder of the project's work folder and
/// compare every entry, byte, mode, extended attribute, hard link group and time with the
/// snapshot of the state. The copy is removed. Returns the checkout, the comparison and the
/// seconds of each step.
pub(crate) fn compare_checkout(
    h: &Here,
    store: &Store,
    state: &str,
    r: &StateRef,
) -> Result<(layr_objects::CheckoutReport, layr_objects::Comparison, f64, f64)> {
    let started = Instant::now();
    let work = h.repo.p.work_dir();
    std::fs::create_dir_all(&work)?;
    let tmp = crate::repo::Temp { path: work.join(crate::store::temp_name()) };
    let made = layr_objects::checkout_with(store, &r.tree, Some(&r.times), &tmp.path)?;
    let checkout_seconds = started.elapsed().as_secs_f64();
    let pin = h.repo.pin(&[state])?;
    let cmp = layr_objects::compare_with(&h.repo.state_root(state)?, &tmp.path, true);
    drop(pin);
    let compare_seconds = started.elapsed().as_secs_f64() - checkout_seconds;
    // Read-only folders of the copy must open again before the copy goes.
    make_writable(&tmp.path);
    drop(tmp);
    Ok((made, cmp?, checkout_seconds, compare_seconds))
}

fn make_writable(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(m) = std::fs::symlink_metadata(dir) else { return };
    if !m.is_dir() {
        return;
    }
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode((m.permissions().mode() & 0o7777) | 0o700));
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            make_writable(&e.path());
        }
    }
}

fn stat(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    if !a.pos.is_empty() {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    h.repo.can_read(ctx)?;
    let s = open(&h)?.stat()?;
    if a.has("--json") {
        outln!(
            ctx,
            "{}",
            json!({
                "format": layr_objects::store::FORMAT_VERSION,
                "states": s.states, "trees": s.trees, "tree_bytes": s.tree_bytes,
                "hot_blobs": s.hot_blobs, "hot_bytes": s.hot_bytes,
                "whole_blobs": s.whole_blobs, "whole_bytes": s.whole_bytes,
                "delta_blobs": s.delta_blobs, "delta_bytes": s.delta_bytes,
                "chunked_blobs": s.chunked_blobs, "chunked_bytes": s.chunked_bytes,
                "chunks": s.chunks, "chunk_bytes": s.chunk_bytes, "times": s.times, "times_bytes": s.times_bytes,
                "packs": s.packs, "pack_bytes": s.pack_bytes, "packed_objects": s.packed_objects,
                "packed_plain_bytes": s.packed_plain_bytes, "loose_bytes": s.loose_bytes(), "total_bytes": s.total_bytes(),
            })
        );
    } else {
        outln!(ctx, "storage        object store v{}", layr_objects::store::FORMAT_VERSION);
        outln!(ctx, "states         {:>10}", s.states);
        outln!(ctx, "trees          {:>10} {:>14} bytes", s.trees, s.tree_bytes);
        outln!(ctx, "hot blobs      {:>10} {:>14} bytes (whole, cloned)", s.hot_blobs, s.hot_bytes);
        outln!(ctx, "whole blobs    {:>10} {:>14} bytes (zstd)", s.whole_blobs, s.whole_bytes);
        outln!(ctx, "delta blobs    {:>10} {:>14} bytes", s.delta_blobs, s.delta_bytes);
        outln!(ctx, "chunked blobs  {:>10} {:>14} bytes (chunk lists)", s.chunked_blobs, s.chunked_bytes);
        outln!(ctx, "chunks         {:>10} {:>14} bytes", s.chunks, s.chunk_bytes);
        outln!(ctx, "times tables   {:>10} {:>14} bytes", s.times, s.times_bytes);
        outln!(
            ctx,
            "packs          {:>10} {:>14} bytes ({} objects, {} plain bytes)",
            s.packs,
            s.pack_bytes,
            s.packed_objects,
            s.packed_plain_bytes
        );
        outln!(ctx, "total                     {:>14} bytes", s.total_bytes());
    }
    Ok(0)
}

/// The states whose blobs stay hot: the head of each line and the states before it on its first
/// parent chain, `objects.hot-states` in all (default 5, the size of the undo ring).
pub const HOT_STATES_DEFAULT: u64 = 5;

pub(crate) fn hot_states(h: &Here) -> Result<std::collections::BTreeSet<String>> {
    let n = h.repo.p.config_u64("objects.hot-states", HOT_STATES_DEFAULT);
    let mut out = std::collections::BTreeSet::new();
    for line in h.repo.lines()? {
        let mut state = Some(line.head.clone());
        for _ in 0..n {
            let Some(id) = state else { break };
            state = h.repo.p.db.state(&id)?.and_then(|s| s.parents.first().cloned());
            out.insert(id);
        }
    }
    Ok(out)
}

fn hot_trees(h: &Here, store: &Store) -> Result<Vec<Id>> {
    let mut out = Vec::new();
    for id in hot_states(h)? {
        if let Some(tree) = store.state_tree(&id)? {
            out.push(tree);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Turn hot blobs that no hot state names into cold objects, then pack every loose object
/// (`Store::pack`: zstd blocks, docs/objects.md "Packs"). `layr gc` runs it too.
pub fn repack_store(h: &Here) -> Result<(layr_objects::store::EvictReport, layr_objects::store::PackReport, f64)> {
    let started = Instant::now();
    let store = open(h)?;
    let evicted = store.evict_hot(&hot_trees(h, &store)?)?;
    let packed = store.pack()?;
    Ok((evicted, packed, started.elapsed().as_secs_f64()))
}

fn repack(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    if !a.pos.is_empty() {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    require_admin(ctx, &h)?;
    let (e, p, seconds) = repack_store(&h)?;
    if a.has("--json") {
        outln!(
            ctx,
            "{}",
            json!({"evicted_blobs": e.blobs, "evicted_bytes": e.bytes, "packs": p.packs, "packed_objects": p.objects,
                "removed_loose": p.removed, "loose_bytes": p.loose_bytes, "pack_bytes": p.pack_bytes, "seconds": seconds})
        );
    } else {
        outln!(
            ctx,
            "repacked: {} hot blobs turned cold ({} bytes); {} objects ({} loose bytes) into {} packs of {} bytes in {:.2} s",
            e.blobs,
            e.bytes,
            p.objects,
            p.loose_bytes,
            p.packs,
            p.pack_bytes,
            seconds
        );
    }
    Ok(0)
}

fn checkout(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    if a.pos.len() != 2 {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    h.repo.require(ctx, "export")?;
    // On a root service every write of the checkout takes the caller's file identity, so the
    // files belong to the caller and root never writes into the caller's folder.
    let caller = crate::privs::CallerFs::of(ctx)?;
    let store = open(&h)?;
    let (refs, state) = match Id::parse(&a.pos[0]) {
        Ok(tree) => match store.states_of(&tree)?.into_iter().next() {
            Some(state) => (store.state_ref(&state)?.expect("a listed state"), state),
            None => bail!("tree {tree} is not the tree of an imported state"),
        },
        Err(_) => {
            let state = crate::revs::resolve(&h.repo, h.line.as_ref(), &a.pos[0])?;
            match store.state_ref(&state)? {
                Some(r) => (r, state),
                None => bail!("state {} is not imported: run layr objects import {}", ids::short(&state), a.pos[0]),
            }
        }
    };
    let tree = refs.tree;
    // The same rule as `layr export`: a folder outside the store gets no secret file.
    crate::secrets::check_contents(ctx, &h.repo, &state)?;
    let dst = if a.pos[1].starts_with('/') { std::path::PathBuf::from(&a.pos[1]) } else { ctx.cwd.join(&a.pos[1]) };
    let started = Instant::now();
    let r = match &caller {
        Some(c) => layr_objects::checkout_as(&store, &tree, Some(&refs.times), &dst, &|step| c.run(step))?,
        None => layr_objects::checkout_with(&store, &tree, Some(&refs.times), &dst)?,
    };
    outln!(
        ctx,
        "checked out tree {} to {}: {} files ({} clones, {} copies, {} decoded), {} folders, {} links in {:.2} s",
        &tree.hex()[..12],
        dst.display(),
        r.files,
        r.clones,
        r.copies,
        r.decoded,
        r.dirs,
        r.symlinks,
        started.elapsed().as_secs_f64()
    );
    Ok(0)
}
