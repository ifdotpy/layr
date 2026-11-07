//! `layr objects`: the experimental content-addressed object store (docs/objects.md). It reads
//! states and writes only `meta/objects` of the project and the folder of `checkout`; no other
//! command reads the object store yet.

use super::{exit, here, Here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use anyhow::{bail, Result};
use layr_objects::{Id, Keying, Store, Temperature, WriteOptions, WriteReport};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Instant;

const USAGE: &str = "usage: layr objects import <rev> [--cold] [--parent <tree>] [--json]
       layr objects verify <rev> [--cold] [--json]
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
        _ => Err(exit(128, USAGE)),
    }
}

fn open(h: &Here) -> Result<Store> {
    // Plain ids for every project in this slice; Secret projects get keyed ids when the project
    // key is available here (docs/objects.md, "Secret projects").
    #[allow(unused_mut)]
    let mut store = Store::open(&h.repo.p.meta_dir().join("objects"), Keying::Plain)?;
    // A sealed APFS state has immutable files, and a clone keeps the flag.
    #[cfg(target_os = "macos")]
    {
        store.clone_hook = Some(crate::sys::apfs_unseal_fd);
    }
    Ok(store)
}

struct Imported {
    state: String,
    tree: Id,
    report: WriteReport,
    previous: Option<Id>,
    seconds: f64,
}

/// Write the tree of the state `rev`. A cold write takes the tree of the state's first parent,
/// when it was imported, as the delta base (`--parent` names another).
fn import_state(h: &Here, store: &Store, rev: &str, cold: bool, parent: Option<Id>) -> Result<Imported> {
    let started = Instant::now();
    let id = crate::revs::resolve(&h.repo, h.line.as_ref(), rev)?;
    let state = h.repo.source_state(&id)?;
    let pin = h.repo.pin(&[&id])?;
    let root = h.repo.state_root(&id)?;
    let previous = match parent {
        Some(tree) => Some(tree),
        None if cold => match state.parents.first() {
            Some(p) => store.state_tree(p)?,
            None => None,
        },
        None => None,
    };
    let temperature = if cold { Temperature::Cold } else { Temperature::Hot };
    let report = layr_objects::write_tree(store, &root, &WriteOptions { temperature, previous, exclude: Vec::new() })?;
    drop(pin);
    let tree = report.tree.expect("a write gives a tree");
    store.set_state(&id, &tree)?;
    Ok(Imported { state: id, tree, report, previous, seconds: started.elapsed().as_secs_f64() })
}

fn imported_json(store: &Store, i: &Imported) -> serde_json::Value {
    json!({
        "state": i.state,
        "tree": i.tree.hex(),
        "previous": i.previous.map(|p| p.hex()),
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
    let a = parse(&Spec::new().flag("--cold").flag("--json").value("--parent"), args)?;
    if a.pos.len() != 1 {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    require_admin(ctx, &h)?;
    let store = open(&h)?;
    let parent = a.get("--parent").map(Id::parse).transpose()?;
    let i = import_state(&h, &store, &a.pos[0], a.has("--cold"), parent)?;
    if a.has("--json") {
        outln!(ctx, "{}", imported_json(&store, &i));
    } else {
        outln!(ctx, "{}", i.tree);
        outln!(
            ctx,
            "state {}: {} files, {} folders, {} links, {} bytes; {} new objects ({} bytes, {} clones) in {:.2} s",
            ids::short(&i.state),
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
    let a = parse(&Spec::new().flag("--cold").flag("--json"), args)?;
    if a.pos.len() != 1 {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    require_admin(ctx, &h)?;
    let store = open(&h)?;
    let i = import_state(&h, &store, &a.pos[0], a.has("--cold"), None)?;
    let started = Instant::now();
    let work = h.repo.p.work_dir();
    std::fs::create_dir_all(&work)?;
    let tmp = crate::repo::Temp { path: work.join(crate::store::temp_name()) };
    let made = layr_objects::checkout(&store, &i.tree, &tmp.path)?;
    let checkout_seconds = started.elapsed().as_secs_f64();
    let pin = h.repo.pin(&[&i.state])?;
    let cmp = layr_objects::compare(&h.repo.state_root(&i.state)?, &tmp.path)?;
    drop(pin);
    let compare_seconds = started.elapsed().as_secs_f64() - checkout_seconds;
    // Read-only folders of the copy must open again before the copy goes.
    make_writable(&tmp.path);
    drop(tmp);
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
            outln!(ctx, "verified: the tree gives state {} entry for entry and byte for byte", ids::short(&i.state));
        }
    }
    Ok(if cmp.differences.is_empty() { 0 } else { 1 })
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
                "states": s.states, "trees": s.trees, "tree_bytes": s.tree_bytes,
                "hot_blobs": s.hot_blobs, "hot_bytes": s.hot_bytes,
                "whole_blobs": s.whole_blobs, "whole_bytes": s.whole_bytes,
                "delta_blobs": s.delta_blobs, "delta_bytes": s.delta_bytes,
                "chunked_blobs": s.chunked_blobs, "chunked_bytes": s.chunked_bytes,
                "chunks": s.chunks, "chunk_bytes": s.chunk_bytes, "total_bytes": s.total_bytes(),
            })
        );
    } else {
        outln!(ctx, "states         {:>10}", s.states);
        outln!(ctx, "trees          {:>10} {:>14} bytes", s.trees, s.tree_bytes);
        outln!(ctx, "hot blobs      {:>10} {:>14} bytes (whole, cloned)", s.hot_blobs, s.hot_bytes);
        outln!(ctx, "whole blobs    {:>10} {:>14} bytes (zstd)", s.whole_blobs, s.whole_bytes);
        outln!(ctx, "delta blobs    {:>10} {:>14} bytes", s.delta_blobs, s.delta_bytes);
        outln!(ctx, "chunked blobs  {:>10} {:>14} bytes (chunk lists)", s.chunked_blobs, s.chunked_bytes);
        outln!(ctx, "chunks         {:>10} {:>14} bytes", s.chunks, s.chunk_bytes);
        outln!(ctx, "total                     {:>14} bytes", s.total_bytes());
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
    // The service runs as root on Linux; until the checkout writes as the caller, it writes only
    // for root (docs/objects.md, "Open questions").
    if rustix::process::geteuid().is_root() && !ctx.caller.is_root() {
        bail!("layr objects checkout is experimental and writes only for root on a root service");
    }
    let store = open(&h)?;
    let (tree, state) = match Id::parse(&a.pos[0]) {
        Ok(tree) => match store.states_of(&tree)?.into_iter().next() {
            Some(state) => (tree, state),
            None => bail!("tree {tree} is not the tree of an imported state"),
        },
        Err(_) => {
            let state = crate::revs::resolve(&h.repo, h.line.as_ref(), &a.pos[0])?;
            match store.state_tree(&state)? {
                Some(tree) => (tree, state),
                None => bail!("state {} is not imported: run layr objects import {}", ids::short(&state), a.pos[0]),
            }
        }
    };
    // The same rule as `layr export`: a folder outside the store gets no secret file.
    crate::secrets::check_contents(ctx, &h.repo, &state)?;
    let dst = if a.pos[1].starts_with('/') { std::path::PathBuf::from(&a.pos[1]) } else { ctx.cwd.join(&a.pos[1]) };
    let started = Instant::now();
    let r = layr_objects::checkout(&store, &tree, &dst)?;
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
