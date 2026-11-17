//! Command entry points for virtual line views.

use crate::changes;
use crate::cmd::{exit, here, Here};
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, StateKind};
use crate::repo::Repo;
use crate::views::*;
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

/// `layr save -m <message>` of a virtual line: every change of the view becomes the new head.
pub fn save(ctx: &Ctx, h: &Here, line: &Line, v: &View, message: &str, quiet: bool, json: bool) -> Result<i32> {
    let repo = &h.repo;
    repo.authorize_line(ctx, line, crate::repo::LineOp::Direct)?;
    let _line_lock = repo.line_lock(line)?;
    let line = repo.line(&line.id)?;
    let t = trees(repo, &line, v)?;
    let list = file_changes(&t, v)?;
    if list.is_empty() {
        if json {
            outln!(ctx, "{}", crate::cmd::work::nothing_json(&line));
        } else {
            outln!(ctx, "On line {}\nnothing to save, working tree clean", line.name);
        }
        return Ok(1);
    }
    let head = repo.state(&line.head)?;
    let mut st = write_state(ctx, repo, &line, &t, &list, StateKind::Commit, message)?;
    let checked = crate::cmd::work::named_checks(
        ctx,
        repo,
        &head,
        &mut st,
        &t.head.clone().into(),
        &t.merged.clone().into(),
        &line,
        &list,
        Default::default(),
    );
    if let Err(error) = checked {
        repo.discard_states(std::slice::from_ref(&st));
        return Err(error);
    }
    let moved = repo.commit_line(&_line_lock, &line, &line.head, |locked, now| {
        repo.move_head(locked, ctx, "commit", now, &st.id, vec![st.clone()], None, serde_json::json!({"line": line.id}))
    });
    if let Err(e) = moved {
        if matches!(e, crate::db::CommitResolution::NotCommitted(_)) {
            repo.discard_states(std::slice::from_ref(&st));
        }
        return Err(e.into());
    }
    if json {
        let new_root = repo.state_src(&st.id)?;
        let fds = crate::diff::file_diffs(&t.head, &new_root, &changes::with_renames(&t.head, &new_root, list))?;
        let mut out = crate::cmd::work::saved_json(&line, &st, &fds);
        out["amend"] = serde_json::json!(false);
        outln!(ctx, "{out}");
    } else if !quiet {
        outln!(ctx, "[{} {}] {}", line.name, ids::short(&st.id), message.lines().next().unwrap_or(""));
        let new_root = repo.state_src(&st.id)?;
        for c in changes::with_renames(&t.head, &new_root, list) {
            let code = match (c.old(), c.new(), c.old_path()) {
                (_, _, Some(o)) => format!("rename {o} =>"),
                (None, _, _) => "create".into(),
                (_, None, _) => "delete".into(),
                _ => "modify".into(),
            };
            outln!(ctx, " {code} {}", c.path());
        }
    }
    Ok(0)
}

/// Delete a virtual line: unsaved changes become its final automatic state, as for a folder.
pub fn delete(
    ctx: &Ctx,
    repo: &Repo,
    l: &Line,
    v: &View,
    target: &crate::lineops::DeleteTarget,
    check: impl FnOnce(&Line) -> Result<()>,
) -> Result<()> {
    let _lifecycle = lifecycle_lock(repo, l)?;
    let _line_lock = repo.line_lock(l)?;
    let l = {
        let _lock = repo.p.lock_maintenance()?;
        target.validate(repo)?
    };
    let at = repo.working(&l);
    let last = if v.mounted || upper(&repo.p, &l.id).exists() {
        let t = trees(repo, &l, v)?;
        let list = file_changes(&t, v)?;
        if list.is_empty() {
            None
        } else {
            Some(write_state(ctx, repo, &l, &t, &list, StateKind::Auto, "final state of a deleted virtual line")?)
        }
    } else {
        None
    };
    // Test only (t257): after the final state was prepared, before the locked validation.
    if let Err(e) = crate::cmd::branch::deletion_barrier() {
        if let Some(st) = &last {
            repo.discard_states(std::slice::from_ref(st));
        }
        return Err(e);
    }
    let _lock = _line_lock.lock(&repo.p)?;
    let fresh = match target.validate(repo).and_then(|line| {
        repo.authorize_line(ctx, &line, crate::repo::LineOp::Manage)?;
        // A refusal after the final state was prepared keeps the upper data and discards the state (t257).
        check(&line)?;
        Ok(line)
    }) {
        Ok(line) => line,
        Err(e) => {
            if let Some(st) = &last {
                repo.discard_states(std::slice::from_ref(st));
            }
            return Err(e);
        }
    };
    if is_mounted(&at) {
        if let Err(e) = unmount(&at) {
            if let Some(st) = &last {
                repo.discard_states(std::slice::from_ref(st));
            }
            return Err(e);
        }
    }
    let last_id = match last {
        Some(st) => {
            let mut rec = crate::model::DraftRecord::new("save");
            let before = repo.recorded_working(&fresh)?;
            rec.data =
                serde_json::json!({"line": fresh.id, "head": fresh.head, "working_before": before, "working_after": st.id});
            rec.states.push(st.clone());
            repo.commit_record(&_lock, ctx, rec)?;
            Some(st.id)
        }
        None => None,
    };
    crate::cmd::branch::delete_recorded(&_lock, ctx, repo, &fresh, last_id, at.exists())?;
    repo.p.trash(&dir(&repo.p, &fresh.id))?;
    Ok(())
}

