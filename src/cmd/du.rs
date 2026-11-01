//! `layr du`: the disk space of each line, and the part of it that only that line holds.
use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::fsutil::Space;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

/// Lines read at the same time: the walks read only metadata, so a few in parallel are enough.
const WORKERS: usize = 4;

pub fn du(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").flag("--store"), args)?;
    let h = here(ctx)?;
    if a.has("--store") {
        if !a.pos.is_empty() {
            bail!("usage: layr du --store [--json]");
        }
        return store(ctx, &h.repo.p.store.root, a.has("--json"));
    }
    let repo = &h.repo;
    let mut lines = Vec::new();
    for l in repo.lines()? {
        if a.pos.is_empty() || a.pos.contains(&l.name) {
            let path = repo.working(&l);
            if path.is_dir() {
                lines.push((l.name, path));
            }
        }
    }
    for name in &a.pos {
        if !lines.iter().any(|(n, _)| n == name) {
            bail!("no line '{name}' with a folder on this machine");
        }
    }
    // No store lock: the walks only read the line folders.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Result<Space>>>> = lines.iter().map(|_| Default::default()).collect();
    std::thread::scope(|s| {
        for _ in 0..WORKERS.min(lines.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((_, path)) = lines.get(i) else { break };
                *results[i].lock().unwrap() = Some(crate::storage::space(path));
            });
        }
    });
    let mut rows = Vec::new();
    for ((name, path), r) in lines.into_iter().zip(results) {
        match r.into_inner().unwrap().expect("every line was read") {
            Ok(space) => rows.push((name, path, space)),
            Err(e) => eprintln!("warning: line '{name}': {e:#}"),
        }
    }
    rows.sort_by(|x, y| y.2.unique.cmp(&x.2.unique).then_with(|| x.0.cmp(&y.0)));
    if a.has("--json") {
        let v: Vec<_> = rows
            .iter()
            .map(|(name, path, s)| serde_json::json!({"line": name, "path": path, "total": s.total, "unique": s.unique}))
            .collect();
        outln!(ctx, "{}", serde_json::to_string(&v)?);
        return Ok(0);
    }
    let size = super::admin::human_size;
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max(4);
    outln!(ctx, "{:<width$}  {:>10}  {:>10}", "line", "unique", "total");
    for (name, _, s) in &rows {
        outln!(ctx, "{:<width$}  {:>10}  {:>10}", name, size(s.unique), size(s.total));
    }
    let unique: u64 = rows.iter().map(|r| r.2.unique).sum();
    outln!(ctx, "{:<width$}  {:>10}", "all lines", size(unique));
    outln!(ctx, "unique: bytes that only this line holds; deleting the line frees them.");
    Ok(0)
}

/// The parts of a store: each line folder, each other folder of a project (states, deltas, git,
/// meta), and each folder at the top (trash).
fn parts(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Result<Vec<(String, PathBuf)>> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(p)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                v.push((e.file_name().to_string_lossy().into_owned(), e.path()));
            }
        }
        v.sort();
        Ok(v)
    };
    for (name, path) in dirs(root)? {
        // The OverlayFS registry holds layers (a part of their own) and the upper layers of
        // lines, which belong to the lines.
        if name == ".layr-overlayfs" {
            out.push(("overlayfs layers".into(), path));
            continue;
        }
        if name != "projects" {
            out.push((name, path));
            continue;
        }
        let moved = crate::linepaths::index(root);
        for (project, ppath) in dirs(&path)? {
            // Lines moved out of the store (`layr line move`) are listed with the others.
            for e in moved.iter().filter(|e| e.project == project && e.path.is_dir()) {
                let db = crate::db::Db::open(&crate::store::db_file(&ppath));
                if let Some(l) = db.ok().and_then(|db| db.line_by_id(&e.line).ok().flatten()) {
                    out.push((format!("{project}/lines/{}", l.name), e.path.clone()));
                }
            }
            for (part, path) in dirs(&ppath)? {
                if part == "lines" {
                    for (line, path) in dirs(&path)? {
                        out.push((format!("{project}/lines/{line}"), path));
                    }
                } else {
                    out.push((format!("{project}/{part}"), path));
                }
            }
        }
    }
    Ok(out)
}

/// What one part holds alone: bytes that no other file shares, and shared data that no other
/// part of the store references.
#[derive(Default)]
struct PartSpace {
    own: u64,
    sole: u64,
}

