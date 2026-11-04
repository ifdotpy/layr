//! merge, cherry-pick, revert and rebase.

use super::{exit, here, work};
use crate::args::{parse, Spec};
use crate::changes;
use crate::ctx::Ctx;
use crate::diff;
use crate::ids;
use crate::merge;
use crate::model::{Line, Record, StateKind, StateRec};
use crate::overlay::{Layer, MergeKind, Mode};
use crate::repo::{skip_path, Repo};
use crate::revs;
use anyhow::Result;

/// Merge `theirs` into `ours` with `base`. Returns the new state (snapshot written, no record)
/// and the merge outcome.
#[allow(clippy::too_many_arguments)]
pub fn merge_states(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    base: &str,
    ours: &str,
    theirs: &str,
    theirs_label: &str,
    parents: Vec<String>,
    message: &str,
) -> Result<(StateRec, merge::Outcome)> {
    #[cfg(target_os = "macos")]
    let prepared = PREPARED_MERGE
        .with(|p| p.borrow_mut().take())
        .filter(|p| p.line == line.id && p.base == base && p.ours == ours && p.theirs == theirs && p.label == theirs_label);
    #[cfg(target_os = "macos")]
    let prepared = prepared.map(|p| (p.work, p.out));
    #[cfg(not(target_os = "macos"))]
    let prepared = None;
    let (work, out) = match prepared {
        Some(p) => p,
        None => merge_trees(ctx, repo, line, base, ours, theirs, theirs_label)?,
    };
    let mut st = repo.new_state(ctx, &work.path, StateKind::Commit, parents, message, Some(line))?;
    st.conflicts = out.conflicts.clone();
    Ok((st, out))
}

/// The three-way merge of the trees of three states into a new writable folder.
fn merge_trees(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    base: &str,
    ours: &str,
    theirs: &str,
    theirs_label: &str,
) -> Result<(crate::repo::Temp, merge::Outcome)> {
    merge_trees_at(ctx, repo, line, base, ours, &repo.state_root(ours)?, theirs, theirs_label)
}

/// `merge_trees` with the tree of `ours` at `ours_root` (a merge result that is no state yet).
#[allow(clippy::too_many_arguments)]
fn merge_trees_at(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    base: &str,
    ours: &str,
    ours_root: &std::path::Path,
    theirs: &str,
    theirs_label: &str,
) -> Result<(crate::repo::Temp, merge::Outcome)> {
    let base_root = repo.state_root(base)?;
    let theirs_root = repo.state_root(theirs)?;
    let work = repo.temp_work(ours_root)?;
    let cfg = |k: &str| repo.p.config_value(k).ok().flatten().and_then(|v| v.as_str().map(|s| s.to_string()));
    let out = merge::merge(&merge::Input {
        base: &base_root,
        ours: ours_root,
        theirs: &theirs_root,
        result: &work.path,
        base_id: base,
        ours_id: ours,
        theirs_id: theirs,
        ours_label: "head",
        theirs_label,
        config: &cfg,
        run_as: ctx.caller.driver_ids(),
        allow_root: ctx.caller.is_root(),
    })?;
    let own = repo.own(line);
    if let Some((u, g)) = own {
        for p in &out.changed {
            if let Ok(full) = crate::fsutil::safe_join(&work.path, p) {
                if full.symlink_metadata().is_ok() {
                    crate::fsutil::chown_nofollow(&full, u, g)?;
                }
            }
        }
    }
    Ok((work, out))
}

/// A merge computed before the store lock (APFS): the states are immutable, so their merge
/// needs no lock. Copying and merging the trees took minutes under the lock in a large project,
/// and every other command of the project waited. Under the lock `merge_states` takes this
/// result when the line, its head and the source did not move; else it merges again.
#[cfg(target_os = "macos")]
struct PreparedMerge {
    line: String,
    base: String,
    ours: String,
    theirs: String,
    label: String,
    work: crate::repo::Temp,
    out: merge::Outcome,
}
#[cfg(target_os = "macos")]
thread_local! {
    static PREPARED_MERGE: std::cell::RefCell<Option<PreparedMerge>> = const { std::cell::RefCell::new(None) };
}

/// The reads of `merge <rev>` before the store lock (APFS): the automatic state of the working
/// folder, the status, the trees of the states, and the merge result. Errors only lose the
/// preparation; the merge under the lock reads everything again.
#[cfg(target_os = "macos")]
fn prepare_merge(ctx: &Ctx, repo: &Repo, line: &Line, rev: &str) -> Result<()> {
    let _t = crate::trace::span("prepare merge");
    repo.prepare_save(line)?;
    repo.prepare_status(line)?;
    let theirs = revs::resolve(repo, Some(line), rev)?;
    let head = line.head.as_str();
    if theirs == head || repo.is_ancestor(&theirs, head)? {
        return Ok(());
    }
    if repo.is_ancestor(head, &theirs)? {
        // A fast-forward reads the tree of the new head.
        repo.state_root(&theirs)?;
        return Ok(());
    }
    let Some(base) = repo.merge_base(head, &theirs)? else { return Ok(()) };
    let label = repo.p.db.line_by_name(rev)?.map(|l| l.name).unwrap_or_else(|| rev.to_string());
    let (work, out) = merge_trees(ctx, repo, line, &base, head, &theirs, &label)?;
    crate::storage::prepare_snapshot(&work.path, &repo.p.work_dir(), true)?;
    PREPARED_MERGE.with(|p| {
        *p.borrow_mut() = Some(PreparedMerge { line: line.id.clone(), base, ours: head.to_string(), theirs, label, work, out })
    });
    Ok(())
}

/// One merge of a chain (`replay`, `pick`) made before the store lock (APFS).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct PreparedStep {
    base: String,
    theirs: String,
    label: String,
    work: crate::repo::Temp,
    out: merge::Outcome,
}

/// The prepared merges of a chain under the lock: each step hands its result to
/// `merge_states` when it is the next one; the first step that is not drops the rest.
type Steps = std::iter::Peekable<std::vec::IntoIter<PreparedStep>>;

/// Give `merge_states` the prepared merge of (`base`, `theirs`) over `ours` when it is the next
/// one of `steps`. Its conflicts get the real state of `ours`.
#[cfg(target_os = "macos")]
fn hand_over(steps: &mut Steps, line: &Line, base: &str, ours: &str, theirs: &str, label: &str) {
    match steps.next_if(|s| s.base == base && s.theirs == theirs && s.label == label) {
        Some(step) => {
            let mut out = step.out;
            for conflict in &mut out.conflicts {
                conflict.ours = Some(ours.to_string());
            }
            PREPARED_MERGE.with(|m| {
                *m.borrow_mut() = Some(PreparedMerge {
                    line: line.id.clone(),
                    base: base.to_string(),
                    ours: ours.to_string(),
                    theirs: theirs.to_string(),
                    label: label.to_string(),
                    work: step.work,
                    out,
                })
            });
        }
        // A later prepared merge was made over this one: none of them holds any more.
        None => *steps = Vec::new().into_iter().peekable(),
    }
}

/// The merges of a chain before the store lock (APFS): each (base, theirs, label) merges onto
/// the result of the one before it, the first onto `onto`, in temporary folders, with a
/// prepared snapshot copy of each result. Copying and merging the trees of every state took
/// minutes under the lock.
#[cfg(target_os = "macos")]
fn prepare_steps(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    onto: &str,
    chain: Vec<(String, String, String)>,
) -> Result<Vec<PreparedStep>> {
    let mut steps: Vec<PreparedStep> = Vec::new();
    for (base, theirs, label) in chain {
        let ours_root = match steps.last() {
            Some(s) => s.work.path.clone(),
            None => repo.state_root(onto)?,
        };
        let (work, out) = merge_trees_at(ctx, repo, line, &base, "", &ours_root, &theirs, &label)?;
        crate::storage::prepare_snapshot(&work.path, &repo.p.work_dir(), true)?;
        steps.push(PreparedStep { base, theirs, label, work, out });
    }
    Ok(steps)
}

/// The merges of `replay` before the store lock (APFS), for the line at its head onto `onto`.
#[cfg(target_os = "macos")]
fn prepare_replay(ctx: &Ctx, repo: &Repo, line: &Line, upstream: &str, onto: &str) -> Result<Vec<PreparedStep>> {
    let _t = crate::trace::span("prepare replay");
    repo.prepare_status(line)?;
    let Some(base) = repo.merge_base(&line.head, upstream)? else { return Ok(Vec::new()) };
    let mut chain = repo.first_parent_chain(&line.head, Some(&base))?;
    chain.reverse();
    let mut list = Vec::new();
    for p in chain {
        let c = repo.state(&p)?;
        if c.parents.len() == 1 {
            list.push((c.parents[0].clone(), p.clone(), ids::short(&p)));
        }
    }
    prepare_steps(ctx, repo, line, onto, list)
}

