//! Revision names: `head`, `@`, line names, `line@machine`, tags, remote refs (`origin/main`),
//! state ids and their prefixes, git commit ids, `park@{n}`, `head@{n}` (automatic states,
//! newest first) and the suffixes `~n`, `^` and `^n`.

pub(crate) mod change;
pub(crate) mod parked;

use crate::errors::{error, Kind as ErrorKind};
use crate::model::{Line, StateKind};
use crate::repo::{Repo, ResolvedRevision};
mod expression;
use anyhow::{bail, Result};
use expression::{AncestorStep, BaseSelector, RevisionExpr};

pub fn resolve_state(repo: &Repo, line: Option<&Line>, rev: &str) -> Result<String> {
    Ok(resolve(repo, line, rev)?.into_state())
}

pub fn resolve(repo: &Repo, line: Option<&Line>, rev: &str) -> Result<ResolvedRevision> {
    let expr = RevisionExpr::parse(rev)?;
    let mut revision = match expr.base {
        BaseSelector::Position { name, index: n } => {
            let id = {
                let history = repo.p.db.ref_log(name)?;
                if !history.is_empty() {
                    let positions: Vec<_> = history.into_iter().filter_map(|ch| ch.after).collect();
                    positions
                        .get(n)
                        .map(|r| r.target.clone())
                        .ok_or_else(|| anyhow::anyhow!("{rev}: ref has only {} positions", positions.len()))?
                } else if name == "park" {
                    let l = need_line(line, rev)?;
                    match crate::revs::parked::parked_lines(repo, l)?.get(n) {
                        Some(parked) => parked.head.clone(),
                        None => bail!("park@{{{n}}} does not exist"),
                    }
                } else {
                    let l = if name.is_empty() || name == "head" || name == "@" {
                        need_line(line, rev)?.clone()
                    } else {
                        repo.line(name)?
                    };
                    let v = repo.p.db.line_states(&l.id, StateKind::Auto)?;
                    match v.get(n) {
                        Some(s) => s.id.clone(),
                        None => bail!("{rev}: line '{}' has only {} automatic states", l.name, v.len()),
                    }
                }
            };
            repo.resolved_state(&id)?
        }
        BaseSelector::Head => repo.resolved_state(&need_line(line, "head")?.head)?,
        BaseSelector::Park => {
            let parked = crate::revs::parked::parked_lines(repo, need_line(line, "park")?)?;
            repo.resolved_state(&parked.first().ok_or_else(|| anyhow::anyhow!("no park entries"))?.head)?
        }
        BaseSelector::Change { prefix, version, spelling } => {
            let id = crate::revs::change::find(repo, prefix)?;
            let states = repo.p.db.change_versions(&id)?;
            let state = states
                .get(version.unwrap_or(states.len().saturating_sub(1)))
                .ok_or_else(|| anyhow::anyhow!("unknown change version '{spelling}'"))?;
            repo.resolved_state(&state.id)?
        }
        BaseSelector::View(id) => ResolvedRevision::View(repo.saved_view(id.as_str())?),
        BaseSelector::Attestation(id) => {
            let attestation =
                repo.p.db.attestation(&id.to_string())?.ok_or_else(|| anyhow::anyhow!("unknown attestation '{id}'"))?;
            match attestation.subject {
                crate::model::Subject::View(id) => ResolvedRevision::View(repo.saved_view(id.as_str())?),
                crate::model::Subject::State(id) => repo.resolved_state(id.as_str())?,
                _ => bail!("unsupported attestation subject"),
            }
        }
        BaseSelector::Name(name) => repo.resolved_state(&base_state(repo, name)?)?,
    };
    for step in &expr.ancestors {
        match step {
            AncestorStep::Identity | AncestorStep::FirstParent(0) => {}
            AncestorStep::FirstParent(count) => {
                let mut seen = std::collections::HashSet::new();
                for _ in 0..*count {
                    let source = revision.into_source(repo)?;
                    if *count > 1 && !seen.insert(source) {
                        bail!("bad revision {rev}");
                    }
                    let parent =
                        repo.parents(&source)?.into_iter().next().ok_or_else(|| anyhow::anyhow!("{rev}: state has no parent"))?;
                    revision = repo.resolved_state(&parent)?;
                }
            }
            AncestorStep::Parent(index) => {
                let source = revision.into_source(repo)?;
                let parent = repo
                    .parents(&source)?
                    .into_iter()
                    .nth(index.get() - 1)
                    .ok_or_else(|| anyhow::anyhow!("{rev}: state has no parent {index}"))?;
                revision = repo.resolved_state(&parent)?;
            }
        }
    }
    Ok(revision)
}

/// Resolve the complete stack for an exact view or line, and a layer-free view for a state.
pub fn resolve_view(repo: &Repo, line: Option<&Line>, rev: &str) -> Result<crate::overlay::View> {
    reject_old(rev)?;
    let view = if let Some(id) = rev.strip_prefix("views/") {
        repo.saved_view(id)?
    } else if let Some(id) = rev.strip_prefix("attestations/") {
        let a = repo.p.db.attestation(id)?.ok_or_else(|| anyhow::anyhow!("unknown attestation '{id}'"))?;
        if matches!(&a.subject, crate::model::Subject::View(_)) {
            repo.saved_view(a.subject.id())?
        } else {
            crate::overlay::View::new(resolve_state(repo, line, rev)?, Vec::new())
        }
    } else if matches!(rev, "" | "head" | "@") {
        repo.view_of(need_line(line, rev)?)?
    } else if let Some(line) = repo.p.db.line_by_name(rev)? {
        repo.view_of(&repo.localize(line)?)?
    } else {
        crate::overlay::View::new(resolve_state(repo, line, rev)?, Vec::new())
    };
    repo.check_view_collected(&view)?;
    Ok(view)
}

