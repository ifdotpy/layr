//! follow: a line keeps a remote branch (docs/follow.md).

use super::admin::set_config;
use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::follow::{self, Last};
use crate::gitbridge;
use crate::model::Line;
use crate::repo::{LineOp, Repo};
use anyhow::Result;

pub fn follow(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--run").flag("--remove");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line()?.clone();
    let usage = || exit(128, "usage: layr follow [<remote>/<branch> | --run | --remove]");
    if a.has("--remove") {
        if a.has("--run") || !a.pos.is_empty() {
            return Err(usage());
        }
        repo.authorize_line(ctx, &line, LineOp::Merge { expect: true, hint: None })?;
        let _lock = repo.p.lock()?;
        set_config(ctx, repo, &follow::config_key(&line.id), serde_json::Value::Null)?;
        outln!(ctx, "Not following on line {}", line.name);
        return Ok(0);
    }
    let set = match a.pos.as_slice() {
        [] => None,
        [target] => Some(target.clone()),
        _ => return Err(usage()),
    };
    let Some(target) = set else {
        if !a.has("--run") {
            return show(ctx, repo, &line);
        }
        return match follow::target(repo, &line)? {
            Some(_) => finish(ctx, repo, &line),
            None => Err(exit(1, format!("line '{}' follows no branch: layr follow <remote>/<branch>", line.name))),
        };
    };
    let (remote, _) =
        follow::split(&target).ok_or_else(|| exit(128, "usage: layr follow <remote>/<branch> (both names are needed)"))?;
    if a.has("--run") {
        return Err(usage());
    }
    // The same right as `layr merge --expect` on this line; a protected line needs its admin.
    repo.authorize_line(ctx, &line, LineOp::Merge { expect: true, hint: None })?;
    // The remote must exist before the target is stored.
    gitbridge::remote_url(repo, remote)?;
    {
        let _lock = repo.p.lock()?;
        set_config(ctx, repo, &follow::config_key(&line.id), serde_json::Value::String(target.clone()))?;
    }
    finish(ctx, repo, &line)
}

/// Run one pass now and print its result. Exit 1 when the pass failed.
fn finish(ctx: &Ctx, repo: &Repo, line: &Line) -> Result<i32> {
    let last = repo.follow_pass(ctx, line)?;
    let target = follow::target(repo, line)?.unwrap_or_default();
    outln!(ctx, "{target}: {}", last.text);
    Ok(if last.ok { 0 } else { 1 })
}

/// `layr follow` alone: the target and the last pass.
fn show(ctx: &Ctx, repo: &Repo, line: &Line) -> Result<i32> {
    match follow::target(repo, line)? {
        None => outln!(ctx, "Not following on line {}", line.name),
        Some(target) => {
            outln!(ctx, "Following {target} on line {}", line.name);
            outln!(ctx, "{}", describe(follow::last_result(repo, line)?.as_ref()));
        }
    }
    Ok(0)
}

/// One line for `layr status`: the target and the last pass. None when the line follows nothing.
pub fn status_line(repo: &Repo, line: &Line) -> Result<Option<String>> {
    Ok(follow::target(repo, line)?
        .map(|target| format!("Following {target}: {}", describe(follow::last_result(repo, line).ok().flatten().as_ref()))))
}

fn describe(last: Option<&Last>) -> String {
    match last {
        None => "last pass: none yet".into(),
        Some(l) => {
            format!("last pass {} {}: {}", super::history::fmt_date(l.time, "iso"), if l.ok { "ok" } else { "error" }, l.text)
        }
    }
}