/// `layr du --store`: the space of every part of the store, and what deleting each one frees.
/// Data that clones share counts once, under its key (`fsutil::Held`): it belongs to a part
/// when no other part references it.
fn store(ctx: &Ctx, root: &Path, json: bool) -> Result<i32> {
    const SHARED: usize = usize::MAX;
    let parts = parts(root)?;
    // Work units: a part with only folders at its top (states, deltas: one per state) splits into
    // them, so that the workers share a large part.
    let mut units: Vec<(usize, PathBuf)> = Vec::new();
    for (i, (name, path)) in parts.iter().enumerate() {
        if name == "overlayfs layers" {
            for e in std::fs::read_dir(path.join("objects"))? {
                let data = e?.path().join("data");
                if data.is_dir() {
                    units.push((i, data));
                }
            }
            continue;
        }
        let children: Vec<_> = std::fs::read_dir(path)?.collect::<std::io::Result<_>>()?;
        if !children.is_empty() && children.iter().all(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
            units.extend(children.into_iter().map(|e| (i, e.path())));
        } else {
            units.push((i, path.clone()));
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let keys: Mutex<HashMap<u64, (usize, u64)>> = Mutex::new(HashMap::new());
    let results: Vec<Mutex<Result<PartSpace>>> = parts.iter().map(|_| Mutex::new(Ok(PartSpace::default()))).collect();
    std::thread::scope(|s| {
        for _ in 0..WORKERS.min(units.len()) {
            s.spawn(|| loop {
                let u = next.fetch_add(1, Ordering::Relaxed);
                let Some((i, path)) = units.get(u) else { break };
                let i = *i;
                let mut own = 0u64;
                let mut local: HashMap<u64, u64> = HashMap::new();
                let walked = crate::storage::visit_space(path, &mut |h| {
                    own += h.exclusive;
                    for &(k, b) in h.shared {
                        let e = local.entry(k).or_insert(0);
                        *e = (*e).max(b);
                    }
                });
                let mut result = results[i].lock().unwrap();
                match walked {
                    Err(e) => *result = Err(e),
                    Ok(()) => {
                        let mut all = keys.lock().unwrap();
                        for (k, b) in local {
                            let e = all.entry(k).or_insert((i, b));
                            if e.0 != i {
                                e.0 = SHARED;
                            }
                            e.1 = e.1.max(b);
                        }
                        if let Ok(p) = result.as_mut() {
                            p.own += own;
                        }
                    }
                }
            });
        }
    });
    let mut spaces: Vec<Option<PartSpace>> = Vec::new();
    for ((name, _), r) in parts.iter().zip(results) {
        match r.into_inner().unwrap() {
            Ok(v) => spaces.push(Some(v)),
            Err(e) => {
                eprintln!("warning: {name}: {e:#}");
                spaces.push(None);
            }
        }
    }
    let mut shared = 0u64;
    for (owner, bytes) in keys.into_inner().unwrap().into_values() {
        match spaces.get_mut(owner).and_then(Option::as_mut) {
            Some(p) => p.sole += bytes,
            None => shared += bytes,
        }
    }
    let mut rows: Vec<(&str, PartSpace)> =
        parts.iter().zip(spaces).filter_map(|((n, _), s)| s.map(|s| (n.as_str(), s))).collect();
    rows.sort_by(|x, y| (y.1.own + y.1.sole).cmp(&(x.1.own + x.1.sole)).then_with(|| x.0.cmp(y.0)));
    let total = rows.iter().map(|r| r.1.own + r.1.sole).sum::<u64>() + shared;
    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|(n, s)| serde_json::json!({"part": n, "frees": s.own + s.sole}))
            .collect();
        outln!(ctx, "{}", serde_json::to_string(&serde_json::json!({"parts": v, "shared": shared, "total": total}))?);
        return Ok(0);
    }
    let size = super::admin::human_size;
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max("shared by several parts".len());
    outln!(ctx, "{:<width$}  {:>10}", "part", "frees");
    for (n, s) in rows.iter().filter(|r| r.1.own + r.1.sole > 0) {
        outln!(ctx, "{:<width$}  {:>10}", n, size(s.own + s.sole));
    }
    outln!(ctx, "{:<width$}  {:>10}", "shared by several parts", size(shared));
    outln!(ctx, "{:<width$}  {:>10}", "store total", size(total));
    outln!(ctx, "frees: bytes that only this part holds; deleting it frees them.");
    Ok(0)
}
