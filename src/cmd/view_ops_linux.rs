//! Command entry points for virtual line views.

use crate::cmd::{exit, here};
use crate::ctx::Ctx;
use crate::views::*;
use anyhow::{bail, Result};

pub fn show(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = crate::args::Spec::new().flag("--folder").flag("--none").flag("--virtual");
    let a = crate::args::parse(&spec, args)?;
    let modes = ["--folder", "--none", "--virtual"].iter().filter(|m| a.has(m)).count();
    if a.pos.is_empty() || a.pos.len() > 2 || modes != 1 || (a.pos.len() == 2 && !a.has("--folder")) {
        return Err(exit(2, "usage: layr line show <line> --folder [<path>] | --none | --virtual"));
    }
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = repo.line(&a.pos[0])?;
    repo.authorize_line(ctx, &line, crate::repo::LineOp::Manage)?;
    let Some(view) = view_of(repo, &line) else {
        bail!("line {} has a folder; only virtual lines change their view for now", line.name);
    };
    // Fast lines are not experimental.
    if !view.is_layrfs() {
        require_enabled(ctx)?;
    }
    if h.line.as_ref().is_some_and(|l| l.id == line.id) || ctx.cwd.starts_with(repo.working(&line)) {
        bail!("run this command outside the line's folder: its view changes");
    }
    let _line_lock = repo.line_lock(&line)?;
    let _lock = _line_lock.lock(&repo.p)?;
    let line = repo.line(&line.id)?;
    if view.is_layrfs() {
        return show_fast(ctx, repo, &line, &view, a.has("--none"), a.has("--folder"));
    }
    recover_move(repo, &line)?;
    let hidden = dir(repo, &line.id).join("tree");
    let visible = repo.p.default_line_path(&line.name);
    let mut from = repo.working(&line);
    if !from.exists() {
        let found: Vec<_> = [&hidden, &visible].into_iter().filter(|p| p.exists()).collect();
        if found.len() != 1 {
            bail!("cannot locate one working tree for view {}; no files were removed", line.name);
        }
        from = found[0].clone();
        repo.p.set_moved_path(&line.id, Some(&from))?;
    }
    let to = if a.has("--none") {
        hidden
    } else if let Some(arg) = a.pos.get(1) {
        // A retry of an interrupted conversion already has its tree at the target.
        // Accept that same tree, but never another existing destination.
        let requested = ctx.cwd.join(arg);
        if requested.canonicalize().ok().as_ref() == Some(&from) {
            from.clone()
        } else {
            crate::cmd::branch::move_target(ctx, repo, &from, arg)?
        }
    } else {
        visible.clone()
    };
    move_tree(ctx, repo, &line, &from, &to)?;
    if to == visible {
        repo.p.set_moved_path(&line.id, None)?;
    }
    if a.has("--folder") {
        forget(repo, &line.id)?;
    }
    if a.has("--none") {
        outln!(ctx, "Line {} has no visible folder; its changes stay in {}", line.name, to.display());
    } else {
        outln!(ctx, "Line {} has a {} view at {}", line.name, if a.has("--folder") { "folder" } else { "virtual" }, to.display());
    }
    Ok(0)
}