/// Delete what the preparation left (after the lock).
fn discard_prepared(repo: &Repo) {
    #[cfg(target_os = "macos")]
    PREPARED_MERGE.with(|p| p.borrow_mut().take());
    repo.discard_prepared();
}

/// The merge of the follow overlays of two lines (docs/overlays.md, section 1): it goes with the
/// merge of the shared set, and the user who merges must read each of them.
#[derive(Default)]
pub struct OverlayMerge {
    /// The stack of the line after the merge.
    layers: Vec<Layer>,
    /// Overlay, its line of the merged line, the new head, the new state (none for a fast-forward).
    moves: Vec<(String, Line, String, Option<StateRec>)>,
    messages: Vec<String>,
    conflicts: usize,
}

impl OverlayMerge {
    fn changed(&self) -> bool {
        !self.moves.is_empty()
    }

    fn names(&self) -> Vec<String> {
        self.moves.iter().map(|m| m.0.clone()).collect()
    }

    fn discard(&self, repo: &Repo) {
        for (name, _, _, st) in &self.moves {
            if let (Some(st), Ok(o)) = (st, repo.overlay_repo(name)) {
                o.discard_states(std::slice::from_ref(st));
            }
        }
    }

    /// The records in the overlays, before the record of the line.
    fn record(&self, ctx: &Ctx, repo: &Repo) -> Result<()> {
        for (name, ol, head, st) in &self.moves {
            let o = repo.overlay_repo(name)?;
            let _lock = o.p.lock()?;
            crate::overlay::move_overlay_line(&o, ctx, "merge", ol, head, st.iter().cloned().collect())?;
        }
        Ok(())
    }
}

/// Merge the follow overlays of `theirs` into those of `line`: a fast-forward, or a three-way merge
/// in the overlay (states written, no records yet).
fn plan_overlays(ctx: &Ctx, repo: &Repo, line: &Line, theirs: Option<&Line>, label: &str, message: &str) -> Result<OverlayMerge> {
    let ours = repo.layers(line)?;
    let mut m = OverlayMerge { layers: ours.clone(), ..Default::default() };
    let theirs = match theirs {
        Some(t) => t,
        None => return Ok(m),
    };
    let r = (|| -> Result<()> {
        for tl in repo.layers(theirs)? {
            let info = match repo.overlay_info(&tl.overlay)? {
                Some(i) if i.mode == Mode::Follow => i,
                _ => continue,
            };
            if !repo.reads_overlay(&tl.overlay, ctx.caller.uid, ctx.caller.gid)? {
                return Err(exit(
                    1,
                    format!("error: merging line '{}' needs the read right of its overlay '{}'", theirs.name, tl.overlay),
                ));
            }
            let i = match ours.iter().position(|l| l.overlay == tl.overlay) {
                Some(i) => i,
                None => {
                    return Err(exit(
                        1,
                        format!(
                            "error: line '{}' shows overlay '{}' (follow mode), which line '{}' does not use; run 'layr overlay use --add {}' in '{}' first",
                            theirs.name, tl.overlay, line.name, tl.overlay, line.name
                        ),
                    ))
                }
            };
            let ours_s = ours[i].state.clone();
            let o = repo.overlay_repo(&tl.overlay)?;
            if tl.state == ours_s || o.is_ancestor(&tl.state, &ours_s)? {
                continue;
            }
            let ol =
                o.p.db
                    .line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))?
                    .ok_or_else(|| exit(1, format!("error: line '{}' has no line in overlay '{}'", line.name, tl.overlay)))?;
            if o.is_ancestor(&ours_s, &tl.state)? {
                m.layers[i].state = tl.state.clone();
                m.moves.push((tl.overlay.clone(), ol, tl.state.clone(), None));
                continue;
            }
            if repo.overlay_props(&tl.overlay)?.merge == MergeKind::UnionTheirs {
                let st = crate::overlay::union_state(&o, ctx, &ol, &ours_s, &tl.state, message)?;
                m.layers[i].state = st.id.clone();
                m.moves.push((tl.overlay.clone(), ol, st.id.clone(), Some(st)));
                continue;
            }
            let base = o
                .merge_base(&ours_s, &tl.state)?
                .ok_or_else(|| exit(128, format!("fatal: overlay '{}': refusing to merge unrelated histories", tl.overlay)))?;
            if !o.p.has_state(&base) {
                return Err(exit(
                    1,
                    format!(
                        "error: overlay '{}': the merge base {} was deleted by its history limit; use 'layr overlay set {} --merge union-theirs'",
                        tl.overlay,
                        crate::ids::short(&base),
                        tl.overlay
                    ),
                ));
            }
            let (st, out) =
                merge_states(ctx, &o, &ol, &base, &ours_s, &tl.state, label, vec![ours_s.clone(), tl.state.clone()], message)?;
            m.conflicts += out.conflicts.len();
            m.messages.extend(out.messages.iter().map(|x| format!("overlay {}: {x}", tl.overlay)));
            m.layers[i].state = st.id.clone();
            m.moves.push((tl.overlay.clone(), ol, st.id.clone(), Some(st)));
        }
        Ok(())
    })();
    if let Err(e) = r {
        m.discard(repo);
        return Err(e);
    }
    Ok(m)
}

/// The checks of a merge that ran before its commit, with what they were based on. The commit
/// reads them again under the project lock (`settled_now`): a change of the rule of the line, or
/// of a check command, stops the merge.
struct Settled<'a> {
    expect: bool,
    rule: Option<&'a crate::access::Rule>,
    /// The reviewed source state.
    source: &'a str,
    /// The check commands of the line's rule, by name.
    commands: Vec<(String, String)>,
    /// The name the merge was asked for, when it is not a queued state. The commit resolves it again:
    /// a source that moved while the merge waited for the project lock is refused.
    rev: Option<String>,
}

/// The check commands of a rule: each name with its command. An undefined name is an error.
fn resolve_commands(repo: &Repo, line: &Line, checks: &[String]) -> Result<Vec<(String, String)>> {
    let mut commands = Vec::new();
    for name in checks {
        let cmd =
            repo.p.config_value(&format!("check.{name}"))?.and_then(|v| v.as_str().map(|s| s.to_string())).ok_or_else(|| {
                exit(
                    1,
                    format!(
                        "error: check '{name}' of line '{}' is not defined (layr config check.{name} '<command>')",
                        line.name
                    ),
                )
            })?;
        commands.push((name.clone(), cmd));
    }
    Ok(commands)
}

/// The checks of a merge, read again under the project lock before the records: the rule of the
/// line, the check commands and, for a merge of another state, the approvals of the source.
fn settled_now(ctx: &Ctx, repo: &Repo, now: &Line, head: &str, candidate: &str, s: &Settled<'_>) -> Result<()> {
    if let Some(rev) = &s.rev {
        let at = revs::resolve(repo, Some(now), rev)?;
        if at != s.source {
            if s.expect {
                return Err(exit(
                    1,
                    format!(
                        "error: {rev} is at {} but --expect names {}: the line changed after the review; review the new state first",
                        ids::short(&at),
                        ids::short(s.source)
                    ),
                ));
            }
            return Err(crate::repo::line_moved(rev, s.source, &at));
        }
    }
    let rule = repo.authorize_line(ctx, now, crate::repo::LineOp::Merge { expect: s.expect, hint: None })?;
    if rule.as_ref() != s.rule {
        return Err(exit(1, "merge protection changed during checks; run the merge again"));
    }
    for (name, command) in &s.commands {
        if repo.p.config_value(&format!("check.{name}"))?.and_then(|v| v.as_str().map(str::to_owned)).as_ref() != Some(command) {
            return Err(exit(1, format!("merge check '{name}' changed during checks; run the merge again")));
        }
    }
    if candidate != head {
        check_approvals(ctx, repo, now, rule.as_ref(), head, s.source)?;
    }
    Ok(())
}

/// A command's result before its commit: the states it made, the working folder it wrote and the
/// overlays it moved. Dropped before `done`, it puts the folder back and deletes the states that no
/// record names. The commit happens only after this guard is complete.
struct Pending<'a> {
    repo: &'a Repo<'a>,
    write: Option<crate::repo::WorkingWrite<'a>>,
    states: Vec<StateRec>,
    ovl: Option<&'a OverlayMerge>,
    done: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // The folder is written back first: its source is the state that the next step deletes.
        drop(self.write.take());
        crate::repo::discard_unrecorded(self.repo, &self.states);
        if let Some(o) = self.ovl {
            o.discard(self.repo);
        }
    }
}

