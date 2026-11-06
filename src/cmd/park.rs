//! `layr park`: unfinished work kept as a line without a working folder. `park` saves the changes
//! of the working folder as the head of a new line `parked.<line>.<n>` and resets the folder;
//! `park pop` and `park apply` merge that head back; `park list` lists those lines, newest first
//! (`park@{0}` is the newest). A parked line is an ordinary line: `line list`, `line delete`,
//! `show`, `diff` and sync work on it.

use super::work::{begin_change, prune_empty_in, record_working};
use super::{exit, here};
use crate::args::{parse, Spec};
use crate::changes;
use crate::ctx::Ctx;
use crate::diff;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, LineChange, Record, StateKind};
use crate::repo::{skip_path, Repo, Snap, Status, Temp};
use crate::revs;
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::path::Path;

/// The name prefix of parked lines.
const PREFIX: &str = "parked.";

/// The name of park entry `n` of line `line`.
fn parked_name(line: &str, n: u64) -> String {
    format!("{PREFIX}{line}.{n}")
}

/// The number of a parked line of `line`, from its name.
fn parked_number(line: &str, name: &str) -> Option<u64> {
    let n = name.strip_prefix(PREFIX)?.strip_prefix(line)?.strip_prefix('.')?;
    n.chars().all(|c| c.is_ascii_digit()).then(|| n.parse().ok()).flatten()
}

