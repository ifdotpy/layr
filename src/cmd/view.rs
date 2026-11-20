//! Addressable shared-state and overlay stacks.

use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use anyhow::{bail, Result};

pub fn view(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").value("--overlay").value("--folder"), args)?;
    let h = here(ctx)?;
    let save = a.pos.first().map(String::as_str) == Some("save");
    let show = a.pos.first().map(String::as_str) == Some("show");
    let rest = if save || show { &a.pos[1..] } else { &a.pos[..] };
    if rest.len() > 1 || a.dashdash {
        bail!("usage: layr view [save [<line>]|show <id>|<rev or line>] [--overlay <overlay>] [--folder <dir>] [--json]");
    }
    if show && (a.get("--overlay").is_some() || a.get("--folder").is_some()) {
        bail!("view show takes no --overlay or --folder");
    }
    let _lock = h.repo.p.lock_maintenance()?;
    let current = h.line.as_ref().map(|line| h.repo.line(&line.id)).transpose()?;
    let mut view = if show {
        let id = rest.first().ok_or_else(|| anyhow::anyhow!("usage: layr view show <id> [--json]"))?;
        h.repo.saved_view(id.strip_prefix("views/").unwrap_or(id))?
    } else if save {
        let line = match rest.first() {
            Some(name) => h.repo.line(name)?,
            None => current.clone().ok_or_else(|| anyhow::anyhow!("view save requires a line"))?,
        };
        h.repo.view_of(&line)?
    } else {
        let rev = rest.first().map(String::as_str).unwrap_or("head");
        let mut view = crate::revs::resolve_view(&h.repo, current.as_ref(), rev)?;
        // The view of a state is the state with its parts (docs/overlays.md, section 1a).
        let state_rev = !matches!(rev, "" | "head" | "@")
            && !rev.starts_with("views/")
            && !rev.starts_with("attestations/")
            && h.repo.p.db.line_by_name(rev)?.is_none();
        if state_rev && view.layers.is_empty() {
            view.layers = h.repo.part_layers(current.as_ref(), &view.shared)?;
            view.validate()?;
            h.repo.check_view_collected(&view)?;
        }
        view
    };
    // Another view of the same line or state, seen from one overlay (docs/overlays.md section 1, owner
    // decision 2026-11-15): the path of the tree from `main` to it. `main` is the view of `main` alone.
    if let Some(top) = a.get("--overlay") {
        let top = top.trim();
        if top == "base" {
            return Err(super::exit(2, "layr view --overlay base was removed: the bottom overlay is main. Use --overlay main"));
        }
        if top.contains(',') {
            return Err(super::exit(2, "layr view --overlay takes one overlay: the view shows it and the overlays below it"));
        }
        let line = match rest.first().map(String::as_str) {
            None | Some("" | "head" | "@") => current.clone(),
            Some(name) => h.repo.p.db.line_by_name(name)?.map(|l| h.repo.localize(l)).transpose()?,
        };
        let mut layers = Vec::new();
        if top != crate::overlay::MAIN {
            if !h.repo.reads_overlay(top, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) || !h.repo.patches_overlay(top)? {
                bail!("unknown overlay '{top}'");
            }
            for name in h.repo.overlay_path(top)? {
                if !h.repo.reads_overlay(&name, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                    bail!("permission denied: you do not read overlay '{name}', which is below '{top}'");
                }
                let layer = match view.layers.iter().find(|l| l.overlay == name) {
                    Some(l) => l.clone(),
                    None => h
                        .repo
                        .overlay_layer(line.as_ref(), &view.shared, &name)?
                        .ok_or_else(|| anyhow::anyhow!("unknown overlay '{name}'"))?,
                };
                layers.push(layer);
            }
        }
        view.layers = layers;
        view.validate()?;
        h.repo.check_view_collected(&view)?;
    }
    if show
        && view.layers.iter().any(|layer| {
            h.repo.need_overlay(&layer.overlay).is_ok_and(|i| i.hidden)
                && !h.repo.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
        })
    {
        bail!("unknown view");
    }
    view.layers.retain(|layer| {
        !h.repo.need_overlay(&layer.overlay).is_ok_and(|i| i.hidden)
            || h.repo.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
    });
    let id = if save { h.repo.save_view(&_lock, ctx, &view)? } else { view.id()? };
    // A folder with the files of the view: a copy to read and build in, not a working folder of
    // the line (it has no status and no save).
    if let Some(dir) = a.get("--folder") {
        let dst = std::path::Path::new(dir);
        let dst = if dst.is_absolute() { dst.to_path_buf() } else { std::env::current_dir()?.join(dst) };
        if dst.exists() {
            bail!("{} already exists", dst.display());
        }
        let tmp = crate::model::Line {
            id: format!("view-{:016x}", crate::ids::random_u64()),
            name: format!(".view-{:016x}", crate::ids::random_u64()),
            head: view.shared.clone(),
            owner: ctx.caller.uid,
            machine: h.repo.p.store.machine.id.clone(),
        };
        if view.layers.is_empty() {
            h.repo.materialize(&tmp, &view.shared, &dst)?;
        } else {
            h.repo.materialize_view(ctx, &tmp, &view, &dst)?;
        }
    }
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::json!({"view_id":id,"view":view}));
    } else {
        outln!(ctx, "{}\t{}", id, serde_json::to_string(&view)?);
    }
    Ok(0)
}
