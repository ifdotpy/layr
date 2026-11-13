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
    merge_states_with_commands(ctx, repo, line, base, ours, theirs, theirs_label, parents, message, true)
}

#[allow(clippy::too_many_arguments)]
fn merge_states_with_commands(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    base: &str,
    ours: &str,
    theirs: &str,
    theirs_label: &str,
    parents: Vec<String>,
    message: &str,
    allow_commands: bool,
) -> Result<(StateRec, merge::Outcome)> {
    #[cfg(target_os = "macos")]
    let prepared = PREPARED_MERGE.with(|p| p.borrow_mut().take()).filter(|p| {
        allow_commands && p.line == line.id && p.base == base && p.ours == ours && p.theirs == theirs && p.label == theirs_label
    });
    #[cfg(target_os = "macos")]
    let prepared = prepared.map(|p| (p.work, p.out));
    #[cfg(not(target_os = "macos"))]
    let prepared = None;
    let (work, out) = match prepared {
        Some(p) => p,
        None => merge_trees(ctx, repo, line, base, ours, theirs, theirs_label, allow_commands)?,
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
    allow_commands: bool,
) -> Result<(crate::repo::Temp, merge::Outcome)> {
    merge_trees_at(ctx, repo, line, base, ours, &repo.state_root(ours)?, theirs, theirs_label, allow_commands)
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
    allow_commands: bool,
) -> Result<(crate::repo::Temp, merge::Outcome)> {
    let base_root = repo.state_root(base)?;
    let theirs_root = repo.state_root(theirs)?;
    let work = repo.temp_work(ours_root)?;
    let commands_blocked = std::cell::Cell::new(false);
    let cfg = |k: &str| {
        if !allow_commands
            && (k.starts_with("merge.driver.") || k.starts_with("merge.verify.") || k.starts_with("derive.command."))
        {
            commands_blocked.set(true);
            return None;
        }
        repo.p.config_value(k).ok().flatten().and_then(|v| v.as_str().map(|s| s.to_string()))
    };
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
    if commands_blocked.get() {
        return Err(anyhow::Error::from(ChecksWaiting("candidate needs a merge command in a sandbox".into())));
    }
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
    repo.prepare_status_report(line)?;
    let theirs = revs::resolve(repo, Some(line), rev)?;
    let head = line.head.as_str();
    if theirs == head || repo.is_ancestor(&theirs, head)? {
        return Ok(());
    }
    if repo.is_ancestor(head, &theirs)? {
        // A fast-forward reads the layers of the new head (`finish`): it needs no complete tree.
        return Ok(());
    }
    let Some(base) = repo.merge_base(head, &theirs)? else { return Ok(()) };
    let label = repo.p.db.line_by_name(rev)?.map(|l| l.name).unwrap_or_else(|| rev.to_string());
    let (work, out) = merge_trees(ctx, repo, line, &base, head, &theirs, &label, true)?;
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
        let (work, out) = merge_trees_at(ctx, repo, line, &base, "", &ours_root, &theirs, &label, true)?;
        crate::storage::prepare_snapshot(&work.path, &repo.p.work_dir(), true)?;
        steps.push(PreparedStep { base, theirs, label, work, out });
    }
    Ok(steps)
}

/// The merges of `replay` before the store lock (APFS), for the line at its head onto `onto`.
#[cfg(target_os = "macos")]
fn prepare_replay(ctx: &Ctx, repo: &Repo, line: &Line, upstream: &str, onto: &str) -> Result<Vec<PreparedStep>> {
    let _t = crate::trace::span("prepare replay");
    repo.prepare_status_report(line)?;
    if repo.merge_base(&line.head, upstream)?.is_none() {
        return Ok(Vec::new());
    }
    let mut chain = repo.first_parents_since(&line.head, upstream)?;
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
    /// Overlay, its line of the merged line (none for an overlay that keeps parts), the new head, the
    /// new state (none for a fast-forward).
    moves: Vec<(String, Option<Line>, String, Option<StateRec>)>,
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
    fn record(&self, ctx: &Ctx, repo: &Repo, line_id: &str) -> Result<()> {
        for (name, ol, head, st) in &self.moves {
            let o = repo.overlay_repo(name)?;
            let _lock = o.p.lock()?;
            match ol {
                Some(ol) => crate::overlay::move_overlay_line(&o, ctx, "merge", ol, head, st.iter().cloned().collect())?,
                None => anyhow::bail!("line {line_id} has no line in overlay '{name}'"),
            }
        }
        Ok(())
    }
}