/// True for the name of a parked line (of any line).
pub fn is_parked(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.rsplit_once('.'))
        .is_some_and(|(line, n)| !line.is_empty() && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The parked lines of `line` on this machine, newest first: `park@{0}` is the first.
pub fn parked_lines(repo: &Repo, line: &Line) -> Result<Vec<Line>> {
    let mut v: Vec<(u64, Line)> = repo
        .p
        .db
        .lines()?
        .into_iter()
        .filter(|l| l.machine == repo.p.store.machine.id)
        .filter_map(|l| parked_number(&line.name, &l.name).map(|n| (n, l)))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(v.into_iter().map(|(_, l)| l).collect())
}

/// The next free parked line name of `line`.
fn next_name(repo: &Repo, line: &Line) -> Result<String> {
    let mut n = repo.p.db.lines()?.iter().filter_map(|l| parked_number(&line.name, &l.name)).max().unwrap_or(0) + 1;
    while repo.p.db.line_by_name(&parked_name(&line.name, n))?.is_some() || repo.p.line_path(&parked_name(&line.name, n)).exists()
    {
        n += 1;
    }
    Ok(parked_name(&line.name, n))
}

/// The park entry that `arg` names (`park@{n}`, a parked line name or its head state), with its
/// position in `park list`. None: the argument names no park entry.
fn entry(repo: &Repo, line: &Line, arg: Option<&str>) -> Result<Option<(usize, Line)>> {
    let parked = parked_lines(repo, line)?;
    let arg = arg.unwrap_or("park@{0}");
    if let Some(n) = arg.strip_prefix("park@{").and_then(|r| r.strip_suffix('}')) {
        let n: usize = n.parse().map_err(|_| anyhow::anyhow!("bad revision {arg}"))?;
        return match parked.get(n) {
            Some(l) => Ok(Some((n, l.clone()))),
            None => bail!("{arg} does not exist"),
        };
    }
    if arg == "park" {
        return match parked.first() {
            Some(l) => Ok(Some((0, l.clone()))),
            None => bail!("no park entries"),
        };
    }
    if let Some(i) = parked.iter().position(|l| l.name == arg) {
        return Ok(Some((i, parked[i].clone())));
    }
    let id = revs::resolve(repo, Some(line), arg)?;
    Ok(parked.iter().position(|l| l.head == id).map(|i| (i, parked[i].clone())))
}

/// Park entries of older layr versions were states of kind `stash` on their line. The first `park`
/// command after the upgrade makes each entry of a line that the caller may change a parked line:
/// one record makes the line and drops the old entry, so a crash leaves each entry in one place.
/// Until then the old entries keep their trees (the `park` owner kind, docs/ownership.md).
fn move_old_entries(ctx: &Ctx, repo: &Repo) -> Result<()> {
    if repo.p.db.old_park_entries()?.is_empty() {
        return Ok(());
    }
    let _lock = repo.p.lock()?;
    for (state, line_id) in repo.p.db.old_park_entries()? {
        let Some(line) = repo.p.db.line_by_id(&line_id)? else { continue };
        if line.machine != repo.p.store.machine.id || repo.authorize_line(ctx, &line, crate::repo::LineOp::Direct).is_err() {
            continue;
        }
        let parked = Line {
            id: ids::new_id(),
            name: next_name(repo, &line)?,
            head: state.clone(),
            owner: line.owner,
            machine: repo.p.store.machine.id.clone(),
        };
        let mut rec = Record::new("stash.drop");
        rec.data = serde_json::json!({"state": state, "line": line.id, "parked_line": parked.name});
        rec.lines.insert(
            parked.id.clone(),
            LineChange { before: None, after: Some(repo.line_state(&parked, &state)), working_before: None, working_after: None },
        );
        repo.commit_record(ctx, rec)?;
        eprintln!("park entry {} is now the line {}", ids::short(&state), parked.name);
    }
    Ok(())
}

pub fn park(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("push");
    let (sub, rest): (&str, Vec<String>) = match sub {
        "push" | "save" | "list" | "show" | "pop" | "apply" | "drop" | "clear" => (sub, args.get(1..).unwrap_or(&[]).to_vec()),
        _ => ("push", args.to_vec()),
    };
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let repo = &h.repo;
    if repo.is_stacked(&line)? && !matches!(sub, "list" | "show" | "drop" | "clear") {
        return Err(exit(
            1,
            "error: layr park is not available in a line with overlays yet; save the changes or use 'layr save'",
        ));
    }
    move_old_entries(ctx, repo)?;
    match sub {
        "list" => {
            for (i, l) in parked_lines(repo, &line)?.iter().enumerate() {
                let message = repo.state(&l.head).map(|s| s.message).unwrap_or_default();
                outln!(ctx, "park@{{{i}}}: {}: {}", l.name, message.lines().next().unwrap_or(""));
            }
            Ok(0)
        }
        "show" => {
            let spec = Spec::new().flag("-p|--patch").flag("--stat").flag("-u|--include-untracked");
            let a = parse(&spec, &rest)?;
            let id = match entry(repo, &line, a.pos.first().map(String::as_str))? {
                Some((_, l)) => l.head,
                None => revs::resolve(repo, Some(&line), &a.pos[0])?,
            };
            let s = repo.state(&id)?;
            let Some(parent) = s.parents.first() else { bail!("state {} has no parent", ids::short(&id)) };
            let parent = repo.state_src(parent)?;
            let root = repo.state_src(&id)?;
            let list = changes::file_changes(&parent, &root)?.into_iter().filter(|c| !skip_path(&c.path)).collect::<Vec<_>>();
            let fds = diff::file_diffs(&parent, &root, &list)?;
            if a.has("-p") {
                for f in &fds {
                    ctx.print(&diff::patch(f, 3));
                }
            } else {
                ctx.print(&diff::stat(&fds, 80));
            }
            Ok(0)
        }
        "drop" | "clear" => {
            repo.authorize_line(ctx, &line, crate::repo::LineOp::Direct)?;
            let targets: Vec<(usize, Line)> = if sub == "clear" {
                parked_lines(repo, &line)?.into_iter().enumerate().collect()
            } else {
                let arg = rest.first().map(String::as_str);
                vec![entry(repo, &line, arg)?
                    .ok_or_else(|| exit(1, format!("error: {} is no park entry of this line", arg.unwrap_or("park@{0}"))))?]
            };
            let _lock = repo.p.lock()?;
            for (i, l) in targets {
                drop_entry(ctx, repo, &l)?;
                if sub == "drop" {
                    outln!(ctx, "Dropped park@{{{i}}} {} ({})", l.name, ids::short(&l.head));
                }
            }
            Ok(0)
        }
        "pop" | "apply" => {
            let arg = rest.iter().find(|r| !r.starts_with('-')).map(String::as_str);
            let found = entry(repo, &line, arg)?;
            let id = match (&found, sub) {
                (Some((_, l)), _) => l.head.clone(),
                // `apply` takes any state with a parent (a dropped entry); `pop` needs a park entry.
                (None, "apply") => revs::resolve(repo, Some(&line), arg.unwrap_or_default())?,
                (None, _) => return Err(exit(1, format!("error: {} is no park entry of this line", arg.unwrap_or_default()))),
            };
            let s = repo.state(&id)?;
            let Some(base_id) = s.parents.first().cloned() else { bail!("state {} has no parent", ids::short(&id)) };
            let (_lock, w) = begin_change(ctx, repo, &line)?;
            let line = repo.line(&line.id)?;
            let scan = repo.scan(&line)?;
            let base = repo.state_root(&base_id)?;
            let theirs = repo.state_root(&id)?;
            let work = repo.temp_work(&scan.path)?;
            let cfg = |k: &str| repo.p.config_value(k).ok().flatten().and_then(|v| v.as_str().map(|s| s.to_string()));
            let out = crate::merge::merge(&crate::merge::Input {
                base: &base,
                ours: &scan.path,
                theirs: &theirs,
                result: &work.path,
                base_id: &base_id,
                ours_id: &line.head,
                theirs_id: &id,
                ours_label: "Updated upstream",
                theirs_label: "Parked changes",
                config: &cfg,
                run_as: ctx.caller.driver_ids(),
                allow_root: ctx.caller.is_root(),
            })?;
            let result = repo.temp_snapshot(&work.path, true)?;
            let wdir = repo.working(&line);
            let own = repo.own(&line);
            for c in changes::file_changes(&scan.path, &result.path)? {
                if skip_path(&c.path) {
                    continue;
                }
                if let Some(src) = &c.old_path {
                    fsutil::remove_entry(&wdir, src)?;
                }
                if c.new.is_none() {
                    fsutil::remove_entry(&wdir, &c.path)?;
                } else {
                    fsutil::copy_work_entry(&result.path, &c.path, &wdir, &c.path, own)?;
                }
            }
            for m in &out.messages {
                outln!(ctx, "{m}");
            }
            record_working(ctx, repo, &line, &format!("park.{sub}"), w)?;
            if !out.conflicts.is_empty() {
                outln!(ctx, "The park entry is kept in case you need it again.");
                return Ok(1);
            }
            if let (Some((i, parked)), "pop") = (found, sub) {
                drop_entry(ctx, repo, &repo.line(&parked.id)?)?;
                outln!(ctx, "Dropped park@{{{i}}} {} ({})", parked.name, ids::short(&id));
            }
            Ok(0)
        }
        _ => push(ctx, repo, &line, &rest),
    }
}

/// Delete a parked line. The caller holds the project lock. A parked line has no folder: the
/// record alone deletes it, and `undo` brings it back without a folder.
fn drop_entry(ctx: &Ctx, repo: &Repo, parked: &Line) -> Result<()> {
    let made = repo.working(parked).exists();
    super::branch::delete_recorded(ctx, repo, parked, None, made)
}

/// `park [-m <message>] [-u] [-a] [-k]`: save the changes as the head of a new parked line, then
/// reset the working folder to the head (with `-k`, to the index).
fn push(ctx: &Ctx, repo: &Repo, line: &Line, rest: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-m|--message")
        .flag("-u|--include-untracked")
        .flag("-a|--all")
        .flag("-q|--quiet")
        .flag("-k|--keep-index");
    let a = parse(&spec, rest)?;
    repo.require(ctx, "line.create")?;
    let untracked = a.has("-u") || a.has("-a");
    let ignored = a.has("-a");
    // APFS: the status, the park tree and its snapshot copy go before the store lock (copies of
    // the working folder and of the head took minutes under it). Under the lock they hold when the
    // head and the index did not change.
    #[cfg(target_os = "macos")]
    let pre = (|| -> Result<_> {
        let token = repo.index_token(line)?;
        let (st, scan, _, _) = repo.status(line)?;
        let (work, paths) = park_tree(repo, line, &st, &scan.path, untracked, ignored)?;
        crate::storage::prepare_snapshot(&work.path, &repo.p.work_dir(), true)?;
        Ok((token, st, scan, work, paths))
    })()
    .ok();
    let r = (|| -> Result<i32> {
        let (_lock, w) = begin_change(ctx, repo, line)?;
        let line = repo.line(&line.id)?;
        #[cfg(target_os = "macos")]
        let pre = match pre {
            Some((token, st, scan, work, paths)) if token == repo.index_token(&line)? => Some((st, scan, work, paths)),
            _ => None,
        };
        #[cfg(not(target_os = "macos"))]
        let pre: Option<(Status, Temp, Temp, BTreeSet<String>)> = None;
        let (st, scan, prepared) = match pre {
            Some((st, scan, work, paths)) => (st, scan, Some((work, paths))),
            None => {
                let (st, scan, _, _) = repo.status(&line)?;
                (st, scan, None)
            }
        };
        let (index, _it) = repo.index_view(&line)?;
        if st.clean() && (!untracked || st.untracked.is_empty()) && (!ignored || st.ignored.is_empty()) {
            outln!(ctx, "No local changes to save");
            return Ok(0);
        }
        let head = repo.state(&line.head)?;
        let head_root = repo.head_src(&line)?;
        let own = repo.own(&line);
        let (work, paths) = match prepared {
            Some(p) => p,
            None => park_tree(repo, &line, &st, &scan.path, untracked, ignored)?,
        };
        let msg = match a.get("-m") {
            Some(m) => format!("On {}: {m}", line.name),
            None => format!("WIP on {}: {} {}", line.name, ids::short(&line.head), head.message.lines().next().unwrap_or("")),
        };
        let mut parked = Line {
            id: ids::new_id(),
            name: next_name(repo, &line)?,
            head: String::new(),
            owner: line.owner,
            machine: repo.p.store.machine.id.clone(),
        };
        let state = repo.new_state(ctx, &work.path, StateKind::Commit, vec![line.head.clone()], &msg, Some(&parked))?;
        parked.head = state.id.clone();
        // Back to the head content, or with --keep-index to the index content (the index stays).
        // New files that went into the park entry leave the working folder.
        let keep_index = a.has("-k");
        let target: &Snap = if keep_index { &index } else { &head_root };
        let wdir = repo.working(&line);
        let wtree = crate::tree::Tree::open(&wdir)?;
        for p in &paths {
            if fsutil::lmeta(target, p).is_some() {
                fsutil::copy_work_entry(target, p, &wdir, p, own)?;
            } else if fsutil::lmeta(&index, p).is_some() || fsutil::lmeta(&head_root, p).is_some() || untracked {
                fsutil::remove_entry(&wdir, p)?;
                prune_empty_in(&wtree, p);
            }
        }
        if !keep_index {
            repo.drop_stage(&line)?;
        }
        // One record: the parked line and the reset of the working folder.
        let mut rec = Record::new("park");
        rec.data = serde_json::json!({"name": parked.name, "start": state.id});
        rec.states.push(state.clone());
        let working_after = repo.after_state(ctx, &line, "park", &mut rec)?;
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                before: Some(repo.line_state(&line, &line.head)),
                after: Some(repo.line_state(&line, &line.head)),
                working_before: w,
                working_after,
            },
        );
        rec.lines.insert(
            parked.id.clone(),
            LineChange {
                before: None,
                after: Some(repo.line_state(&parked, &state.id)),
                working_before: None,
                working_after: None,
            },
        );
        repo.commit_record(ctx, rec)?;
        if !a.has("-q") {
            outln!(ctx, "Saved working directory and index state {msg} as line {}", parked.name);
        }
        Ok(0)
    })();
    repo.discard_prepared();
    r
}

