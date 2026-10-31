//! `layr du`: the disk space of each line, and the part of it that only that line holds.
use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::fsutil::Space;
use anyhow::{bail, Result};

/// Lines read at the same time: the walks read only metadata, so a few in parallel are enough.
const WORKERS: usize = 4;

pub fn du(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    let h = here(ctx)?;
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