/// Merge the follow overlays of `theirs` into those of `line`: a fast-forward, or a three-way merge
/// in the overlay (states written, no records yet). Without a line of `theirs`, the parts that the
/// state `theirs_state` names (docs/overlays.md, section 1a) are the other side.
#[allow(clippy::too_many_arguments)]
fn plan_overlays(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    theirs: Option<&Line>,
    theirs_state: Option<&str>,
    label: &str,
    message: &str,
) -> Result<OverlayMerge> {
    let ours = repo.layers(line)?;
    let mut m = OverlayMerge { layers: ours.clone(), ..Default::default() };
    let (their_layers, theirs_name) = match theirs {
        Some(t) => (repo.layers(t)?, t.name.clone()),
        None => {
            let parts = match theirs_state {
                Some(st) => repo.p.db.state(st)?.map(|s| s.parts).unwrap_or_default(),
                None => Default::default(),
            };
            // A state takes part only on the overlays that the line shows.
            let layers: Vec<Layer> = parts
                .into_iter()
                .filter(|(overlay, _)| ours.iter().any(|l| &l.overlay == overlay))
                .map(|(overlay, state)| Layer { overlay, state })
                .collect();
            (layers, label.to_string())
        }
    };
    if their_layers.is_empty() {
        return Ok(m);
    }
    let r = (|| -> Result<()> {
        for tl in their_layers {
            let info = match repo.overlay_info(&tl.overlay)? {
                Some(i) if i.mode == Mode::Follow => i,
                _ => continue,
            };
            if !repo.reads_overlay(&tl.overlay, ctx.caller.uid, ctx.caller.gid)? {
                return Err(exit(
                    1,
                    format!("error: merging line '{}' needs the read right of its overlay '{}'", theirs_name, tl.overlay),
                ));
            }
            // A line does not know overlays (owner decision 2026-11-11): an overlay that the incoming line shows and this
            // line does not comes into this line's view, from its empty first state.
            // An overlay that only the incoming line shows: this line's own state there (its line on the
            // overlay, else the part of its head). The overlay comes into the view only when the merge
            // brings something.
            let o = repo.overlay_repo(&tl.overlay)?;
            let i = match m.layers.iter().position(|l| l.overlay == tl.overlay) {
                Some(i) => i,
                None => {
                    let own = repo
                        .overlay_layer(Some(line), &line.head, &tl.overlay)?
                        .map(|l| l.state)
                        .unwrap_or(info.root.clone());
                    if tl.state == own || o.is_ancestor(&tl.state, &own)? {
                        continue;
                    }
                    m.layers.push(Layer { overlay: tl.overlay.clone(), state: own });
                    m.layers.len() - 1
                }
            };
            let ours_s = m.layers[i].state.clone();
            if tl.state == ours_s || o.is_ancestor(&tl.state, &ours_s)? {
                continue;
            }
            // The line of this line on the overlay, made at the state that this line shows when missing.
            let ol = {
                let _lock = o.p.lock()?;
                o.ensure_overlay_line(ctx, &info, line, &ours_s)?
            };
            let write_line = ol.clone();
            let ol = Some(ol);
            let detach = |st: StateRec| st;
            if o.is_ancestor(&ours_s, &tl.state)? {
                m.layers[i].state = tl.state.clone();
                m.moves.push((tl.overlay.clone(), ol, tl.state.clone(), None));
                continue;
            }
            if repo.overlay_props(&tl.overlay)?.merge == MergeKind::UnionTheirs {
                let st = detach(crate::overlay::union_state(&o, ctx, &write_line, &ours_s, &tl.state, message)?);
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
            let (st, out) = merge_states(
                ctx,
                &o,
                &write_line,
                &base,
                &ours_s,
                &tl.state,
                label,
                vec![ours_s.clone(), tl.state.clone()],
                message,
            )?;
            let st = detach(st);
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
    require_approvals: bool,
    queue_entry: Option<&'a crate::model::QueueEntry>,
    rule: Option<&'a crate::access::Rule>,
    /// The reviewed source state.
    source: &'a str,
    /// The check commands of the line's rule, by name.
    commands: Vec<(String, String)>,
    /// Platform requirements used when the check results are evaluated.
    platforms: serde_json::Value,
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
        let platforms = repo.p.config_value("checks.platforms")?.unwrap_or_else(|| serde_json::json!({}));
        if super::checks::platforms_of(&platforms, name) != super::checks::platforms_of(&s.platforms, name) {
            return Err(exit(1, format!("merge check '{name}' platforms changed during checks; run the merge again")));
        }
        if s.require_approvals {
            let argv = layr_model::protection::check_argv(command);
            let mut wants = super::checks::platforms_of(&platforms, name);
            if wants.is_empty() {
                wants.push(None);
            }
            for want in wants {
                if !super::checks::passing_required(repo, candidate, name, &argv, want.as_deref())? {
                    return Err(anyhow::Error::from(ChecksWaiting(format!(
                        "check '{name}' no longer has a passing executor run"
                    ))));
                }
            }
        }
    }
    if candidate != head {
        queue_approvals(ctx, repo, now, rule.as_ref(), head, s.source, s.require_approvals, s.queue_entry)?;
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
        let _t = crate::trace::span("finish: secrets check");
        crate::secrets::check_between(ctx, repo, &repo.state_src(&line.head)?, &repo.state_src(new_head)?, &line.head)?;
    }
    // A line with overlays: the full trees of the old and the new head, with the overlays in
    // place, so the update never touches an overlay file it does not change.
    let layers = match ovl {
        Some(o) if o.changed() => o.layers.clone(),
        _ => repo.layers(line)?,
    };
    // A new state names its parts on the overlays (docs/overlays.md, section 1a).
    if !layers.is_empty() {
        let mut parts = repo.parts_of(&layers)?;
        repo.carry_parts(&mut parts, &line.head, &layers)?;
        for st in pending.states.iter_mut().filter(|s| s.id == new_head) {
            st.parts = parts.clone();
        }
    }
    let trees = crate::trace::span("finish: trees");
    // Without overlays both trees are read in their layers: a complete view of a thin state
    // cost one clone of the whole tree, also for a fast-forward that changes one file.
    let view = if layers.is_empty() { None } else { Some(repo.assemble(line, new_head, &layers)?) };
    let old_root = repo.head_src_to_write(line)?;
    let new_root = match &view {
        Some(v) => crate::repo::snap(&v.path),
        None => repo.state_src_to_write(new_head)?,
    };
    drop(trees);
    let wdir = repo.working(line);
    let own = repo.own(line);
    let applied = repo.apply_delta(&old_root, &new_root, &wdir, own, local, false)?;
    let record = crate::trace::span("finish: head record");
    pending.write = Some(crate::repo::WorkingWrite::new(repo, wdir, own, old_root, new_root, view));
    let mut data = data;
    if let Some(o) = ovl.filter(|o| o.changed()) {
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
    drop(record);
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
            o.record(ctx, repo, &line.id)?;
        }
        repo.commit_record(ctx, rec)?;
        Ok(())
    });
    if let Err(e) = view {
        return Err(e);
    }
    pending.done = true;
    let _t = crate::trace::span("finish: after commit");
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
/// states the merge brings in. An approval of an earlier version of the change counts when the
/// newer version changed only paths that keep approvals (`--keep-approvals`). Each owner group
/// (`--owner`) whose paths the change touches needs an approval by one of its principals.
fn check_approvals(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    rule: Option<&crate::access::Rule>,
    head: &str,
    theirs: &str,
    enforce_for_root: bool,
) -> Result<()> {
    let rule = match rule {
        Some(r) if (r.approvals > 0 || !r.owners.is_empty()) && (enforce_for_root || !ctx.caller.is_root()) => r,
        _ => return Ok(()),
    };
    let subjects = approval_subjects(repo, rule, theirs, None)?;
    let count = counted_approvers(repo, rule, head, theirs, &subjects)?.len();
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
    if let Some((pattern, who)) = missing_owner(repo, rule, head, theirs, &subjects)? {
        let from: Vec<String> = who.iter().map(|k| crate::access::show_principal(k)).collect();
        return Err(exit(
            1,
            format!(
                "error: state {} changes '{pattern}', which in line '{}' needs an approval by {} other than its authors",
                ids::short(theirs),
                line.name,
                from.join(" or ")
            ),
        ));
    }
    Ok(())
}

fn queue_approvals(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    rule: Option<&crate::access::Rule>,
    head: &str,
    theirs: &str,
    checked: bool,
    entry: Option<&crate::model::QueueEntry>,
) -> Result<()> {
    if checked {
        let project = super::checks::creator_project(repo)?;
        let creator_policy = project.access_policy(crate::ids::now_ms(), false);
        let creator = creator_policy.rule_for(&line.name);
        if let Some(creator) = &creator {
            let local_checks = rule.map(|rule| rule.check.as_slice()).unwrap_or_default();
            let creator_platforms = project.setting("checks.platforms").cloned().unwrap_or_else(|| serde_json::json!({}));
            let local_platforms = repo.p.config_value("checks.platforms")?.unwrap_or_else(|| serde_json::json!({}));
            for name in &creator.check {
                let command = project.setting(&format!("check.{name}"));
                if !local_checks.contains(name)
                    || command.and_then(serde_json::Value::as_str).is_none_or(str::is_empty)
                    || command != repo.p.config_value(&format!("check.{name}"))?.as_ref()
                    || super::checks::platforms_of(&creator_platforms, name)
                        != super::checks::platforms_of(&local_platforms, name)
                {
                    return Err(anyhow::Error::from(ChecksWaiting(format!("creator check policy differs for '{name}'"))));
                }
            }
            if creator.approvals > 0 || !creator.owners.is_empty() {
                check_creator_approvals(repo, &project, &creator_policy, creator, head, theirs)
                    .map_err(|error| anyhow::Error::from(ChecksWaiting(format!("creator approval required: {error:#}"))))?;
            }
        }
        let patterns: Vec<_> =
            creator.iter().flat_map(|r| r.paths.keys()).chain(rule.into_iter().flat_map(|r| r.paths.keys())).collect();
        if !patterns.is_empty() {
            let base = repo.merge_base(head, theirs)?.unwrap_or_else(|| head.to_owned());
            let changed = changes::file_changes(&repo.state_src(&base)?, &repo.state_src(theirs)?)?;
            let paths: Vec<_> = changed
                .iter()
                .flat_map(|c| [Some(c.path.as_str()), c.old_path.as_deref()])
                .flatten()
                .filter(|path| patterns.iter().any(|pattern| crate::access::path_matches(pattern, path)))
                .collect();
            if !paths.is_empty() {
                let entry = entry.ok_or_else(|| anyhow::Error::from(ChecksWaiting("queue actor is unavailable".into())))?;
                check_queue_path_actor(repo, &project, &creator_policy, creator.as_ref(), rule, entry, &paths)?;
            }
        }
    }
    check_approvals(ctx, repo, line, rule, head, theirs, checked).map_err(|error| {
        if checked {
            anyhow::Error::from(ChecksWaiting(format!("approval required: {error:#}")))
        } else {
            error
        }
    })
}

fn creator_approvers(
    repo: &Repo,
    project: &layr_model::Project,
    policy: &layr_model::policy::Policy,
    rule: &crate::access::Rule,
    head: &str,
    theirs: &str,
    subjects: &[String],
) -> Result<std::collections::BTreeSet<String>> {
    let mode = layr_model::protection::ApprovalMode::Merge;
    let authors = project.incoming_person_authors(theirs, Some(head), mode);
    let mut found = std::collections::BTreeSet::new();
    for subject in subjects {
        if project.machine == repo.p.store.machine.id {
            let approvals = project.approvals(subject);
            let matches = layr_model::protection::approvals(
                theirs,
                Some(head),
                mode,
                &approvals,
                |id| {
                    let state = repo.p.db.state(id)?;
                    if state.as_ref().is_some_and(|state| state.kind.auxiliary()) {
                        repo.source_state(id)?;
                    }
                    Ok(state)
                },
                |uid| {
                    let who = crate::access::Who::of(uid, crate::ctx::Caller::from_uid(uid).gid);
                    policy.may_approve(rule, &layr_model::policy::Who { uid: who.uid, groups: who.groups })
                },
            )?;
            found.extend(matches.iter().map(|approval| format!("uid:{}", approval.approver)));
        }
        for approval in project.allowed_person_approvals(subject, Some(head), mode, policy, rule) {
            if let Some(login) = approval.person.filter(|login| !authors.contains(login)) {
                found.insert(format!("person:{login}"));
            }
        }
    }
    Ok(found)
}

fn check_creator_approvals(
    repo: &Repo,
    project: &layr_model::Project,
    policy: &layr_model::policy::Policy,
    rule: &crate::access::Rule,
    head: &str,
    theirs: &str,
) -> Result<()> {
    let subjects = approval_subjects(repo, rule, theirs, Some(project))?;
    if creator_approvers(repo, project, policy, rule, head, theirs, &subjects)?.len() < rule.approvals as usize {
        anyhow::bail!("not enough approvals of the source state");
    }
    if !rule.owners.is_empty() {
        let base = repo.merge_base(head, theirs)?.unwrap_or_else(|| head.to_owned());
        let changed = changes::file_changes(&repo.state_src(&base)?, &repo.state_src(theirs)?)?;
        let paths: Vec<_> = changed.iter().flat_map(|c| [Some(c.path.as_str()), c.old_path.as_deref()]).flatten().collect();
        for (pattern, principals) in rule.owners_for(paths.iter().copied()) {
            let mut approved = false;
            for principal in principals {
                let owner = crate::access::Rule { approvers: vec![principal], ..rule.clone() };
                if !creator_approvers(repo, project, policy, &owner, head, theirs, &subjects)?.is_empty() {
                    approved = true;
                    break;
                }
            }
            if !approved {
                anyhow::bail!("missing owner approval for {pattern}");
            }
        }
    }
    Ok(())
}

/// The queue runner may be root, so path rights come from the verified requester, never the runner.
fn check_queue_path_actor(
    repo: &Repo,
    project: &layr_model::Project,
    creator: &layr_model::policy::Policy,
    creator_rule: Option<&crate::access::Rule>,
    local_rule: Option<&crate::access::Rule>,
    entry: &crate::model::QueueEntry,
    paths: &[&str],
) -> Result<()> {
    let local = repo.policy();
    let local_keys = repo.p.config_value("person.keys")?.unwrap_or_default();
    for record in repo.p.db.records_with_op("queue.add")? {
        if record.data["queue_change"]["id"].as_str() != Some(&entry.id) {
            continue;
        }
        let original = serde_json::from_value::<crate::model::QueueEntry>(record.data["queue_change"]["after"].clone());
        if original.as_ref().ok() != Some(entry) {
            continue;
        }
        let roles = if let Some(login) = record.actor.name.strip_prefix("person:") {
            let key = record.data["person_key"].as_str();
            let creator_key = project.setting("person.keys").and_then(|keys| keys[login].as_str());
            if key.is_none()
                || key != creator_key
                || key != local_keys[login].as_str()
                || record.data.get("request").is_none()
                || layr_model::validation::validate(&record).is_err()
                || creator.check_person(login, "merge").is_err()
                || local.check_person(login, "merge").is_err()
            {
                continue;
            }
            (creator.person_role(login), local.person_role(login))
        } else {
            // Numeric identities are meaningful only on the creator machine itself.
            if record.machine != project.machine
                || record.machine != repo.p.store.machine.id
                || record.actor.uid != entry.added_by
                || entry.added_by == u32::MAX
            {
                continue;
            }
            let caller = crate::ctx::Caller::from_uid(record.actor.uid);
            let who = crate::access::Who::of(record.actor.uid, caller.gid);
            let shared = layr_model::policy::Who { uid: who.uid, groups: who.groups.clone() };
            if creator.check(&shared, "merge").is_err() || local.check(&who, "merge").is_err() {
                continue;
            }
            (creator.role(&shared), local.role(&who))
        };
        if paths.iter().all(|path| {
            creator_rule.is_none_or(|rule| roles.0 >= rule.role_for_path(path))
                && local_rule.is_none_or(|rule| roles.1 >= rule.role_for_path(path))
        }) {
            return Ok(());
        }
    }
    Err(anyhow::Error::from(ChecksWaiting("queue requester lacks verified rights for the protected path".into())))
}

/// The states whose approvals count for `theirs`: itself, and the earlier versions of its change after which it
/// changed only paths that keep approvals (`Rule::keeps_approval`).
fn approval_subjects(
    repo: &Repo,
    rule: &crate::access::Rule,
    theirs: &str,
    creator: Option<&layr_model::Project>,
) -> Result<Vec<String>> {
    let mut subjects = vec![theirs.to_string()];
    if rule.keep_approvals.is_empty() {
        return Ok(subjects);
    }
    let Some(state) = repo.p.db.state(theirs)? else { return Ok(subjects) };
    let Some(change) = &state.change_id else { return Ok(subjects) };
    let model = repo.read_model()?;
    let new = repo.state_src(theirs)?;
    for version in repo.p.db.change_versions(change)? {
        if version.id == theirs || version.time > state.time {
            continue;
        }
        if repo.p.db.approvals(&version.id)?.is_empty()
            && model.person_approvals(&version.id).is_empty()
            && creator.is_none_or(|project| project.person_approvals(&version.id).is_empty())
        {
            continue;
        }
        let changed = changes::file_changes(&repo.state_src(&version.id)?, &new)?;
        if rule.keeps_approval(changed.iter().flat_map(|c| [Some(c.path.as_str()), c.old_path.as_deref()]).flatten()) {
            subjects.push(version.id);
        }
    }
    Ok(subjects)
}

/// The approvers (`uid:<n>`, `person:<login>`) of the `subjects` that `rule` lets approve a merge of `theirs` at
/// `head`: not authors of the states that `theirs` brings in.
fn counted_approvers(
    repo: &Repo,
    rule: &crate::access::Rule,
    head: &str,
    theirs: &str,
    subjects: &[String],
) -> Result<std::collections::BTreeSet<String>> {
    let pol = repo.policy();
    let model = repo.read_model()?;
    let shared = crate::access::shared_policy(&pol);
    let mode = layr_model::protection::ApprovalMode::Merge;
    let incoming = model.incoming_person_authors(theirs, Some(head), mode);
    let mut out = std::collections::BTreeSet::new();
    for subject in subjects {
        // The approvals of `subject` against the authors of `theirs`: an approval never counts for a state that
        // its approver wrote.
        let counted = layr_model::protection::approvals(
            theirs,
            Some(head),
            mode,
            &repo.p.db.approvals(subject)?,
            |id| {
                let state = repo.p.db.state(id)?;
                if state.as_ref().is_some_and(|state| state.kind.auxiliary()) {
                    repo.source_state(id)?;
                }
                Ok(state)
            },
            |uid| pol.may_approve(rule, &crate::access::Who::of(uid, crate::ctx::Caller::from_uid(uid).gid)),
        )?;
        out.extend(counted.iter().map(|a| format!("uid:{}", a.approver)));
        for approval in model.allowed_person_approvals(subject, Some(head), mode, &shared, rule) {
            if let Some(login) = approval.person.filter(|login| !incoming.contains(login)) {
                out.insert(format!("person:{login}"));
            }
        }
    }
    Ok(out)
}

/// The first owner group (`Rule::owners`) whose paths the change of `theirs` touches (since its merge base with
/// `head`) and that has no approval by one of its principals.
fn missing_owner(
    repo: &Repo,
    rule: &crate::access::Rule,
    head: &str,
    theirs: &str,
    subjects: &[String],
) -> Result<Option<(String, Vec<String>)>> {
    if rule.owners.is_empty() {
        return Ok(None);
    }
    let base = repo.merge_base(head, theirs)?.unwrap_or_else(|| head.to_string());
    let changed = changes::file_changes(&repo.state_src(&base)?, &repo.state_src(theirs)?)?;
    let paths: Vec<&str> = changed.iter().flat_map(|c| [Some(c.path.as_str()), c.old_path.as_deref()]).flatten().collect();
    for (pattern, who) in rule.owners_for(paths.iter().copied()) {
        let mut met = false;
        for principal in &who {
            let owner = crate::access::Rule { approvers: vec![principal.clone()], ..rule.clone() };
            if !counted_approvers(repo, &owner, head, theirs, subjects)?.is_empty() {
                met = true;
                break;
            }
        }
        if !met {
            return Ok(Some((pattern, who)));
        }
    }
    Ok(None)
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
        if !super::attest::check_passed(repo, after, &cmd)? {
            return Ok(false);
        }
    }
    if rule.approvals == 0 && rule.owners.is_empty() {
        return Ok(true);
    }
    let subjects = approval_subjects(repo, rule, theirs, None)?;
    Ok(counted_approvers(repo, rule, before, theirs, &subjects)?.len() as u32 >= rule.approvals
        && missing_owner(repo, rule, before, theirs, &subjects)?.is_none())
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
        // A check with platforms (`checks.platforms`) needs a passing run on each of them: a run of this machine
        // counts for its own platform only, and another platform waits for its runner.
        let wants: Vec<String> = super::checks::platforms_of(&settled.platforms, name).into_iter().flatten().collect();
        if !wants.is_empty() {
            let here = super::checks::platform();
            for want in &wants {
                if queue.is_some_and(|q| q.checked_only) {
                    if super::checks::passing_required(repo, candidate, name, &argv, Some(want))? {
                        continue;
                    }
                    if let Some(code) = failed_on(repo, candidate, &argv, want)? {
                        return Err(failed_elsewhere(name, want, code, candidate, line, head));
                    }
                    return Err(anyhow::Error::from(ChecksWaiting(format!(
                        "check '{name}' has no passing executor run on {want}"
                    ))));
                }
                if passed_on(repo, candidate, &argv, want)? {
                    continue;
                }
                if let Some(code) = failed_on(repo, candidate, &argv, want)? {
                    return Err(failed_elsewhere(name, want, code, candidate, line, head));
                }
                if !super::checks::meets(want, &here) {
                    return Err(anyhow::Error::from(ChecksWaiting(format!(
                        "check '{name}' has no passing run on {want} for {}, the state '{}' would take; a {want} runner runs it (layr checks watch)",
                        ids::short(candidate),
                        line.name
                    ))));
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
            continue;
        }
        if queue.is_some_and(|q| q.checked_only) {
            if super::checks::passing_required(repo, candidate, name, &argv, None)? {
                continue;
            }
            return Err(anyhow::Error::from(ChecksWaiting(format!("check '{name}' has no passing executor run"))));
        }
        if super::attest::check_passed(repo, candidate, command)? {
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
    let args = &super::overlay_args("merge", args, &[("--caches-only", "--overlays-only")])?;
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
        .flag("--caches-only")
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
            let (taken, refused) = h.repo.take_follow_caches(ctx, &now, vec![theirs], Some(&before), false)?;
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
    // A plain fetch takes only the records of a cache overlay: the caches of the line come now from the store remote.
    crate::store_remote::fetch_caches_on_demand(ctx, repo, &theirs.head, Some(&theirs));
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
        let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, &info, theirs)? else {
            continue;
        };
        orepo.hold(orepo.pin(&[ol.head.as_str()])?);
        let Some(saved) = orepo.p.db.state(&ol.head)?.and_then(|s| s.cache_of) else {
            return Ok(Some(format!(
                "the cache of {} names no project state; run `layr save --overlays` in {} first",
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

/// A required check has no passing run on a platform that this machine is not: the merge waits for that runner. A
/// queue entry stays pending.
#[derive(Debug)]
pub struct ChecksWaiting(pub String);

#[derive(Debug)]
pub struct CandidateReady(pub String);
impl std::fmt::Display for CandidateReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "merge candidate {} is ready for checks", self.0)
    }
}
impl std::error::Error for CandidateReady {}

#[derive(Debug)]
pub struct CandidateConflict(pub String);
impl std::fmt::Display for CandidateConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CandidateConflict {}

/// The candidate that an earlier queue pass recorded (`merge.candidate`) for the same merge: `theirs` into `line` at
/// `head`, the same squash choice and message, with its tree on this machine. None: the pass makes a new one.
fn recorded_candidate(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    head: &str,
    theirs: &str,
    squash: bool,
    message: &str,
    safe_only: bool,
) -> Result<Option<String>> {
    let machine = safe_only.then_some(repo.p.store.machine.id.as_str());
    if let Some(id) = recorded_candidate_id(repo, line, head, theirs, squash, message, safe_only, machine)? {
        repo.check_view_access(ctx, &crate::overlay::View::new(id.clone(), vec![]))?;
        if !repo.tree_available(&id) {
            repo.state_root(&id)
                .map_err(|error| anyhow::Error::from(ChecksWaiting(format!("cannot restore merge candidate {id}: {error:#}"))))?;
        }
        return Ok(Some(id));
    }
    Ok(None)
}

fn recorded_candidate_id(
    repo: &Repo,
    line: &Line,
    head: &str,
    theirs: &str,
    squash: bool,
    message: &str,
    safe_only: bool,
    machine: Option<&str>,
) -> Result<Option<String>> {
    for rec in repo.p.db.records_with_op("merge.candidate")?.into_iter().rev() {
        let d = &rec.data;
        if d["line"].as_str() == Some(&line.id)
            && d["theirs"].as_str() == Some(theirs)
            && d["head"].as_str() == Some(head)
            && d["squash"].as_bool() == Some(squash)
            && d["message"].as_str() == Some(message)
            && (!safe_only || d["commands_disabled"].as_bool() == Some(true))
            && machine.is_none_or(|id| rec.machine == id)
        {
            if let Some(st) = rec.states.first() {
                if st.parents.first().map(String::as_str) == Some(head) {
                    return Ok(Some(st.id.clone()));
                }
            }
        }
    }
    Ok(None)
}

/// Find a prepared candidate from signed metadata without making or reading its tree.
pub(crate) fn existing_candidate(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    entry: &crate::model::QueueEntry,
    machine: Option<&str>,
) -> Result<Option<String>> {
    let candidate = if repo.is_ancestor(&entry.state, &line.head)? {
        Some(line.head.clone())
    } else if entry.title.is_none() && repo.is_ancestor(&line.head, &entry.state)? {
        Some(entry.state.clone())
    } else {
        let message = match &entry.title {
            Some(title) => match &entry.change_id {
                Some(change) => format!("{title}\n\nLayr-Change: {change}"),
                None => title.clone(),
            },
            None => format!("Merge {}", entry.state),
        };
        recorded_candidate_id(repo, line, &line.head, &entry.state, entry.title.is_some(), &message, true, machine)?
    };
    if let Some(id) = &candidate {
        repo.check_view_access(ctx, &crate::overlay::View::new(id.clone(), vec![]))?;
    }
    Ok(candidate)
}
impl std::fmt::Display for ChecksWaiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ChecksWaiting {}

/// Whether an active check attestation of `state` with `argv` passed on a platform that meets `want`.
/// The exit code of an active, failed run of `argv` on `state` on a platform that meets `want`, when no run there
/// passed (callers ask `passed_on` first). A queue entry then fails with the reason instead of waiting: no runner
/// runs that exact check again on its own, so the entry would stay pending for good.
fn failed_on(repo: &Repo, state: &str, argv: &[String], want: &str) -> Result<Option<i64>> {
    let subject = crate::model::Subject { kind: "state".into(), id: state.to_owned() };
    Ok(crate::probes::active_checks(repo, &subject)?.into_values().find_map(|(_, r)| {
        let check = &r.data["check"];
        (check["argv"] == serde_json::json!(argv)
            && check["platform"].as_str().is_some_and(|have| super::checks::meets(want, have)))
        .then(|| check["exit"].as_i64())
        .flatten()
        .filter(|code| *code != 0)
    }))
}

fn failed_elsewhere(name: &str, want: &str, code: i64, candidate: &str, line: &Line, head: &str) -> anyhow::Error {
    exit(
        1,
        format!(
            "error: check '{name}' failed (exit {code}) on {want} for {}, the state '{}' would take; the line stays at {}; queue it again after a fix, or after its runner revokes the result",
            ids::short(candidate),
            line.name,
            ids::short(head)
        ),
    )
}

fn passed_on(repo: &Repo, state: &str, argv: &[String], want: &str) -> Result<bool> {
    let subject = crate::model::Subject { kind: "state".into(), id: state.to_owned() };
    Ok(crate::probes::active_checks(repo, &subject)?.into_values().any(|(_, r)| {
        let check = &r.data["check"];
        check["argv"] == serde_json::json!(argv)
            && check["exit"].as_i64() == Some(0)
            && check["platform"].as_str().is_some_and(|have| super::checks::meets(want, have))
    }))
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
    checked_only: bool,
    candidate_only: bool,
    expected_candidate: Option<&'a str>,
    probes: std::cell::RefCell<Option<serde_json::Value>>,
}

fn verify_candidate(queue: Option<&QueuedMerge<'_>>, candidate: &str) -> Result<()> {
    if queue.and_then(|q| q.expected_candidate).is_some_and(|expected| expected != candidate) {
        return Err(anyhow::Error::from(ChecksWaiting("the merge candidate changed after its checks".into())));
    }
    Ok(())
}

/// A queue entry pins its source and compares the target under the merge's project lock.
pub fn merge_queued(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    entry: &crate::model::QueueEntry,
    expected_head: &str,
    attempts: u32,
    checked_only: bool,
    candidate_only: bool,
    expected_candidate: Option<&str>,
) -> (Result<i32>, Option<serde_json::Value>) {
    if checked_only && !candidate_only && expected_candidate.is_none() {
        match repo.is_ancestor(expected_head, &entry.state) {
            Ok(true) if entry.title.is_none() => {}
            Ok(_) => {
                return (
                    Err(anyhow::Error::from(ChecksWaiting("checked queue run needs an untitled fast-forward entry".into()))),
                    None,
                )
            }
            Err(error) => return (Err(error), None),
        }
    }
    let run_probes = match crate::probes::queue_enabled(repo) {
        Ok(enabled) => enabled,
        Err(error) => return (Err(error), None),
    };
    if checked_only && run_probes && !candidate_only {
        return (Err(anyhow::Error::from(ChecksWaiting("checked queue run cannot execute probes".into()))), None);
    }
    let queue = QueuedMerge {
        entry,
        expected_head,
        attempts,
        run_probes,
        checked_only,
        candidate_only,
        expected_candidate,
        probes: Default::default(),
    };
    let mut pinned = vec![entry.state.as_str(), expected_head];
    if let Some(candidate) = expected_candidate {
        pinned.push(candidate);
    }
    match repo.pin(&pinned) {
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
        // A titled entry lands as one state with its title; the change it came from is a trailer.
        if let Some(title) = &entry.title {
            let mut message = title.clone();
            if let Some(change) = &entry.change_id {
                message.push_str(&format!("\n\nLayr-Change: {change}"));
            }
            args.flags.insert("--squash".into(), 1);
            args.values.insert("-m".into(), vec![message]);
        }
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
        require_approvals: queue.is_some_and(|q| q.checked_only),
        queue_entry: queue.map(|q| q.entry),
        rule: rule.as_ref(),
        source: &theirs,
        commands: resolve_commands(repo, line, &checks)?,
        platforms: repo.p.config_value("checks.platforms")?.unwrap_or_else(|| serde_json::json!({})),
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
        if queue.is_some_and(|q| q.candidate_only) {
            return Err(anyhow::Error::from(CandidateReady(head.clone())));
        }
        verify_candidate(queue, &head)?;
        if queue.is_some() {
            if queue.is_some_and(|q| q.run_probes || q.checked_only) {
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
    let st = {
        let _t = crate::trace::span("merge: locked status");
        repo.locked_status_report(line)?
    };
    if !st.staged.is_empty() {
        return Err(exit(1, "error: you have staged changes; save or park them before merging."));
    }
    refuse_open_conflicts(&st)?;
    let mut local = local_paths(repo, line, &st)?;
    let quiet = a.has("-q");
    // The follow overlays merge with the line: all of them or none.
    // A queued merge takes no overlays: its state comes without a line.
    let theirs_parts = if queue.is_none() { Some(theirs.as_str()) } else { None };
    let ovl = plan_overlays(ctx, repo, line, theirs_line.as_ref(), theirs_parts, &label, &message)?;
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
    if queue.is_some_and(|q| q.candidate_only) && ovl.changed() {
        ovl.discard(repo);
        return Err(anyhow::Error::from(ChecksWaiting("candidate preparation cannot merge overlays".into())));
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
        queue_approvals(
            ctx,
            repo,
            line,
            rule.as_ref(),
            &head,
            &theirs,
            queue.is_some_and(|q| q.checked_only),
            queue.map(|q| q.entry),
        )?;
        if queue.is_some_and(|q| q.candidate_only) {
            ovl.discard(repo);
            return Err(anyhow::Error::from(CandidateReady(theirs.clone())));
        }
        verify_candidate(queue, &theirs)?;
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
    // A queued merge that waits for its checks keeps its candidate: the next pass (also a fresh clone that fetched
    // the record) takes the same state while the target head is the same, so the runners check one state.
    if queue.is_some()
        && (!checks.is_empty() || run_probes || queue.is_some_and(|q| q.candidate_only || q.expected_candidate.is_some()))
        && !ovl.changed()
    {
        let safe_only = queue.is_some_and(|q| q.candidate_only || q.expected_candidate.is_some());
        if let Some(sid) = recorded_candidate(ctx, repo, line, &head, &theirs, a.has("--squash"), &message, safe_only)? {
            check_paths(ctx, repo, line, rule.as_ref(), &head, &sid)?;
            queue_approvals(
                ctx,
                repo,
                line,
                rule.as_ref(),
                &head,
                &theirs,
                queue.is_some_and(|q| q.checked_only),
                queue.map(|q| q.entry),
            )?;
            if queue.is_some_and(|q| q.candidate_only) {
                ovl.discard(repo);
                return Err(anyhow::Error::from(CandidateReady(sid)));
            }
            verify_candidate(queue, &sid)?;
            local = checks_unlocked(ctx, repo, line, &head, &sid, &settled, quiet, queue, &mut held)?;
            finish(
                ctx,
                repo,
                line,
                "merge",
                &sid,
                Vec::new(),
                w,
                &local,
                serde_json::json!({"theirs": theirs, "base": base, "squash": a.has("--squash"), "conflicts": 0, "candidate": sid}),
                Some(&ovl),
                queue,
                Some(&settled),
            )?;
            report(ctx);
            return Ok(0);
        }
    }
    if queue.is_some_and(|q| q.checked_only && q.expected_candidate.is_some()) {
        ovl.discard(repo);
        return Err(anyhow::Error::from(ChecksWaiting("the checked merge candidate is unavailable".into())));
    }
    let parents = if a.has("--squash") { vec![head.clone()] } else { vec![head.clone(), theirs.clone()] };
    let (state, out) = merge_states_with_commands(
        ctx,
        repo,
        line,
        &base,
        &head,
        &theirs,
        &label,
        parents,
        &message,
        !queue.is_some_and(|q| q.candidate_only),
    )?;
    let sid = state.id.clone();
    let nconf = out.conflicts.len();
    // A queued merge and a follow pass (`REFUSE_CONFLICTS`) never leave a merge with conflicts.
    if (queue.is_some() || a.has(REFUSE_CONFLICTS)) && nconf > 0 {
        let paths = out.conflicts.iter().map(|c| c.path.as_str()).collect::<Vec<_>>().join(", ");
        repo.discard_states(std::slice::from_ref(&state));
        ovl.discard(repo);
        let what = if queue.is_some() { "queued merge" } else { "merge" };
        if queue.is_some_and(|q| q.candidate_only) {
            return Err(anyhow::Error::from(CandidateConflict(format!("{what} has conflicts: {paths}"))));
        }
        return Err(exit(1, format!("{what} has conflicts: {paths}")));
    }
    let mut states = vec![state];
    if let Err(e) = check_paths(ctx, repo, line, rule.as_ref(), &head, &sid).and_then(|_| {
        queue_approvals(
            ctx,
            repo,
            line,
            rule.as_ref(),
            &head,
            &theirs,
            queue.is_some_and(|q| q.checked_only),
            queue.map(|q| q.entry),
        )
    }) {
        repo.discard_states(&states);
        ovl.discard(repo);
        return Err(e);
    }
    if !checks.is_empty() || run_probes || queue.is_some_and(|q| q.candidate_only) {
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
        rec.data =
            serde_json::json!({"line": line.id, "theirs": theirs, "head": head, "squash": a.has("--squash"), "message": message});
        if queue.is_some_and(|q| q.candidate_only) {
            rec.data["commands_disabled"] = serde_json::json!(true);
        }
        if let Err(e) = repo.commit_line(line, &head, |_| repo.commit_record(ctx, rec).map(|_| ())) {
            crate::repo::discard_unrecorded(repo, &states);
            ovl.discard(repo);
            return Err(e);
        }
        // Recorded: the states of the finish below come from the candidate record.
        states.clear();
        if queue.is_some_and(|q| q.candidate_only) {
            ovl.discard(repo);
            return Err(anyhow::Error::from(CandidateReady(sid)));
        }
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
    if a.has("--continue") {
        let name = if revert { "inverts" } else { "picks" };
        return Err(exit(1, format!("layr {name} never stop on a conflict: fix the files and run 'layr save -m <message>'.")));
    }
    if a.pos.is_empty() {
        return Err(exit(128, if revert { "usage: layr invert <rev>..." } else { "usage: layr pick <rev>..." }));
    }
    let mut picks = Vec::new();
    for r in &a.pos {
        if let Some((x, y)) = r.split_once("..") {
            let xi = revs::resolve(repo, Some(&line), x)?;
            let yi = revs::resolve(repo, Some(&line), y)?;
            let mut chain = repo.first_parents_since(&yi, &xi)?;
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
        repo.prepare_status_report(&line)?;
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
        let st = repo.locked_status_report(&line)?;
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
    let spec = Spec::new().value("--onto").flag("--abort").flag("--continue").flag("-i|--interactive").flag("-q|--quiet");
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
    let st = repo.locked_status_report(&line)?;
    if !st.clean() {
        return Err(exit(1, "error: cannot replay: you have unstaged or staged changes.\nPlease save or park them."));
    }
    repo.merge_base(&line.head, &upstream)?.ok_or_else(|| exit(128, "fatal: no common ancestor"))?;
    if repo.is_ancestor(&upstream, &line.head)? && onto == upstream {
        outln!(ctx, "Current line {} is up to date.", line.name);
        return Ok(0);
    }
    let mut chain = repo.first_parents_since(&line.head, &upstream)?;
    chain.reverse();
    let target_line = repo.p.db.line_by_name(a.get("--onto").unwrap_or(up))?.map(|l| l.id).or(repo.state(&onto)?.line);
    let mut cur = onto.clone();
    let mut states = Vec::new();
    let mut conflicts = 0;
    for p in &chain {
        let c = repo.state(p)?;
        let parent = match c.parents.as_slice() {
            [parent] => parent.clone(),
            [] => return Err(exit(128, format!("fatal: cannot replay {}: it has no parent", ids::short(p)))),
            _ => continue,
        };
        #[cfg(target_os = "macos")]
        hand_over(&mut steps, &line, &parent, &cur, p, &ids::short(p));
        let (mut s, out) = merge_states(ctx, repo, &line, &parent, &cur, p, &ids::short(p), vec![cur.clone()], &c.message)?;
        s.author = c.author.clone();
        s.change_id = c.change_id.clone();
        // A replayed state moves the base change onto the new upstream; its parts on the overlays stay those of the
        // state it replays (docs/overlays.md section 1a).
        s.parts = c.parts.clone();
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
        repo.copy_hidden_parts(ctx, p, &s.id)?;
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
            author: Default::default(),
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
            tree: None,
            times: None,
            cache_key_parts: Default::default(),
            parts: Default::default(),
        };
        let base = state(StateKind::Commit, vec![]);
        let derived = state(StateKind::Derived, vec![]);
        let source = state(StateKind::Commit, vec![derived.id.clone()]);
        let ordinary = state(StateKind::Commit, vec![base.id.clone()]);
        let mut record = Record::new("state");
        // A forward parent reference can arrive before the parent's record during import.
        record.states = vec![base.clone(), source.clone(), ordinary.clone()];
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
        let error = check_approvals(&ctx, &repo, &line, Some(&rule), &base.id, &source.id, false).unwrap_err().to_string();
        let expected = repo.source_state(&derived.id).unwrap_err().to_string();
        assert_eq!(error, expected);
        assert_eq!(repo.is_ancestor(&base.id, &source.id).unwrap_err().to_string(), expected);
        assert!(error.contains("is derived; cache and derived states are outside source history"));
        let root_ctx = Ctx {
            store: Store::open(&dir).unwrap(),
            caller: Caller::from_uid(0),
            cwd: dir.clone(),
            env: std::collections::HashMap::new(),
            author: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        assert!(check_approvals(&root_ctx, &repo, &line, Some(&rule), &base.id, &ordinary.id, false).is_ok());
        let error = queue_approvals(&root_ctx, &repo, &line, Some(&rule), &base.id, &ordinary.id, true, None).unwrap_err();
        assert!(error.downcast_ref::<ChecksWaiting>().is_some());
        assert!(error.to_string().contains("approval required"));
        drop(root_ctx);
        drop(repo);
        drop(ctx);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