/// Move the line to `new_head` and update the working folder in place. Under the line lock, which the
/// caller holds, it checks and writes the folder; under the project lock it checks the line head and
/// records the move (`Repo::commit_line`). The project lock covers no tree work: the tree of the new
/// head and the write of the folder come before it, and a failed commit writes the folder back.
#[allow(clippy::too_many_arguments)]
fn finish(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    op: &str,
    new_head: &str,
    states: Vec<StateRec>,
    working_before: Option<String>,
    local: &std::collections::BTreeSet<String>,
    data: serde_json::Value,
    ovl: Option<&OverlayMerge>,
    queue: Option<&QueuedMerge<'_>>,
    settled: Option<&Settled<'_>>,
) -> Result<Vec<changes::Change>> {
    let op = if queue.is_some() { "queue.result" } else { op };
    let mut pending = Pending { repo, write: None, states, ovl, done: false };
    repo.check_move(ctx, line, new_head)?;
    if op == "merge" && new_head != line.head {
        crate::secrets::check_between(ctx, repo, &repo.state_src(&line.head)?, &repo.state_src(new_head)?, &line.head)?;
    }
    // A line with overlays: the full trees of the old and the new head, with the overlays in
    // place, so the update never touches an overlay file it does not change.
    let layers = match ovl {
        Some(o) if o.changed() => o.layers.clone(),
        _ => repo.layers(line)?,
    };
    let view = if layers.is_empty() { None } else { Some(repo.assemble(line, new_head, &layers)?) };
    let old_root = repo.head_tree(line)?;
    let new_root = match &view {
        Some(v) => v.path.clone(),
        None => repo.state_root(new_head)?,
    };
    let wdir = repo.working(line);
    let own = repo.own(line);
    let applied = repo.apply_delta(&old_root, &new_root, &wdir, own, local, false)?;
    pending.write = Some(crate::repo::WorkingWrite::new(repo, wdir, own, old_root, new_root, view));
    let mut data = data;
    if let Some(o) = ovl.filter(|o| o.changed()) {
        data["stack"] = repo.public_stack_data(&line.id, &layers)?;
        data["line"] = serde_json::json!(line.id);
        data["overlays"] = serde_json::json!(o
            .names()
            .into_iter()
            .filter(|n| !repo.need_overlay(n).map(|i| i.hidden).unwrap_or(true))
            .collect::<Vec<_>>());
    }
    let mut rec = repo.head_record(ctx, op, line, new_head, pending.states.clone(), working_before, data)?;
    // The states of the record include the snapshot of the working folder after the move.
    pending.states = rec.states.clone();
    let view = repo.commit_line(line, &line.head, |now| {
        if let Some(q) = queue {
            rec.data =
                super::queue::complete_data(repo, q.entry, new_head, q.attempts, q.probes.borrow().clone(), rec.data.clone())?;
        }
        match settled {
            Some(s) => settled_now(ctx, repo, now, &line.head, new_head, s)?,
            // A pick or a rebase is a direct change of the line: its authorization holds at the commit.
            None => {
                repo.authorize_line(ctx, now, crate::repo::LineOp::Direct)?;
            }
        }
        if let Some(o) = ovl.filter(|o| o.changed()) {
            o.record(ctx, repo)?;
        }
        repo.commit_record(ctx, rec)?;
        Ok(())
    });
    if let Err(e) = view {
        return Err(e);
    }
    pending.done = true;
    let view = pending.write.take().and_then(|w| w.commit());
    if let Some(v) = view {
        repo.install_base(line, v, new_head, &layers)?;
    }
    for name in ovl.filter(|o| o.changed()).map(|o| o.names()).unwrap_or_default() {
        repo.after_overlay_move(ctx, &name);
    }
    repo.after_head_moved(line, new_head);
    Ok(applied)
}