/// The tree of a park entry: the head with the changed paths of the status (and the untracked or
/// ignored ones when asked) from the scan of the working folder. Returns it with the paths.
fn park_tree(
    repo: &Repo,
    line: &Line,
    st: &Status,
    scan: &Path,
    untracked: bool,
    ignored: bool,
) -> Result<(Temp, BTreeSet<String>)> {
    let work = Temp { path: repo.p.work_dir().join(crate::store::temp_name()) };
    repo.snapshot_state(&line.head, &work.path, false)?;
    let own = repo.own(line);
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for c in st.staged.iter().chain(st.unstaged.iter()) {
        paths.insert(c.path.clone());
        if let Some(o) = &c.old_path {
            paths.insert(o.clone());
        }
    }
    if untracked {
        paths.extend(st.untracked.iter().cloned());
    }
    if ignored {
        paths.extend(st.ignored.iter().cloned());
    }
    for p in &paths {
        if fsutil::lmeta(scan, p).is_some() {
            fsutil::copy_entry(scan, p, &work.path, p, own)?;
        } else {
            fsutil::remove_entry(&work.path, p)?;
        }
    }
    Ok((work, paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_names_are_read_back_only_for_their_line() {
        assert_eq!(parked_name("main", 3), "parked.main.3");
        assert_eq!(parked_number("main", "parked.main.3"), Some(3));
        assert_eq!(parked_number("a", "parked.a.b.3"), None);
        assert_eq!(parked_number("a.b", "parked.a.b.3"), Some(3));
        assert_eq!(parked_number("main", "parked.main.x"), None);
        assert_eq!(parked_number("main", "parked.main."), None);
        assert_eq!(parked_number("main", "main.3"), None);
        assert!(is_parked("parked.main.1"));
        assert!(is_parked("parked.a.b.12"));
        assert!(!is_parked("parked.main"));
        assert!(!is_parked("main"));
    }
}