/// `layr line show <line> --folder [<path>] | --none | --virtual`.
pub fn show(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = crate::args::Spec::new().flag("--folder").flag("--none").flag("--virtual");
    let a = crate::args::parse(&spec, args)?;
    let modes = ["--folder", "--none", "--virtual"].iter().filter(|m| a.has(m)).count();
    if a.pos.is_empty() || a.pos.len() > 2 || modes != 1 || (a.pos.len() == 2 && !a.has("--folder")) {
        return Err(exit(2, "usage: layr line show <line> --folder [<path>] | --none | --virtual"));
    }
    require_enabled(ctx)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let l = repo.line(&a.pos[0])?;
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
    let Some(mut v) = of_line(&repo.p, &l.id) else {
        return Err(exit(1, format!("line {} has a folder; only virtual lines change their view for now", l.name)));
    };
    if h.line.as_ref().is_some_and(|c| c.id == l.id) || ctx.cwd.starts_with(repo.working(&l)) {
        return Err(exit(1, "run this command outside the line's folder: its mount changes"));
    }
    let _lifecycle = lifecycle_lock(repo, &l)?;
    let _line_lock = repo.line_lock(&l)?;
    let l = repo.line(&l.id)?;
    let at = repo.working(&l);
    if a.has("--none") {
        if is_mounted(&at) {
            unmount(&at)?;
        }
        let _ = std::fs::remove_dir(&at);
        v.mounted = false;
        write_view(&repo.p, &v)?;
        outln!(ctx, "Line {} has no folder now; its changes stay in {}", l.name, upper(&repo.p, &l.id).display());
        return Ok(0);
    }
    if a.has("--virtual") {
        if !is_mounted(&at) {
            if at.symlink_metadata().is_err() {
                std::fs::create_dir(&at)?;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o700))?;
            }
            mount(&dir(&repo.p, &l.id), &at)?;
        }
        v.mounted = true;
        write_view(&repo.p, &v)?;
        outln!(ctx, "Line {} is virtual at {}", l.name, at.display());
        return Ok(0);
    }
    // --folder: a native folder with the merged content, then the view goes.
    let target = match a.pos.get(1) {
        Some(p) => {
            let p = ctx.cwd.join(p);
            if p.symlink_metadata().is_ok() {
                bail!("{} already exists", p.display());
            }
            Some(p)
        }
        None => None,
    };
    let t = trees(repo, &l, &v)?;
    strip_quarantine(&t.upper, &{
        let mut all = BTreeSet::new();
        fsutil::walk(t.upper.as_path(), "", &mut |rel, _| {
            all.insert(rel.to_string());
            Ok(true)
        })?;
        all
    });
    let tmp = repo.p.work_dir().join(crate::store::temp_name());
    std::fs::create_dir_all(repo.p.work_dir())?;
    let made = (|| -> Result<()> {
        t.merged.materialize(&tmp, false)?;
        for name in SYSTEM_ROOT_NAMES {
            let p = tmp.join(name);
            if p.symlink_metadata().is_ok() {
                crate::fs::remove(&p)?;
            }
        }
        repo.prepare_at(&l, &l.head, &tmp, &[])?;
        repo.validate_cache_locations(&l, &tmp, target.as_deref().unwrap_or(&at), crate::cache_overlay::CacheAccess::Prepared)
    })();
    if let Err(e) = made {
        let _ = crate::fs::remove(&tmp);
        return Err(e);
    }
    if is_mounted(&at) {
        if let Err(e) = unmount(&at) {
            let _ = crate::fs::remove(&tmp);
            return Err(e);
        }
    }
    let _ = std::fs::remove_dir(&at);
    let dst = target.clone().unwrap_or_else(|| at.clone());
    crate::fsutil::rename_tree(&tmp, &dst)?;
    if let Some(p) = &target {
        let _lock = _line_lock.lock(&repo.p)?;
        repo.p.set_moved_path(&l.id, Some(p))?;
    }
    repo.p.trash(&dir(&repo.p, &l.id))?;
    outln!(ctx, "Line {} has a folder at {}", l.name, dst.display());
    Ok(0)
}