/// Reject old revision tokens before a caller can reinterpret them as paths or refs.
pub fn reject_old(rev: &str) -> Result<()> {
    let rev = rev.trim_start_matches('^');
    let base = rev.split(['~', '^', ':']).next().unwrap_or(rev);
    if base == "HEAD" || base.starts_with("HEAD@{") {
        return Err(error(ErrorKind::Unsupported, "HEAD was removed. Use head."));
    }
    if base == "stash" || base.starts_with("stash@{") {
        return Err(error(ErrorKind::Unsupported, "stash revisions were removed. Use park@{n}."));
    }
    Ok(())
}

fn need_line<'a>(line: Option<&'a Line>, rev: &str) -> Result<&'a Line> {
    match line {
        Some(l) => Ok(l),
        None => bail!("{rev}: not in a line; name a line, tag or state id instead, or add --line <name>"),
    }
}

fn base_state(repo: &Repo, name: &str) -> Result<String> {
    if let Some(r) = repo.p.db.ref_value(name, crate::ids::now_ms())? {
        return Ok(r.target);
    }
    if crate::model::reserved_revision(name) {
        bail!("unknown or expired ref '{name}'");
    }
    if let Some(l) = repo.p.db.line_by_name(name)? {
        return Ok(repo.localize(l)?.head);
    }
    if let Some((ln, m)) = name.split_once('@') {
        if let (Some(l), Some(mid)) = (repo.p.db.line_by_name(ln)?, repo.p.db.machine_by_name(m)?) {
            for (machine, head) in repo.p.db.line_heads(&l.id)? {
                if machine == mid {
                    return Ok(head);
                }
            }
            bail!("line '{ln}' has no head on machine {m}");
        }
    }
    if let Some(t) = repo.p.db.tag(name)? {
        return Ok(t);
    }
    if let Some((s, _)) = repo.p.db.remote_ref(name)? {
        return Ok(s);
    }
    if let Some((s, _)) = repo.p.db.remote_ref(&format!("origin/{name}"))? {
        if name.contains('/') {
            return Ok(s);
        }
    }
    // A branch that a fetch stored but did not import (a fetch imports only the branches that it
    // wants): it is imported on first use, as `layr git fetch <remote> <branch>` would.
    if let Some(st) = import_stored_branch(repo, name)? {
        return Ok(st);
    }
    if name.len() == 36 && uuid::Uuid::parse_str(name).is_ok() && repo.p.db.state(name)?.is_some() {
        return Ok(name.to_string());
    }
    if name.len() >= 4 && name.chars().all(|c| c.is_ascii_hexdigit()) {
        let lower = name.to_ascii_lowercase();
        let v = repo.p.db.states_by_prefix(&lower)?;
        if v.len() == 1 {
            return Ok(v[0].clone());
        }
        if v.len() > 1 {
            bail!("short state id {name} is ambiguous");
        }
        let g = repo.p.db.state_of_commit(&lower)?;
        if g.len() == 1 {
            return Ok(g[0].clone());
        }
        // Several states of one full Git commit id (a state that a push exported and the import of the same commit
        // on another machine): they hold the same tree. The one whose tree this machine has or builds goes first.
        if g.len() > 1 && lower.len() == 40 {
            return Ok(crate::gitbridge::mapped_state(repo, &lower)?.unwrap_or_else(|| g[0].clone()));
        }
        // A commit that layr made without a record: `git rev-parse HEAD` in a line.
        if g.is_empty() {
            if let Some(st) = crate::gitbridge::state_of_history_commit(repo, &lower)? {
                return Ok(st);
            }
        }
    }
    bail!("unknown revision '{name}'")
}

/// The state of `<remote>/<branch>` (or `<branch>` of origin when it has a slash) that the Git
/// store of the project holds from a fetch but layr has not imported: imported now. None for any
/// other name, and for a command without a context.
fn import_stored_branch(repo: &Repo, name: &str) -> Result<Option<String>> {
    let Some(ctx) = repo.ctx else { return Ok(None) };
    let Some((first, _)) = name.split_once('/') else { return Ok(None) };
    let full = if crate::gitbridge::remote_url(repo, first).is_ok() { name.to_string() } else { format!("origin/{name}") };
    let remote = full.split('/').next().unwrap_or("origin");
    if crate::gitbridge::remote_url(repo, remote).is_err() {
        return Ok(None);
    }
    let stored = crate::gitbridge::stored_remote_refs(repo, remote).unwrap_or_default();
    let Some((_, commit)) = stored.into_iter().find(|(n, _)| *n == full) else { return Ok(None) };
    Ok(Some(crate::gitbridge::import_ref(ctx, repo, &commit, &full, None)?))
}
