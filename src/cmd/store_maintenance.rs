//! CLI entry points for remote store maintenance.
use crate::ctx::Ctx;
use crate::store_remote::{open_named, overlays};
use anyhow::{bail, Context, Result};

/// The CLI commands use the same named-remote configuration as push and fetch.
pub fn command(ctx: &Ctx, operation: &str, args: &[String]) -> Result<i32> {
    let args = &crate::cmd::overlay_args(operation, args, &[("--caches", "--overlays")])?;
    let spec = crate::args::Spec::new()
        .value("--remote")
        .flag("--files")
        .flag("--json")
        .flag("-n|--dry-run")
        .flag("-q|--quiet")
        .flag("--caches");
    let a = crate::args::parse(&spec, args)?;
    let name = a.get("--remote").context("name a remote with --remote <name>")?;
    let h = crate::cmd::here(ctx)?;
    h.repo.require(ctx, if operation == "gc" { "gc" } else { "fsck" })?;
    let mut store = open_named(name, &h.repo)?;
    store.connect(ctx)?;
    match operation {
        "fsck" if a.has("--caches") => {
            // Read only: what a sweep would delete (`sweep`).
            let (mut trees, mut bytes) = (0, 0);
            for (overlay, swept) in overlays::sweep_caches(&store, ctx, &h.repo, true)? {
                outln!(
                    ctx,
                    "remote {name} overlay {overlay}: {} unreachable cache state(s), {} part(s), {} bytes; {} unreachable chunk(s) in {} pack object(s), {} bytes; {} tree(s) with missing chunks; {} state(s) not known here kept",
                    swept.trees.len(),
                    swept.parts.len(),
                    swept.bytes,
                    swept.chunks,
                    swept.packs.len(),
                    swept.chunk_bytes,
                    swept.missing,
                    swept.unknown
                );
                trees += swept.trees.len() + swept.parts.len() + swept.packs.len() + swept.missing;
                bytes += swept.bytes + swept.chunk_bytes;
            }
            outln!(ctx, "remote {name}: unreachable cache data: {trees} object(s), {bytes} bytes");
            if trees > 0 {
                return Ok(1);
            }
        }
        "fsck" => {
            let report = store.verify(a.has("--files"))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&report)?);
            } else {
                outln!(
                    ctx,
                    "remote {name}: {} records, {} states, {} verified files, {} overlaps",
                    report.records,
                    report.states,
                    report.files,
                    report.overlaps
                );
                for gap in &report.gaps {
                    outln!(ctx, "gap: {gap}");
                }
            }
            if !report.gaps.is_empty() {
                return Ok(1);
            }
        }
        "log" => {
            for line in store.log()? {
                if a.has("--json") {
                    outln!(ctx, "{line}");
                } else {
                    let record = crate::oplog::parse_line(&line)?.record;
                    for state in record.states {
                        outln!(ctx, "{} {} {}", crate::ids::short(&state.id), state.time, state.message);
                    }
                }
            }
        }
        "gc" => {
            for (overlay, swept) in overlays::sweep_caches(&store, ctx, &h.repo, a.has("-n"))? {
                if let Some(refused) = &swept.refused {
                    eprintln!("warning: overlay {overlay}: the remote refused a delete ({refused}); the rest stays");
                }
                if !a.has("-q") && (!swept.trees.is_empty() || !swept.packs.is_empty()) {
                    let verb = if a.has("-n") { "would delete" } else { "deleted" };
                    outln!(
                        ctx,
                        "overlay {overlay}: {verb} {} dead cache state(s), {} bytes; {} dead chunk(s) in {} pack object(s), {} bytes; {} dead chunk(s) wait for a later pass",
                        swept.trees.len(),
                        swept.bytes,
                        swept.chunks,
                        swept.packs.len(),
                        swept.chunk_bytes,
                        swept.chunks_waiting
                    );
                }
            }
            let deleted = store.gc(a.has("-n"))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&deleted)?);
            } else if !a.has("-q") {
                outln!(ctx, "{} {} orphan remote parts", if a.has("-n") { "would delete" } else { "deleted" }, deleted.len());
            }
        }
        _ => bail!("unknown remote store operation"),
    }
    Ok(0)
}
