//! Attach a local input folder to an existing project without moving its files.
use crate::{
    args::{parse, Spec},
    ctx::Ctx,
    repo::Repo,
    workspace::Binding,
};
use anyhow::{bail, Result};
use std::path::PathBuf;

pub fn workspace(ctx: &Ctx, args: &[String]) -> Result<i32> {
    if args.first().map(String::as_str) == Some("inspect") {
        let a = parse(&Spec::new().value("--line"), &args[1..])?;
        if a.pos.len() != 1 {
            bail!("usage: layr workspace inspect <project> [--line <line>]");
        }
        let repo = Repo::open(ctx, &a.pos[0])?;
        repo.can_read(ctx)?;
        let _lock = repo.p.lock()?;
        let line = repo.line(a.get("--line").unwrap_or("main"))?;
        outln!(ctx, "{}", serde_json::json!({
            "version": 1,
            "project": repo.p.name,
            "project_id": repo.p.id,
            "line": line.name,
            "line_id": line.id,
            "state": crate::ids::display(&line.head),
            "path": repo.working(&line),
            "owner": line.owner,
        }));
        return Ok(0);
    }
    if args.first().map(String::as_str) != Some("attach") {
        bail!("usage: layr workspace attach <project> --path <folder> [--line <line>]");
    }
    let a = parse(&Spec::new().value("--path").value("--line"), &args[1..])?;
    if a.pos.len() != 1 {
        bail!("workspace attach needs one project name");
    }
    let source = a.get("--path").ok_or_else(|| anyhow::anyhow!("workspace attach requires --path"))?;
    let source = crate::privs::as_caller_fs(ctx, || Ok(ctx.cwd.join(PathBuf::from(source)).canonicalize()?))?;
    let repo = Repo::open(ctx, &a.pos[0])?;
    repo.require(ctx, "config")?;
    let line = repo.line(a.get("--line").unwrap_or("main"))?;
    repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
    let _lock = repo.p.lock()?;
    if repo.p.workspace.is_none() {
        let (status, _, _, _) = repo.status(&line)?;
        if !status.clean() || !status.untracked.is_empty() {
            bail!("the managed line has local changes; save them before attachment");
        }
        if repo.is_stacked(&line)? {
            bail!("attach the input folder before adding overlays");
        }
    }
    let binding = crate::privs::as_caller_fs(ctx, || {
        Binding::new(&ctx.store.root, &repo.p.name, &repo.p.id, &line.name, &line.id, &source, line.owner)
    })?;
    let attached = crate::workspace::attach(&repo.p.meta_dir(), binding)?;
    outln!(ctx, "{}", serde_json::to_string(&attached.locator)?);
    Ok(0)
}