/// Path rules of a protected line: every path that the merge changes needs the role of its
/// rule (`layr protect <line> --path <path>=<role>`).
fn check_paths(ctx: &Ctx, repo: &Repo, line: &Line, rule: Option<&crate::access::Rule>, head: &str, result: &str) -> Result<()> {
    crate::secrets::check_between(ctx, repo, &repo.state_src(head)?, &repo.state_src(result)?, head)?;
    let rule = match rule {
        Some(r) if !r.paths.is_empty() && !ctx.caller.is_root() => r,
        _ => return Ok(()),
    };
    let role = repo.policy().role(&Repo::who(ctx));
    for c in changes::file_changes(&repo.state_src(head)?, &repo.state_src(result)?)? {
        for p in [Some(c.path.clone()), c.old_path.clone()].into_iter().flatten() {
            let requirement =
                layr_model::protection::path_requirements([p.as_str()], role, |path| rule.role_for_path(path)).remove(0);
            let need = requirement.role;
            if requirement.met != Some(true) {
                return Err(exit(
                    1,
                    format!(
                        "permission denied: the merge changes '{p}', which in line '{}' needs the role {}; you are {}",
                        line.name,
                        need.name(),
                        role.name()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Approvals of a protected line: the reviewed state (`theirs`) needs the rule's number of
/// approvals of exactly that state, from users who may approve and who are not authors of the
/// states the merge brings in.
fn check_approvals(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    rule: Option<&crate::access::Rule>,
    head: &str,
    theirs: &str,
) -> Result<()> {
    let rule = match rule {
        Some(r) if r.approvals > 0 && !ctx.caller.is_root() => r,
        _ => return Ok(()),
    };
    let count = approval_count(repo, rule, head, theirs)?;
    if (count as u32) < rule.approvals {
        let from: Vec<String> = rule.approvers.iter().map(|k| crate::access::show_principal(k)).collect();
        return Err(exit(
            1,
            format!(
                "error: line '{}' takes state {} after {} approval(s) of exactly that state by {} other than its authors; it has {}",
                line.name,
                ids::short(theirs),
                rule.approvals,
                if from.is_empty() { "maintainers".to_string() } else { from.join(" and ") },
                count
            ),
        ));
    }
    Ok(())
}

/// The approvals of `theirs` that a merge into a line with `rule` at `head` counts.
fn approval_count(repo: &Repo, rule: &crate::access::Rule, head: &str, theirs: &str) -> Result<usize> {
    let pol = repo.policy();
    let counted = layr_model::protection::approvals(
        theirs,
        Some(head),
        layr_model::protection::ApprovalMode::Merge,
        &repo.p.db.approvals(theirs)?,
        |id| {
            let state = repo.p.db.state(id)?;
            if state.as_ref().is_some_and(|state| state.kind.auxiliary()) {
                repo.source_state(id)?;
            }
            Ok(state)
        },
        |uid| pol.may_approve(rule, &crate::access::Who::of(uid, crate::ctx::Caller::from_uid(uid).gid)),
    )?;
    Ok(counted.len()
        + crate::access::person_approval_count(
            repo,
            &pol,
            rule,
            Some(head),
            theirs,
            layr_model::protection::ApprovalMode::Merge,
        )?)
}

/// Whether a finished merge of `theirs` into a protected line (from `before` to `after`) met the
/// line's protection, with no bypass: each required check passed on the new head, and `theirs`
/// has the required approvals. Only such a merge carries the source line's caches
/// (docs/cache-overlays.md, section 5): a cache holds code that review does not see.
pub(crate) fn merge_met_rule(repo: &Repo, rule: &crate::access::Rule, before: &str, after: &str, theirs: &str) -> Result<bool> {
    for name in &rule.check {
        let Some(cmd) = repo.p.config_value(&format!("check.{name}"))?.and_then(|v| v.as_str().map(str::to_owned)) else {
            return Ok(false);
        };
        let argv = layr_model::protection::check_argv(&cmd);
        if repo.p.db.check_exit(after, &argv, Some(0))?.is_none() {
            return Ok(false);
        }
    }
    Ok(rule.approvals == 0 || approval_count(repo, rule, before, theirs)? as u32 >= rule.approvals)
}

/// Run the checks of a merge on the candidate state. Callers run it without the line lock
/// (`checks_unlocked`).
fn run_line_checks(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    head: &str,
    candidate: &str,
    settled: &Settled<'_>,
    quiet: bool,
    queue: Option<&QueuedMerge<'_>>,
) -> Result<()> {
    for (name, command) in &settled.commands {
        let argv = layr_model::protection::check_argv(command);
        let evidence = layr_model::protection::required_check(
            name,
            Some(command),
            repo.p.db.check_exit(candidate, &argv, Some(0))?.is_some(),
            None,
        );
        if evidence.result == layr_model::protection::CheckResult::Passed {
            continue;
        }
        let code = super::admin::run_check(ctx, repo, candidate, &argv, command, quiet)?;
        if code != 0 {
            return Err(exit(
                1,
                format!(
                    "error: check '{name}' failed (exit {code}) on {}, the state '{}' would take; the line stays at {}",
                    ids::short(candidate),
                    line.name,
                    ids::short(head)
                ),
            ));
        }
    }
    if let Some(q) = queue.filter(|q| q.run_probes) {
        let outcome = crate::probes::queue_run(ctx, repo, head, candidate)?;
        let reason = outcome["probes"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|probe| probe["reasons"].as_array().into_iter().flatten())
            .filter_map(serde_json::Value::as_str)
            .next()
            .map(str::to_owned);
        *q.probes.borrow_mut() = Some(outcome);
        if let Some(reason) = reason {
            anyhow::bail!("{reason}");
        }
    }
    Ok(())
}

/// Run the checks of a merge without the line lock, then take the lock again. A check may save
/// this line meanwhile (a concurrent command does, and a queue check can): the lock is not held
/// then, so neither waits for the other. A head that moved refuses the merge, which the caller
/// runs again (a queued merge retries once). Returns the local paths that the merge must not
/// overwrite, read under the lock.
#[allow(clippy::too_many_arguments)]
fn checks_unlocked(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    head: &str,
    candidate: &str,
    settled: &Settled<'_>,
    quiet: bool,
    queue: Option<&QueuedMerge<'_>>,
    held: &mut Option<crate::store::LineLock>,
) -> Result<std::collections::BTreeSet<String>> {
    held.take();
    run_line_checks(ctx, repo, line, head, candidate, settled, quiet, queue)?;
    *held = Some(repo.line_lock(line)?);
    let now = repo.line(&line.id)?;
    if now.head != head {
        return Err(crate::repo::line_moved(&line.name, head, &now.head));
    }
    let (st, _scan, _it, _idx) = repo.status(line)?;
    local_paths(repo, line, &st)
}

/// Paths a merge must not overwrite: local changes, and in a line with overlays also ignored
/// files (a user's own key where an overlay has one).
/// A new state would drop the open conflicts of the head and keep their markers as content.
/// Only files with markers matter: other kinds stay listed until the next commit.
fn refuse_open_conflicts(st: &crate::repo::Status) -> Result<()> {
    let paths: Vec<&str> =
        st.conflicts.iter().filter(|c| c.kind == "content" || c.kind == "add/add").map(|c| c.path.as_str()).collect();
    if paths.is_empty() {
        return Ok(());
    }
    Err(exit(
        1,
        format!("error: unresolved conflicts in {}: fix the files and 'layr save -m <message>' first.", paths.join(", ")),
    ))
}

fn local_paths(repo: &Repo, line: &Line, st: &crate::repo::Status) -> Result<std::collections::BTreeSet<String>> {
    let mut local = st.local_paths();
    if !repo.layers(line)?.is_empty() {
        local.extend(st.ignored.iter().cloned());
    }
    Ok(local)
}

fn print_stat(ctx: &Ctx, repo: &Repo, from: &str, to: &str) -> Result<()> {
    let a = repo.state_src(from)?;
    let b = repo.state_src(to)?;
    let list: Vec<changes::Change> = changes::file_changes(&a, &b)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
    let fds = diff::file_diffs(&a, &b, &list)?;
    ctx.print(&diff::stat(&fds, 80));
    for f in &fds {
        match (&f.old, &f.new) {
            (None, Some(n)) => outln!(ctx, " create mode {:06o} {}", n.mode, n.path),
            (Some(o), None) => outln!(ctx, " delete mode {:06o} {}", o.mode, o.path),
            _ => {}
        }
    }
    Ok(())
}

pub fn merge(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-m|--message")
        .flag("--no-ff")
        .flag("--ff")
        .flag("--ff-only")
        .flag("--squash")
        .value("--expect")
        .flag("--abort")
        .flag("--continue")
        .flag("--no-edit")
        .flag("--no-save")
        .flag("--caches-only")
        .removed("--no-commit", "--no-save")
        .flag("-q|--quiet")
        .flag("--stat")
        .flag("--no-stat")
        .value("-s|--strategy")
        .value("-X|--strategy-option");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    if a.has("--abort") {
        return super::admin::undo_last_of(ctx, &h.repo, &line, &["merge", "pull"]);
    }
    if a.has("--continue") {
        return Err(exit(1, "layr merges never stop on a conflict: fix the files and run 'layr save -m <message>'."));
    }
    let rev = a.pos.first().ok_or_else(|| exit(128, "usage: layr merge <rev> [--expect <state>]"))?.clone();
    if a.has("--caches-only") {
        return caches_only(ctx, &h.repo, &line, &rev, a.has("-q"));
    }
    let before = line.head.clone();
    let code = merge_rev(ctx, &h.repo, &line, &rev, &a);
    discard_prepared(&h.repo);
    let code = code?;
    // The caches of a merged line go with it, taken and saved as the follow pass does (docs/cache-overlays.md,
    // section 5). They are regenerable: a failure is reported and never undoes the merge.
    if let Some(theirs) = h.repo.p.db.line_by_name(&rev)? {
        let r = (|| -> Result<()> {
            let theirs = h.repo.localize(theirs)?;
            let now = h.repo.line(&line.id)?;
<<<<<<< head
            let (taken, refused) = h.repo.take_follow_caches(ctx, &now, vec![theirs], Some(&before), false)?;
||||||| base
            let lock = h.repo.p.lock()?;
            let line = h.repo.line(&line.id)?;
            // A protected line takes caches only with a merge that met its protection.
            if let Some(rule) = h.repo.policy().rule_for(&line.name) {
                if !merge_met_rule(&h.repo, &rule, &before, &line.head, &theirs.head)? {
                    if !a.has("-q") {
                        outln!(
                            ctx,
                            "Caches of '{rev}' not merged: the merge did not pass the checks and approvals of line '{}'",
                            line.name
                        );
                    }
                    return Ok(());
=======
            let (taken, refused) = h.repo.take_follow_caches(ctx, &now, vec![theirs], Some(&before))?;
>>>>>>> origin-https/main
            if !a.has("-q") {
                for (p, id) in &taken {
                    outln!(ctx, "Merged cache {p}: {}", ids::short(id));
                }
                if let (true, Some(reason)) = (taken.is_empty(), refused) {
                    outln!(ctx, "Caches of '{rev}' not merged: {reason}");
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!("warning: caches of '{rev}' not merged: {e:#}");
        }
    }
    Ok(code)
}

/// `merge <line> --caches-only`: the caches of a line, with no tree change and no merge commit.
/// A protected line takes them only when its rule allows and the source caches name a project
/// state whose changes the target already holds (a squash merge passes).
fn caches_only(ctx: &Ctx, repo: &Repo, line: &Line, rev: &str, quiet: bool) -> Result<i32> {
    let theirs = repo.p.db.line_by_name(rev)?.ok_or_else(|| exit(1, format!("error: '{rev}' is not a line")))?;
    let theirs = repo.localize(theirs)?;
    repo.hold(repo.pin(&[&theirs.head, &line.head])?);
    // No head moves, so the reviewed-state rule of a protected line does not apply.
    repo.authorize_line(ctx, line, crate::repo::LineOp::Merge { expect: true, hint: None })?;
    let lock = repo.p.lock()?;
    let line = repo.line(&line.id)?;
    if let Some(rule) = repo.policy().rule_for(&line.name) {
        if !merge_met_rule(repo, &rule, &line.head, &line.head, &theirs.head)? {
            let reason = "checks and approvals do not allow them";
            return Err(exit(1, format!("error: caches of '{rev}' refused: line '{}' {reason}", line.name)));
        }
        if let Some(reason) = caches_contained(repo, &line, &theirs)? {
            return Err(exit(1, format!("error: caches of '{rev}' refused: {reason}")));
        }
    }
    // The copies of build output take minutes: they hold only the lock of each cache overlay.
    drop(lock);
    let none = std::collections::BTreeSet::new();
    let taken = repo.pull_caches(ctx, &line, &crate::cache_overlay::Source::Line(&theirs), &none)?;
    let folders: Vec<String> = taken.iter().map(|(f, _)| f.clone()).collect();
    if let Err(e) = crate::source_times::apply(&repo.working(&line), &folders) {
        eprintln!("warning: source file times of the caches not set: {e:#}");
    }
    for (p, id) in &taken {
        if !quiet {
            outln!(ctx, "Merged cache {p}: {}", ids::short(id));
        }
    }
    Ok(0)
}

/// The squash-safe check of a protected target: for each cache of the source, the changes from
/// the base to the project state the cache was saved with must hold the same content in the
/// target head. None when they do; else the reason, naming the first path that differs.
pub(crate) fn caches_contained(repo: &Repo, line: &Line, theirs: &Line) -> Result<Option<String>> {
    let rules = crate::ignore_rules::Rules::new(&repo.working(line));
    let head = repo.state_src(&line.head)?;
    for (name, _) in repo.folder_overlays()? {
        // The newest cache of the source line: the head of its overlay line (`cache_source`).
        let info = repo.need_overlay(&name)?;
        let orepo = repo.overlay_repo(&name)?;
        let Some(ol) = orepo.p.db.line_by_name(&Repo::overlay_line_name(&info, &theirs.id, theirs.owner))? else {
            continue;
        };
        orepo.hold(orepo.pin(&[ol.head.as_str()])?);
        let Some(saved) = orepo.p.db.state(&ol.head)?.and_then(|s| s.cache_of) else {
            return Ok(Some(format!(
                "the cache of {} names no project state; run `layr save --caches` in {} first",
                theirs.name, theirs.name
            )));
        };
        repo.hold(repo.pin(&[saved.as_str()])?);
        let Some(base) = repo.merge_base(&saved, &line.head)? else {
            return Ok(Some(format!("the cache of {} and line '{}' have no common state", theirs.name, line.name)));
        };
        let (base, saved) = (repo.state_src(&base)?, repo.state_src(&saved)?);
        for found in crate::radar::compare(&base, &saved, &rules)? {
            if crate::radar::recheck(&saved, &head, &rules, &found.path)?.is_some() {
                let (path, name) = (&found.path, &theirs.name);
                return Ok(Some(format!("'{path}' differs from the saved state of the cache of {name}")));
            }
        }
    }
    Ok(None)
}

#[derive(Debug)]
pub struct TargetMoved;
impl std::fmt::Display for TargetMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("target head changed")
    }
}
impl std::error::Error for TargetMoved {}

fn target_moved(name: &str) -> anyhow::Error {
    exit(1, format!("error: line '{name}' moved while its merge ran; merge again")).context(TargetMoved)
}

struct QueuedMerge<'a> {
    entry: &'a crate::model::QueueEntry,
    expected_head: &'a str,
    attempts: u32,
    run_probes: bool,
    probes: std::cell::RefCell<Option<serde_json::Value>>,
}

/// A queue entry pins its source and compares the target under the merge's project lock.
pub fn merge_queued(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    entry: &crate::model::QueueEntry,
    expected_head: &str,
    attempts: u32,
) -> (Result<i32>, Option<serde_json::Value>) {
    let run_probes = match crate::probes::queue_enabled(repo) {
        Ok(enabled) => enabled,
        Err(error) => return (Err(error), None),
    };
    let queue = QueuedMerge { entry, expected_head, attempts, run_probes, probes: Default::default() };
    match repo.pin(&[&entry.state, expected_head]) {
        Ok(pin) => repo.hold(pin),
        Err(error) => return (Err(error), None),
    }
    let result = (|| {
        let source = repo.source_state(&entry.state)?;
        if !source.conflicts.is_empty() {
            return Err(exit(
                1,
                format!(
                    "queued state has conflicts: {}",
                    source.conflicts.iter().map(|c| c.path.as_str()).collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        let mut args = crate::args::Parsed::default();
        args.values.insert("--expect".into(), vec![entry.state.clone()]);
        args.flags.insert("-q".into(), 1);
        merge_rev_inner(ctx, repo, line, &entry.state, &args, Some(&queue))
    })();
    (result, queue.probes.into_inner())
}

/// Internal flag of `merge_rev`: a merge with conflicts is discarded and returns an error, and the
/// line keeps its head. A follow pass sets it; a user merge never does.
pub(crate) const REFUSE_CONFLICTS: &str = "--refuse-conflicts";

pub fn merge_rev(ctx: &Ctx, repo: &Repo, line: &Line, rev: &str, a: &crate::args::Parsed) -> Result<i32> {
    merge_rev_inner(ctx, repo, line, rev, a, None)
}

fn merge_rev_inner(
    ctx: &Ctx,
    repo: &Repo,
    arg_line: &Line,
    rev: &str,
    a: &crate::args::Parsed,
    queue: Option<&QueuedMerge<'_>>,
) -> Result<i32> {
    // The line lock for the whole merge. The head is read under it, and the project lock is taken
    // only for the commit points (`commit_line`, the automatic save of the working folder).
    let mut held = Some(repo.line_lock(arg_line)?);
    let line_now = repo.line(&arg_line.id)?;
    let line = &line_now;
    // Pin the expectation before waiting, but resolve the source only under the project lock.
    let want = a
        .get("--expect")
        .map(|e| if queue.is_some() { Ok(e.to_string()) } else { revs::resolve(repo, Some(line), e) })
        .transpose()?;
    let expect = want.is_some();
    // The refusal of a protected line names the ready command; the state is read only.
    let hint = match queue {
        None if !expect => Some(format!("layr merge {rev} --expect {}", ids::display(&revs::resolve(repo, Some(line), rev)?))),
        _ => None,
    };
    repo.authorize_line(ctx, line, crate::repo::LineOp::Merge { expect, hint: hint.as_deref() })?;
    if queue.is_none() {
        repo.hold(repo.pin(&[&revs::resolve(repo, Some(line), rev)?])?);
    }
    #[cfg(target_os = "macos")]
    if queue.is_none() {
        let _ = prepare_merge(ctx, repo, line, rev);
    }
    if let Some(q) = queue {
        if line.head != q.expected_head {
            return Err(target_moved(&line.name));
        }
        if repo.p.db.queue_entry(&q.entry.id)?.as_ref() != Some(q.entry) {
            return Err(exit(1, "queue entry changed before the merge"));
        }
    }
    let (theirs, theirs_line) = match queue {
        Some(q) => (q.entry.state.clone(), None),
        None => (revs::resolve(repo, Some(line), rev)?, repo.p.db.line_by_name(rev)?),
    };
    if let Some(want) = want {
        if want != theirs {
            return Err(exit(
                1,
                format!(
                    "error: {rev} is at {} but --expect names {}: the line changed after the review; review the new state first",
                    ids::short(&theirs),
                    ids::short(&want)
                ),
            ));
        }
    }
    let rule = repo.authorize_line(ctx, line, crate::repo::LineOp::Merge { expect, hint: hint.as_deref() })?;
    // The automatic save of the working folder is a record: its project lock covers that one step.
    let w = {
        let _commit = repo.p.lock()?;
        repo.working_state(ctx, line, if queue.is_some() { "before a queued merge" } else { "before a layr command" })?
    };
    let checks = rule.as_ref().map(|r| r.check.clone()).unwrap_or_default();
    let settled = Settled {
        expect,
        rule: rule.as_ref(),
        source: &theirs,
        commands: resolve_commands(repo, line, &checks)?,
        rev: queue.is_none().then(|| rev.to_string()),
    };
    let head = line.head.clone();
    let label = theirs_line.as_ref().map(|l| l.name.clone()).unwrap_or_else(|| rev.to_string());
    let message = a.get("-m").map(|s| s.to_string()).unwrap_or_else(|| {
        if theirs_line.is_some() {
            format!("Merge line '{label}'")
        } else {
            format!("Merge {label}")
        }
    });
    let theirs_line = match theirs_line {
        Some(l) => Some(repo.localize(l)?).filter(|l| l.head == theirs),
        None => None,
    };
    let up_to_date = theirs == head || repo.is_ancestor(&theirs, &head)?;
    if up_to_date && theirs_line.as_ref().map(|t| repo.layers(t).map(|v| v.is_empty())).transpose()?.unwrap_or(true) {
        if queue.is_some() {
            if queue.is_some_and(|q| q.run_probes) {
                checks_unlocked(ctx, repo, line, &head, &head, &settled, true, queue, &mut held)?;
            }
            finish(
                ctx,
                repo,
                line,
                "merge",
                &head,
                vec![],
                w,
                &std::collections::BTreeSet::new(),
                serde_json::json!({"theirs":theirs,"up_to_date":true}),
                None,
                queue,
                Some(&settled),
            )?;
        }
        if !a.has("-q") {
            outln!(ctx, "Already up to date.");
        }
        return Ok(0);
    }
    let st = repo.locked_status(line)?.0;
    if !st.staged.is_empty() {
        return Err(exit(1, "error: you have staged changes; save or park them before merging."));
    }
    refuse_open_conflicts(&st)?;
    let mut local = local_paths(repo, line, &st)?;
    let quiet = a.has("-q");
    // The follow overlays merge with the line: all of them or none.
    let ovl = plan_overlays(ctx, repo, line, theirs_line.as_ref(), &label, &message)?;
    if queue.is_some() && ovl.conflicts > 0 {
        let paths: Vec<_> = ovl
            .moves
            .iter()
            .filter_map(|(_, _, _, s)| s.as_ref())
            .flat_map(|s| s.conflicts.iter().map(|c| c.path.as_str()))
            .collect();
        ovl.discard(repo);
        return Err(exit(1, format!("queued merge has overlay conflicts: {}", paths.join(", "))));
    }
    if !checks.is_empty() && ovl.conflicts > 0 {
        ovl.discard(repo);
        return Err(exit(
            1,
            format!(
                "error: the merge of the overlays into the protected line '{}' has conflicts; resolve them in a line first",
                line.name
            ),
        ));
    }
    let report = |ctx: &Ctx| {
        for m in &ovl.messages {
            outln!(ctx, "{m}");
        }
        if ovl.changed() && !quiet {
            outln!(ctx, "Merged overlays: {}", ovl.names().join(" "));
        }
    };
    if up_to_date {
        if !ovl.changed() {
            outln!(ctx, "Already up to date.");
            return Ok(0);
        }
        // Only the overlays changed: the shared set stays.
        finish(
            ctx,
            repo,
            line,
            "merge",
            &head,
            vec![],
            w,
            &local,
            serde_json::json!({"theirs": theirs, "overlays_only": true}),
            Some(&ovl),
            queue,
            Some(&settled),
        )?;
        report(ctx);
        return Ok(if ovl.conflicts > 0 { 1 } else { 0 });
    }
    let ff = repo.is_ancestor(&head, &theirs)?;
    let run_probes = queue.is_some_and(|q| q.run_probes);
    if ff && !a.has("--no-ff") && !a.has("--squash") {
        check_paths(ctx, repo, line, rule.as_ref(), &head, &theirs)?;
        check_approvals(ctx, repo, line, rule.as_ref(), &head, &theirs)?;
        if !checks.is_empty() || run_probes {
            local = checks_unlocked(ctx, repo, line, &head, &theirs, &settled, quiet, queue, &mut held)?;
        }
        finish(
            ctx,
            repo,
            line,
            "merge",
            &theirs,
            vec![],
            w,
            &local,
            serde_json::json!({"theirs": theirs, "fast_forward": true}),
            Some(&ovl),
            queue,
            Some(&settled),
        )?;
        if !quiet {
            outln!(ctx, "Updating {}..{}\nFast-forward", ids::short(&head), ids::short(&theirs));
            print_stat(ctx, repo, &head, &theirs)?;
        }
        report(ctx);
        return Ok(if ovl.conflicts > 0 { 1 } else { 0 });
    }
    if a.has("--ff-only") {
        ovl.discard(repo);
        return Err(exit(128, "fatal: Not possible to fast-forward, aborting."));
    }
    let base = match repo.merge_base(&head, &theirs)? {
        Some(b) => b,
        None => {
            ovl.discard(repo);
            return Err(exit(128, format!("fatal: refusing to merge unrelated histories ({rev})")));
        }
    };
    let parents = if a.has("--squash") { vec![head.clone()] } else { vec![head.clone(), theirs.clone()] };
    let (state, out) = merge_states(ctx, repo, line, &base, &head, &theirs, &label, parents, &message)?;
    let sid = state.id.clone();
    let nconf = out.conflicts.len();
    // A queued merge and a follow pass (`REFUSE_CONFLICTS`) never leave a merge with conflicts.
    if (queue.is_some() || a.has(REFUSE_CONFLICTS)) && nconf > 0 {
        let paths = out.conflicts.iter().map(|c| c.path.as_str()).collect::<Vec<_>>().join(", ");
        repo.discard_states(std::slice::from_ref(&state));
        ovl.discard(repo);
        let what = if queue.is_some() { "queued merge" } else { "merge" };
        return Err(exit(1, format!("{what} has conflicts: {paths}")));
    }
    let mut states = vec![state];
    if let Err(e) = check_paths(ctx, repo, line, rule.as_ref(), &head, &sid)
        .and_then(|_| check_approvals(ctx, repo, line, rule.as_ref(), &head, &theirs))
    {
        repo.discard_states(&states);
        ovl.discard(repo);
        return Err(e);
    }
    if !checks.is_empty() || run_probes {
        if nconf > 0 {
            repo.discard_states(&states);
            ovl.discard(repo);
            return Err(exit(
                1,
                format!("error: the merge into the protected line '{}' has {nconf} conflict(s); resolve them in a line and merge that line", line.name),
            ));
        }
        // The result becomes a state of its own record, so the check runs on exactly the content
        // that the line will take, and a failed result stays for inspection.
        let mut rec = Record::new("merge.candidate");
        rec.states = states.clone();
        rec.data = serde_json::json!({"line": line.id, "theirs": theirs});
        if let Err(e) = repo.commit_line(line, &head, |_| repo.commit_record(ctx, rec).map(|_| ())) {
            crate::repo::discard_unrecorded(repo, &states);
            ovl.discard(repo);
            return Err(e);
        }
        // Recorded: the states of the finish below come from the candidate record.
        states.clear();
        local = checks_unlocked(ctx, repo, line, &head, &sid, &settled, quiet, queue, &mut held)?;
    }
    finish(
        ctx,
        repo,
        line,
        "merge",
        &sid,
        states,
        w,
        &local,
        serde_json::json!({"theirs": theirs, "base": base, "squash": a.has("--squash"), "conflicts": nconf}),
        Some(&ovl),
        queue,
        Some(&settled),
    )?;
    for m in &out.messages {
        outln!(ctx, "{m}");
    }
    report(ctx);
    if nconf > 0 {
        outln!(
            ctx,
            "Automatic merge left {nconf} conflict(s) in state {}: fix them, then 'layr add' and 'layr save -m <message>'.",
            ids::short(&sid)
        );
        return Ok(1);
    }
    if !quiet {
        outln!(ctx, "Merge made by the 'layr' strategy: {}", ids::short(&sid));
        print_stat(ctx, repo, &head, &sid)?;
    }
    Ok(if ovl.conflicts > 0 { 1 } else { 0 })
}

fn pick(ctx: &Ctx, args: &[String], revert: bool) -> Result<i32> {
    let spec = Spec::new()
        .flag("-n|--no-save")
        .removed("--no-commit", "--no-save")
        .flag("-x")
        .flag("--no-edit")
        .flag("--abort")
        .flag("--continue")
        .value("-m|--mainline")
        .flag("-e|--edit")
        .flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let repo = &h.repo;
    if a.has("--abort") {
        return super::admin::undo_last_of(ctx, repo, &line, &[if revert { "revert" } else { "cherry-pick" }]);
    }
    if a.pos.is_empty() {
        return Err(exit(128, if revert { "usage: layr invert <rev>..." } else { "usage: layr pick <rev>..." }));
    }
    let mut picks = Vec::new();
    for r in &a.pos {
        if let Some((x, y)) = r.split_once("..") {
            let xi = revs::resolve(repo, Some(&line), x)?;
            let yi = revs::resolve(repo, Some(&line), y)?;
            let mut chain = repo.first_parent_chain(&yi, Some(&xi))?;
            chain.reverse();
            picks.extend(chain);
        } else {
            picks.push(revs::resolve(repo, Some(&line), r)?);
        }
    }
    let picked: Vec<&str> = picks.iter().map(String::as_str).collect();
    repo.hold(repo.pin(&picked)?);
    // The merge of each picked state onto the head (base and theirs swapped for an invert).
    let pair = |p: &str| -> Result<(String, String)> {
        let parent = repo
            .state(p)?
            .parents
            .first()
            .cloned()
            .ok_or_else(|| exit(128, format!("fatal: {} has no parent", ids::short(p))))?;
        Ok(if revert { (p.to_string(), parent) } else { (parent, p.to_string()) })
    };
    // APFS: a pick that needs no merge is a thin layer and a write of its paths (`thin_pick`).
    // The picks it does not take go on to the merge below.
    #[cfg(target_os = "macos")]
    if !revert {
        let mut done = 0;
        for p in &picks {
            let cur = repo.line(&line.id)?;
            let Some(sid) = thin_pick(ctx, repo, &cur, p, a.has("-x"))? else { break };
            done += 1;
            if !a.has("-q") {
                let subject = repo.state(&sid)?.message.lines().next().unwrap_or("").to_string();
                outln!(ctx, "[{} {}] {}", cur.name, ids::short(&sid), subject);
            }
        }
        picks.drain(..done);
        if picks.is_empty() {
            return Ok(0);
        }
    }
    #[cfg(target_os = "macos")]
    let prepared = (|| -> Result<Vec<PreparedStep>> {
        let _t = crate::trace::span("prepare pick");
        repo.prepare_status(&line)?;
        let chain = picks.iter().map(|p| pair(p).map(|(b, t)| (b, t, ids::short(p)))).collect::<Result<Vec<_>>>()?;
        prepare_steps(ctx, repo, &line, &line.head, chain)
    })()
    .unwrap_or_default();
    #[cfg(not(target_os = "macos"))]
    let prepared: Vec<PreparedStep> = Vec::new();
    let head_before = line.head.clone();
    let r = (|| -> Result<i32> {
        let (_line_lock, w) = work::begin_line(ctx, repo, &line, crate::repo::LineOp::Direct)?;
        let mut line = repo.line(&line.id)?;
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut, unused_variables))]
        let mut steps: Steps = if line.head == head_before { prepared } else { Vec::new() }.into_iter().peekable();
        let st = repo.locked_status(&line)?.0;
        if !st.staged.is_empty() {
            return Err(exit(1, "error: you have staged changes; save or park them first."));
        }
        refuse_open_conflicts(&st)?;
        let local = st.local_paths();
        let mut code = 0;
        let mut first_w = w;
        for p in picks {
            let c = repo.state(&p)?;
            let (base, theirs) = pair(&p)?;
            let subject = c.message.lines().next().unwrap_or("").to_string();
            let message = if revert {
                format!("Invert \"{subject}\"\n\nThis inverts state {}.", ids::display(&p))
            } else if a.has("-x") {
                format!("{}\n\n(picked from state {})", c.message, ids::display(&p))
            } else {
                c.message.clone()
            };
            let label = ids::short(&p);
            #[cfg(target_os = "macos")]
            hand_over(&mut steps, &line, &base, &line.head, &theirs, &label);
            let (mut state, out) =
                merge_states(ctx, repo, &line, &base, &line.head, &theirs, &label, vec![line.head.clone()], &message)?;
            if !revert {
                state.author = c.author.clone();
                state.change_id = c.change_id.clone();
            }
            let sid = state.id.clone();
            let n = out.conflicts.len();
            finish(
                ctx,
                repo,
                &line,
                if revert { "revert" } else { "cherry-pick" },
                &sid,
                vec![state],
                first_w.take(),
                &local,
                if revert {
                    serde_json::json!({"source": p})
                } else {
                    serde_json::json!({"source":p,"version":crate::model::VersionOperation {
                        state: sid.clone(), kind: "pick".into(), source: p.clone(),
                        target_line: Some(line.id.clone()), target_head: line.head.clone(),
                    }})
                },
                None,
                None,
                None,
            )?;
            line = repo.line(&line.id)?;
            for m in &out.messages {
                outln!(ctx, "{m}");
            }
            if n > 0 {
                outln!(
                    ctx,
                    "{} {label} left {n} conflict(s) in state {}: fix them and save.",
                    if revert { "Invert of" } else { "Pick of" },
                    ids::short(&sid)
                );
                code = 1;
            } else if !a.has("-q") {
                outln!(ctx, "[{} {}] {}", line.name, ids::short(&sid), message.lines().next().unwrap_or(""));
            }
        }
        // The uncommitted work that the pick left in the folder: an agent checks this line, not `status`.
        if !local.is_empty() {
            outln!(ctx, "{}", kept_message(&local));
        }
        Ok(code)
    })();
    discard_prepared(repo);
    r
}

/// Leaves a pick to the merge; the trace names the reason.
#[cfg(target_os = "macos")]
fn declined(reason: &str) -> Result<Option<String>> {
    let _t = crate::trace::span(format!("thin pick declined: {reason}"));
    Ok(None)
}

/// The APFS pick without a merge (the common case): the change of the picked state over its
/// parent is a thin layer over the head, and only the changed paths are written to the working
/// folder. No tree is copied. Returns false, with nothing changed, when the merge is needed: the
/// head changed the same paths, a folder or an attributes file changed, the working folder has
/// local changes, or the state is not a thin layer over its parent.
#[cfg(target_os = "macos")]
fn thin_pick(ctx: &Ctx, repo: &Repo, line: &Line, pick: &str, picked_from: bool) -> Result<Option<String>> {
    let _t = crate::trace::span("thin pick");
    let c = repo.state(pick)?;
    let [base] = c.parents.as_slice() else {
        return declined("parents");
    };
    let base = base.clone();
    // A thin picked state holds its change in its own layer (when that layer sits on `base`); a
    // complete one (saved with caches) is read through its change set.
    let own_layer = repo.p.delta_path(pick);
    let complete = repo.p.state_path(pick);
    let thin = !complete.is_dir()
        && own_layer.is_dir()
        && crate::storage::subvol_info(&own_layer)?.base.as_deref() == Some(base.as_str());
    if !thin && !complete.is_dir() {
        return declined("own layer");
    }
    repo.authorize_line(ctx, line, crate::repo::LineOp::Direct)?;
    let _line_lock = repo.line_lock(line)?;
    let line = repo.line(&line.id)?;
    if repo.current_stage(&line)?.is_some() {
        return declined("stage");
    }
    let w = repo.working(&line);
    let head_src = repo.head_src(&line)?;
    // Local changes go through the merge. Overlay folders are not part of the head: the walk for
    // them (`cache_paths`) runs only when the working folder has a change.
    let local = changes::file_changes(&head_src, &w)?;
    if !local.is_empty() {
        let regen = repo.cache_paths(&w)?;
        let keep = |p: &str| !skip_path(p) && !regen.iter().any(|r| p == r || p.starts_with(&format!("{r}/")));
        if local.iter().any(|x| keep(&x.path) || x.old_path.as_deref().is_some_and(keep)) {
            return declined("local change");
        }
    }
    let base_src = repo.state_src(&base)?;
    let pick_src = repo.state_src(pick)?;
    let cands = changes::candidates(&base_src, &pick_src)?;
    if cands.iter().any(|x| x.old.as_ref().is_some_and(|m| m.is_dir()) || x.new.as_ref().is_some_and(|m| m.is_dir())) {
        return declined("directory change");
    }
    let theirs: Vec<changes::Change> = changes::without_moves(changes::file_changes_of(&base_src, &pick_src, cands)?)
        .into_iter()
        .filter(|x| !skip_path(&x.path))
        .collect();
    let attributes = |p: &str| p.ends_with(".layrattributes") || p.ends_with(".gitattributes");
    // Only a derive rule changes the merge of other paths; the merge reads the root attributes.
    let mut derive = false;
    for s in [&head_src as &dyn crate::fsutil::Src, &pick_src] {
        for n in [".layrattributes", ".gitattributes"] {
            if let Ok(text) = s.at(n).and_then(|p| Ok(std::fs::read_to_string(p).unwrap_or_default())) {
                derive |= text.contains("derive");
            }
        }
    }
    if derive || theirs.iter().any(|x| attributes(&x.path) || x.old_path.as_deref().is_some_and(attributes)) {
        return declined("attributes");
    }
    let mut ours: Vec<String> = Vec::new();
    if line.head != base {
        for x in changes::without_moves(changes::file_changes(&base_src, &head_src)?) {
            ours.push(x.path);
        }
    }
    let overlaps = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"));
    for x in &theirs {
        if ours.iter().any(|o| overlaps(&x.path, o)) {
            return declined("overlap");
        }
    }
    let _t = crate::trace::span(format!("pick visits {} paths", theirs.len()));
    // The layer of the picked state already holds exactly its change over `base`: it becomes the
    // layer of the new state over the head.
    let (src, diff) = if thin {
        let mut write = std::collections::BTreeSet::new();
        crate::fsutil::walk(&own_layer, "", &mut |p, _| {
            if !p.is_empty() {
                write.insert(p.to_string());
            }
            Ok(true)
        })?;
        (own_layer.clone(), crate::thin::Diff { write, removed: crate::thin::removed_of(&own_layer)?, whole: Default::default() })
    } else {
        // The change set of the picked state over `base`, read from its complete tree.
        let mut write = std::collections::BTreeSet::new();
        let mut removed = std::collections::BTreeSet::new();
        for x in &theirs {
            if x.new.is_some() {
                write.insert(x.path.clone());
            } else {
                removed.insert(x.path.clone());
            }
            if let Some(o) = &x.old_path {
                removed.insert(o.clone());
            }
        }
        (complete.clone(), crate::thin::Diff { write, removed, whole: Default::default() })
    };
    let id = ids::new_id();
    let dst = repo.p.delta_path(&id);
    crate::thin::write_layer(&src, &dst, &diff, &line.head, false, None)?;
    let message =
        if picked_from { format!("{}\n\n(picked from state {})", c.message, ids::display(pick)) } else { c.message.clone() };
    let mut st = repo.describe_state(ctx, id.clone(), StateKind::Commit, vec![line.head.clone()], &message, Some(&line))?;
    st.author = c.author.clone();
    st.change_id = c.change_id.clone();
    let states = std::slice::from_ref(&st);
    let fail = |e: anyhow::Error| {
        repo.discard_states(states);
        e
    };
    if let Err(e) = repo.check_move(ctx, &line, &id) {
        return Err(fail(e));
    }
    // The working folder takes the paths of the change (removals first, as a folder can replace
    // a file). Each path is read from the layer that holds it in the new tree. A failure after
    // this point writes each path back from the head.
    let view = crate::thin::Layered::of_state(&repo.p, &id)?;
    let own = repo.own(&line);
    let undo_work = || -> Result<()> {
        for x in &theirs {
            for p in x.old_path.iter().chain(std::iter::once(&x.path)) {
                if crate::fsutil::lmeta(&head_src, p).is_some() {
                    crate::fsutil::copy_work_entry(&head_src, p, &w, p, own)?;
                } else {
                    crate::fsutil::remove_entry(&w, p)?;
                }
            }
        }
        Ok(())
    };
    let work = (|| -> Result<()> {
        for x in &theirs {
            for gone in x.old_path.iter().chain(x.new.is_none().then_some(&x.path)) {
                if view.find(gone)?.is_none() {
                    crate::fsutil::remove_entry(&w, gone)?;
                }
            }
        }
        for x in theirs.iter().filter(|x| x.new.is_some()) {
            crate::fsutil::copy_work_entry(&view, &x.path, &w, &x.path, own)?;
        }
        Ok(())
    })();
    if let Err(e) = work {
        let _ = undo_work();
        return Err(fail(e));
    }
    let data = serde_json::json!({"source": pick, "version": crate::model::VersionOperation {
        state: id.clone(), kind: "pick".into(), source: pick.into(),
        target_line: Some(line.id.clone()), target_head: line.head.clone(),
    }});
    // The working folder was clean, so its content before the pick is the head: undo reverts the
    // working content of the pick with it (deleted files come back).
    let head = line.head.clone();
    let rec = match repo.head_record(ctx, "cherry-pick", &line, &id, vec![st], Some(line.head.clone()), data) {
        Ok(rec) => rec,
        Err(e) => {
            let _ = undo_work();
            return Err(e);
        }
    };
    let all_states = rec.states.clone();
    if let Err(e) = repo.commit_line(&line, &head, |_| repo.commit_record(ctx, rec).map(|_| ())) {
        let _ = undo_work();
        crate::repo::discard_unrecorded(repo, &all_states);
        return Err(e);
    }
    repo.after_head_moved(&line, &id);
    Ok(Some(id))
}

/// The uncommitted paths that a pick kept: the names of the first 20, then the count of the rest.
fn kept_message(paths: &std::collections::BTreeSet<String>) -> String {
    let names: Vec<&str> = paths.iter().take(20).map(String::as_str).collect();
    let more = paths.len() - names.len();
    let mut text = format!("kept {} uncommitted file(s): {}", paths.len(), names.join(", "));
    if more > 0 {
        text.push_str(&format!(", and {more} more"));
    }
    text
}

pub fn cherry_pick(ctx: &Ctx, args: &[String]) -> Result<i32> {
    pick(ctx, args, false)
}

pub fn revert(ctx: &Ctx, args: &[String]) -> Result<i32> {
    pick(ctx, args, true)
}

pub fn rebase(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("--onto")
        .flag("--abort")
        .flag("--continue")
        .flag("-i|--interactive")
        .flag("-q|--quiet")
        .flag("--autopark")
        .removed("--autostash", "--autopark");
    let a = parse(&spec, args)?;
    if a.has("-i") {
        return Err(exit(1, "interactive replay is not supported"));
    }
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let repo = &h.repo;
    if a.has("--abort") {
        return super::admin::undo_last_of(ctx, repo, &line, &["rebase"]);
    }
    if a.has("--continue") {
        return Err(exit(1, "layr replay never stops on a conflict: fix the files and run 'layr save -m <message>'."));
    }
    let up = a.pos.first().ok_or_else(|| exit(128, "usage: layr replay [--onto <newbase>] <upstream>"))?;
    let upstream = revs::resolve(repo, Some(&line), up)?;
    let onto = match a.get("--onto") {
        Some(o) => revs::resolve(repo, Some(&line), o)?,
        None => upstream.clone(),
    };
    repo.hold(repo.pin(&[&line.head, &upstream, &onto])?);
    #[cfg(target_os = "macos")]
    let prepared = (line.head.clone(), prepare_replay(ctx, repo, &line, &upstream, &onto).unwrap_or_default());
    #[cfg(not(target_os = "macos"))]
    let prepared = (String::new(), Vec::new());
    let r = replay_locked(ctx, repo, &line, &a, up, &upstream, &onto, prepared);
    discard_prepared(repo);
    r
}

#[allow(clippy::too_many_arguments)]
fn replay_locked(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    a: &crate::args::Parsed,
    up: &str,
    upstream: &str,
    onto: &str,
    prepared: (String, Vec<PreparedStep>),
) -> Result<i32> {
    let (upstream, onto) = (upstream.to_string(), onto.to_string());
    let (_line_lock, w) = work::begin_line(ctx, repo, line, crate::repo::LineOp::Direct)?;
    let line = repo.line(&line.id)?;
    let head0 = line.head.clone();
    // The merges prepared before the lock hold when the line did not move.
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut, unused_variables))]
    let mut steps: Steps = if prepared.0 == line.head { prepared.1 } else { Vec::new() }.into_iter().peekable();
    let st = repo.locked_status(&line)?.0;
    if !st.clean() {
        return Err(exit(1, "error: cannot replay: you have unstaged or staged changes.\nPlease save or park them."));
    }
    let base = repo.merge_base(&line.head, &upstream)?.ok_or_else(|| exit(128, "fatal: no common ancestor"))?;
    if repo.is_ancestor(&upstream, &line.head)? && onto == upstream {
        outln!(ctx, "Current line {} is up to date.", line.name);
        return Ok(0);
    }
    let mut chain = repo.first_parent_chain(&line.head, Some(&base))?;
    chain.reverse();
    let target_line = repo.p.db.line_by_name(a.get("--onto").unwrap_or(up))?.map(|l| l.id).or(repo.state(&onto)?.line);
    let mut cur = onto.clone();
    let mut states = Vec::new();
    let mut conflicts = 0;
    for p in &chain {
        let c = repo.state(p)?;
        if c.parents.len() > 1 {
            continue;
        }
        let parent = c.parents[0].clone();
        #[cfg(target_os = "macos")]
        hand_over(&mut steps, &line, &parent, &cur, p, &ids::short(p));
        let (mut s, out) = merge_states(ctx, repo, &line, &parent, &cur, p, &ids::short(p), vec![cur.clone()], &c.message)?;
        s.author = c.author.clone();
        s.change_id = c.change_id.clone();
        conflicts += out.conflicts.len();
        for m in &out.messages {
            outln!(ctx, "{m}");
        }
        cur = s.id.clone();
        // Later picks read this state: write its record now, as part of the rebase.
        let mut rec = Record::new("rebase.step");
        rec.states.push(s.clone());
        rec.data = serde_json::json!({"line":line.id,"version":crate::model::VersionOperation {
            state: s.id.clone(), kind: "replay".into(), source: p.clone(),
            target_line: target_line.clone(), target_head: onto.clone(),
        }});
        let step_states = rec.states.clone();
        if let Err(e) = repo.commit_line(&line, &head0, |_| repo.commit_record(ctx, rec).map(|_| ())) {
            crate::repo::discard_unrecorded(repo, &step_states);
            return Err(e);
        }
        states.push(s);
    }
    let local = st.local_paths();
    finish(
        ctx,
        repo,
        &line,
        "rebase",
        &cur,
        vec![],
        w,
        &local,
        serde_json::json!({"upstream": upstream, "onto": onto, "count": states.len()}),
        None,
        None,
        None,
    )?;
    if conflicts > 0 {
        outln!(
            ctx,
            "Replayed {} state(s) onto {} with {conflicts} conflict(s): fix them and save.",
            states.len(),
            ids::short(&onto)
        );
        return Ok(1);
    }
    outln!(ctx, "Successfully replayed {} onto {}.", line.name, ids::short(&onto));
    Ok(0)
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use crate::{
        ctx::Caller,
        model::{Author, Record, StateKind, StateRec},
        store::{Project, Store},
    };

    #[test]
    fn merge_approval_guard_refuses_derived_source_ancestry() {
        let dir = std::env::temp_dir().join(format!("layr-merge-ancestry-{}", crate::ids::new_id()));
        let ctx = Ctx {
            store: Store::open(&dir).unwrap(),
            caller: Caller::from_uid(1001),
            cwd: dir.clone(),
            env: std::collections::HashMap::new(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let project_dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(project_dir.join("meta/db")).unwrap();
        let project_id = crate::ids::new_id();
        let p = Project {
            store: &ctx.store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: project_id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let mut creation = Record::new("project");
        creation.data = serde_json::json!({"id":project_id,"name":"p","config":{}});
        p.append(creation, &ctx.actor()).unwrap();
        let state = |kind, parents| StateRec {
            id: crate::ids::new_id(),
            kind,
            change_id: None,
            derived_from: None,
            context: Some("test-v1".into()),
            parents,
            line: None,
            subvol: String::new(),
            author: Author::default(),
            message: String::new(),
            time: crate::ids::now_ms(),
            running: false,
            conflicts: vec![],
            caches: Default::default(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
            cache_of: None,
        };
        let base = state(StateKind::Commit, vec![]);
        let derived = state(StateKind::Derived, vec![]);
        let source = state(StateKind::Commit, vec![derived.id.clone()]);
        let mut record = Record::new("state");
        // A forward parent reference can arrive before the parent's record during import.
        record.states = vec![base.clone(), source.clone()];
        p.append(record, &ctx.actor()).unwrap();
        let mut record = Record::new("state");
        record.states = vec![derived.clone()];
        p.append(record, &ctx.actor()).unwrap();
        let repo = Repo { p, ctx: None };
        let line = Line {
            id: crate::ids::new_id(),
            name: "main".into(),
            head: base.id.clone(),
            owner: 1001,
            machine: ctx.store.machine.id.clone(),
        };
        let mut rule = crate::access::RuleSpec::default().rule();
        rule.approvals = 1;
        // Exercise the merge's approval guard even if an earlier ancestry check also rejects it.
        let error = check_approvals(&ctx, &repo, &line, Some(&rule), &base.id, &source.id).unwrap_err().to_string();
        let expected = repo.source_state(&derived.id).unwrap_err().to_string();
        assert_eq!(error, expected);
        assert_eq!(repo.is_ancestor(&base.id, &source.id).unwrap_err().to_string(), expected);
        assert!(error.contains("is derived; cache and derived states are outside source history"));
        drop(repo);
        drop(ctx);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
