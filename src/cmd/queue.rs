//! A signed queue of exact states for the existing protected merge.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::{Caller, Ctx};
use crate::ids;
use crate::model::{QueueChange, QueueEntry, Record, RefChange, RefState};
use crate::repo::Repo;
use anyhow::{bail, Result};
use std::fs::File;

pub(crate) fn try_lock(repo: &Repo) -> Result<Option<File>> {
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(repo.p.meta_dir().join("queue.lock"))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn require_lock(repo: &Repo) -> Result<File> {
    try_lock(repo)?.ok_or_else(|| exit(1, "a queue runner is already active for this project"))
}

fn hold(entry: &QueueEntry) -> String {
    format!("holds/queue/{}", entry.id)
}

fn find(repo: &Repo, prefix: &str) -> Result<QueueEntry> {
    let entries: Vec<_> = repo
        .p
        .db
        .queue_entries(None)?
        .into_iter()
        .filter(|entry| entry.id == prefix || (prefix.len() >= 4 && ids::display(&entry.id).starts_with(prefix)))
        .collect();
    match entries.as_slice() {
        [entry] => Ok(entry.clone()),
        [] => bail!("unknown queue entry '{prefix}'"),
        _ => bail!("ambiguous queue entry '{prefix}'"),
    }
}

fn target(repo: &Repo, entry: &QueueEntry) -> Result<crate::model::Line> {
    let line = repo.line(&entry.target_line)?;
    if repo.p.db.line_by_name(&line.name)?.map(|l| l.id != entry.target_line).unwrap_or(true) {
        bail!("queue target line '{}' was deleted", entry.into);
    }
    Ok(line)
}

fn print_entry(ctx: &Ctx, entry: &QueueEntry) {
    outln!(
        ctx,
        "{}\t{}\t{}\t{}{}{}",
        entry.id,
        entry.status,
        entry.into,
        entry.state,
        entry.reason.as_ref().map(|r| format!("\t{r}")).unwrap_or_default(),
        entry
            .probes
            .as_ref()
            .map(|outcome| {
                let rows = outcome["probes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|probe| {
                        let errors = probe["errors"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|error| {
                                format!("; {}: {}", error["side"].as_str().unwrap_or(""), error["error"].as_str().unwrap_or(""))
                            })
                            .collect::<String>();
                        format!("{}: {}{errors}", probe["probe"].as_str().unwrap_or(""), probe["status"].as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("\tprobes: {rows}")
            })
            .unwrap_or_default()
    );
}

/// The conventional title of a queue entry (the pr-title rule): `type(scope)!: subject`.
pub(crate) fn conventional_title(title: &str) -> Result<String> {
    const TYPES: &[&str] = &["feat", "fix", "perf", "revert", "docs", "test", "refactor", "build", "ci", "chore", "style"];
    let title = title.lines().next().unwrap_or("").trim();
    let ok = title.split_once(": ").is_some_and(|(head, subject)| {
        let head = head.strip_suffix('!').unwrap_or(head);
        let (kind, scope) = match head.split_once('(') {
            Some((k, rest)) => (k, rest.strip_suffix(')')),
            None => (head, Some("")),
        };
        TYPES.contains(&kind)
            && scope.is_some_and(|s| s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            && !subject.trim().is_empty()
    });
    if !ok {
        bail!("the queue entry needs a Conventional Commit title, for example 'fix(merge): keep the base'; pass it with -m (it was: '{title}')");
    }
    Ok(title.to_string())
}

pub fn queue(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(
        &Spec::new()
            .value("--into")
            .value("--push")
            .value("-m|--message")
            .flag("--json")
            .flag("--existing")
            .flag("--no-run-queue")
            .value("--checked-entry")
            .value("--checked-head")
            .value("--checked-state")
            .value("--checked-candidate")
            .value("--candidate-machine")
            .value("--expect-head")
            .value("--expect-state"),
        args,
    )?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let command = a.pos.first().map(String::as_str).unwrap_or("");
    if a.has("--no-run-queue") && command != "add" {
        bail!("--no-run-queue is only for queue add");
    }
    let guarded = (a.get("--checked-entry"), a.get("--checked-head"), a.get("--checked-state"));
    if guarded != (None, None, None) && (command != "run" || guarded.0.is_none() || guarded.1.is_none() || guarded.2.is_none()) {
        bail!("--checked-entry, --checked-head and --checked-state must be used together with queue run");
    }
    if a.get("--checked-candidate").is_some()
        && (command != "run" || guarded.0.is_none() || guarded.1.is_none() || guarded.2.is_none())
    {
        bail!("--checked-candidate needs all checked queue arguments");
    }
    if (a.get("--expect-head").is_some() || a.get("--expect-state").is_some()) && command != "candidate" {
        bail!("--expect-head and --expect-state are only for queue candidate");
    }
    if a.has("--existing") && command != "candidate" {
        bail!("--existing is only for queue candidate");
    }
    if a.get("--candidate-machine").is_some() && (command != "candidate" || !a.has("--existing")) {
        bail!("--candidate-machine needs queue candidate --existing");
    }
    if a.get("--candidate-machine").is_some_and(|machine| !crate::model::is_uuid(machine)) {
        bail!("--candidate-machine needs a full machine UUID");
    }
    let usage = "usage: layr queue (add <rev> [-m <conventional title>] [--no-run-queue]|list|remove <entry>|candidate <entry> --expect-head <state> --expect-state <state> [--existing [--candidate-machine <uuid>]]|run [--push <remote>] [--checked-entry <id> --checked-head <state> --checked-state <state> [--checked-candidate <state>]]) [--into <line>] [--json]";
    match command {
        "candidate" if a.pos.len() == 2 => {
            repo.require(ctx, if a.has("--existing") { "try" } else { "merge" })?;
            let head = a.get("--expect-head").ok_or_else(|| anyhow::anyhow!("queue candidate needs --expect-head"))?;
            let state = a.get("--expect-state").ok_or_else(|| anyhow::anyhow!("queue candidate needs --expect-state"))?;
            let head = crate::revs::resolve(repo, h.line.as_ref(), head)?;
            let state = crate::revs::resolve(repo, h.line.as_ref(), state)?;
            let _queue_lock = require_lock(repo)?;
            let entry = find(repo, &a.pos[1])?;
            if entry.status != "pending" || entry.state != state {
                bail!("queue entry is not pending at the expected state");
            }
            let closed = match entry.change_id.as_deref() {
                Some(change) => closed_reason(&repo.read_model()?, change),
                None => None,
            };
            if let Some(reason) = closed {
                bail!("{reason}: queue run fails its entry");
            }
            let line = target(repo, &entry)?;
            if line.head != head || a.get("--into").is_some_and(|name| name != line.name) {
                bail!("queue target is not at the expected head");
            }
            if a.has("--existing") {
                let candidate = super::mergecmd::existing_candidate(ctx, repo, &line, &entry, a.get("--candidate-machine"))?;
                if a.has("--json") {
                    outln!(ctx, "{}", serde_json::json!({"entry":entry.id,"head":head,"state":state,"candidate":candidate}));
                } else if let Some(candidate) = candidate {
                    outln!(ctx, "{}", candidate);
                }
                return Ok(0);
            }
            super::change::try_assign_numbers(ctx, repo);
            let (result, _) = super::mergecmd::merge_queued(ctx, repo, &line, &entry, &head, 1, true, true, None);
            let error = match result {
                Err(error) => error,
                Ok(_) => bail!("queue candidate unexpectedly completed a merge"),
            };
            if let Some(conflict) = error.downcast_ref::<super::mergecmd::CandidateConflict>() {
                let _lock = repo.p.lock()?;
                if repo.p.db.queue_entry(&entry.id)?.as_ref() != Some(&entry) || target(repo, &entry)?.head != head {
                    bail!("queue entry or target changed while preparing the candidate");
                }
                let mut after = entry.clone();
                after.status = "failed".into();
                after.reason = Some(conflict.0.clone());
                after.attempts = 1;
                let mut rec = Record::new("queue.result");
                rec.data = serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:Some(entry.clone()),after:Some(after.clone())}});
                repo.commit_record(ctx, rec)?;
                if a.has("--json") {
                    outln!(ctx, "{}", serde_json::json!({"entry":entry.id,"status":"failed","reason":conflict.0}));
                } else {
                    print_entry(ctx, &after);
                }
                return Ok(1);
            }
            if let Some(wait) = error.downcast_ref::<super::mergecmd::ChecksWaiting>() {
                if a.has("--json") {
                    outln!(ctx, "{}", serde_json::json!({"entry":entry.id,"status":"waiting","reason":wait.0}));
                }
                eprintln!("layr queue: {}", wait.0);
                return Ok(1);
            }
            let candidate =
                error.downcast_ref::<super::mergecmd::CandidateReady>().ok_or(error.to_string()).map_err(anyhow::Error::msg)?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::json!({"entry":entry.id,"head":head,"state":state,"candidate":candidate.0}));
            } else {
                outln!(ctx, "{}", candidate.0);
            }
            Ok(0)
        }
        "add" if a.pos.len() == 2 => {
            repo.require(ctx, "merge")?;
            let state = crate::revs::resolve(repo, h.line.as_ref(), &a.pos[1])?;
            let source = repo.source_state(&state)?;
            repo.state_src(&state)?;
            if let Some(change) = &source.change_id {
                // On the numbering machine, the new entry's change gets its number now.
                super::change::try_assign_numbers(ctx, repo);
                let model = repo.read_model()?;
                if let Some(reason) = closed_reason(&model, change) {
                    bail!("{reason}; reopen it first (layr change reopen {change})");
                }
            }
            // With a title (-m), the entry lands on the target as one state with that message.
            let title = a.get("-m").map(conventional_title).transpose()?;
            let upstream = match h
                .line
                .as_ref()
                .map(|line| repo.p.config_value(&format!("line.{}.upstream", line.name)))
                .transpose()?
                .flatten()
            {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(name)) => Some(name),
                Some(_) => bail!("the line upstream must be a line name"),
            };
            let into = a.get("--into").or(upstream.as_deref()).unwrap_or("main");
            let line = repo.line(into)?;
            if !repo.working(&line).exists() {
                bail!("queue target line '{into}' has no local work folder");
            }
            let entry = QueueEntry {
                id: ids::new_id(),
                into: line.name,
                target_line: line.id,
                state: state.clone(),
                change_id: source.change_id,
                added_by: ctx.caller.uid,
                time: ids::now_ms(),
                status: "pending".into(),
                attempts: 0,
                result: None,
                reason: None,
                probes: None,
                title,
            };
            {
                let _lock = repo.p.lock()?;
                let mut rec = Record::new("queue.add");
                rec.data = serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:None,after:Some(entry.clone())},"ref_change":RefChange{name:hold(&entry),before:None,after:Some(RefState{name:hold(&entry),target:state,expires_at:None})}});
                repo.commit_record(ctx, rec)?;
            }
            if !a.has("--no-run-queue") {
                if let Err(error) = run_configured(ctx, repo, Some(&entry.target_line)) {
                    eprintln!("layr queue: {error:#}");
                }
            }
            let entry = repo.p.db.queue_entry(&entry.id)?.unwrap_or(entry);
            if a.has("--json") {
                outln!(ctx, "{}", entry_json(&mut super::LineNames::new(&repo.p.db), &entry)?);
            } else {
                print_entry(ctx, &entry);
            }
            Ok(0)
        }
        "list" if a.pos.len() == 1 => {
            let line = a.get("--into").map(|name| repo.line(name)).transpose()?;
            let entries = repo.p.db.queue_entries(line.as_ref().map(|line| line.id.as_str()))?;
            if a.has("--json") {
                outln!(ctx, "{}", entries_json(repo, &entries)?);
            } else {
                for entry in entries {
                    print_entry(ctx, &entry);
                }
            }
            Ok(0)
        }
        "remove" if a.pos.len() == 2 => {
            repo.require(ctx, "merge")?;
            let _queue_lock = require_lock(repo)?;
            let _lock = repo.p.lock()?;
            let entry = find(repo, &a.pos[1])?;
            if entry.added_by != ctx.caller.uid {
                repo.require(ctx, "line.manage")?;
            }
            let mut rec = Record::new("queue.remove");
            rec.data = serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:Some(entry.clone()),after:None}});
            release_hold(repo, &entry, &mut rec.data)?;
            repo.commit_record(ctx, rec)?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::json!({"removed":entry.id}));
            } else {
                outln!(ctx, "removed {}", entry.id);
            }
            Ok(0)
        }
        "run" if a.pos.len() == 1 => {
            let line = a.get("--into").map(|name| repo.line(name)).transpose()?;
            let checked_head = guarded.1.map(|rev| crate::revs::resolve(repo, h.line.as_ref(), rev)).transpose()?;
            let checked_state = guarded.2.map(|rev| crate::revs::resolve(repo, h.line.as_ref(), rev)).transpose()?;
            let checked_candidate =
                a.get("--checked-candidate").map(|rev| crate::revs::resolve(repo, h.line.as_ref(), rev)).transpose()?;
            let guard = match guarded {
                (Some(entry), Some(_), Some(_)) => Some(QueueGuard {
                    entry,
                    head: checked_head.as_deref().expect("checked head"),
                    state: checked_state.as_deref().expect("checked state"),
                    candidate: checked_candidate.as_deref(),
                }),
                _ => None,
            };
            // On the numbering machine (usually the Layr Cloud merge machine), each pass numbers the changes that
            // have none before it merges, in one record (docs/review.md, "How numbers work").
            super::change::try_assign_numbers(ctx, repo);
            let entries = run_entries(ctx, repo, line.as_ref().map(|line| line.id.as_str()), false, guard)?;
            let mut failed = entries.iter().any(|entry| entry.status == "failed");
            let push = a.get("--push").map(str::to_owned).or(push_remote(repo)?);
            if let Some(remote) = push {
                if let Err(e) = push_merged(ctx, repo, &entries, &remote) {
                    eprintln!("layr queue: the merged line was not pushed to {remote}: {e:#}");
                    failed = true;
                }
            }
            if a.has("--json") {
                outln!(ctx, "{}", entries_json(repo, &entries)?);
            } else {
                for entry in entries {
                    print_entry(ctx, &entry);
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
        _ => Err(exit(128, usage)),
    }
}

/// A queue entry in JSON output: its fields and `target_line_name`, the current name of its target line.
fn entry_json(names: &mut super::LineNames, entry: &QueueEntry) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(entry)?;
    names.add(&mut value, "target_line")?;
    Ok(value)
}

fn entries_json(repo: &Repo, entries: &[QueueEntry]) -> Result<serde_json::Value> {
    let mut names = super::LineNames::new(&repo.p.db);
    Ok(serde_json::Value::Array(entries.iter().map(|e| entry_json(&mut names, e)).collect::<Result<_>>()?))
}

fn release_hold(repo: &Repo, entry: &QueueEntry, data: &mut serde_json::Value) -> Result<()> {
    if let Some(before) = repo.p.db.ref_value(&hold(entry), ids::now_ms())? {
        data["ref_change"] = serde_json::to_value(RefChange { name: hold(entry), before: Some(before), after: None })?;
    }
    Ok(())
}

/// Called under the merge's project lock. Queue status and line movement share one record.
pub fn complete_data(
    repo: &Repo,
    entry: &QueueEntry,
    head: &str,
    attempts: u32,
    probes: Option<serde_json::Value>,
    mut data: serde_json::Value,
) -> Result<serde_json::Value> {
    if repo.p.db.queue_entry(&entry.id)?.as_ref() != Some(entry) {
        bail!("queue entry '{}' changed while the merge ran", entry.id);
    }
    let mut after = entry.clone();
    after.status = "done".into();
    after.result = Some(head.into());
    after.reason = None;
    after.attempts = attempts;
    after.probes = probes;
    data["queue_change"] =
        serde_json::to_value(QueueChange { id: entry.id.clone(), before: Some(entry.clone()), after: Some(after) })?;
    release_hold(repo, entry, &mut data)?;
    Ok(data)
}

struct QueueGuard<'a> {
    entry: &'a str,
    head: &'a str,
    state: &'a str,
    candidate: Option<&'a str>,
}

fn run_entries(
    ctx: &Ctx,
    repo: &Repo,
    into: Option<&str>,
    skip_busy: bool,
    guard: Option<QueueGuard<'_>>,
) -> Result<Vec<QueueEntry>> {
    repo.require(ctx, "merge")?;
    let _queue_lock = match try_lock(repo)? {
        Some(lock) => lock,
        None if skip_busy => return Ok(Vec::new()),
        None => return Err(exit(1, "a queue runner is already active for this project")),
    };
    let merge_ctx = Ctx {
        store: crate::store::Store::open(&ctx.store.root)?,
        caller: ctx.caller.clone(),
        cwd: ctx.cwd.clone(),
        env: ctx.env.clone(),
        author: Default::default(),
        out: std::cell::RefCell::new(Box::new(std::io::sink())),
    };
    let merge_repo = Repo::open(&merge_ctx, &repo.p.name)?;
    let mut results = Vec::new();
    let mut found_checked_entry = false;
    while let Some(entry) = repo
        .p
        .db
        .queue_entries(into)?
        .into_iter()
        .find(|entry| entry.status == "pending" && guard.as_ref().is_none_or(|guard| entry.id == guard.entry))
    {
        found_checked_entry = guard.is_some();
        if guard.as_ref().is_some_and(|guard| entry.state != guard.state) {
            bail!("checked queue entry state changed");
        }
        let line = target(repo, &entry)?;
        if guard.as_ref().is_some_and(|guard| line.head != guard.head) {
            bail!("checked queue target head changed");
        }
        // A caller who cannot update this line cannot fail other users' entries.
        repo.authorize_line(ctx, &line, crate::repo::LineOp::Merge { expect: true, hint: None })?;
        // A closed change does not merge: its entry fails and gives its hold back.
        let closed = match entry.change_id.as_deref() {
            Some(change) => closed_reason(&repo.read_model()?, change),
            None => None,
        };
        if let Some(reason) = closed {
            let _lock = repo.p.lock()?;
            if repo.p.db.queue_entry(&entry.id)?.as_ref() != Some(&entry) {
                bail!("queue entry '{}' changed", entry.id);
            }
            let mut after = entry.clone();
            after.status = "failed".into();
            after.reason = Some(reason);
            let mut rec = Record::new("queue.result");
            rec.data =
                serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:Some(entry.clone()),after:Some(after)}});
            release_hold(repo, &entry, &mut rec.data)?;
            repo.commit_record(ctx, rec)?;
            results.push(repo.p.db.queue_entry(&entry.id)?.ok_or_else(|| anyhow::anyhow!("queue entry disappeared"))?);
            if guard.is_some() {
                break;
            }
            continue;
        }
        let mut attempts = 0;
        let outcome = loop {
            attempts += 1;
            let line = target(&merge_repo, &entry)?;
            if guard.as_ref().is_some_and(|guard| line.head != guard.head) {
                bail!("checked queue target head changed");
            }
            let (result, probes) = super::mergecmd::merge_queued(
                &merge_ctx,
                &merge_repo,
                &line,
                &entry,
                &line.head,
                attempts,
                guard.is_some(),
                false,
                guard.as_ref().and_then(|guard| guard.candidate),
            );
            if attempts == 1
                && guard.is_none()
                && result.as_ref().err().is_some_and(|error| error.downcast_ref::<super::mergecmd::TargetMoved>().is_some())
            {
                continue;
            }
            break (result, probes);
        };
        let (outcome, probes) = outcome;
        if guard.is_some() && outcome.as_ref().err().is_some_and(|e| e.downcast_ref::<super::mergecmd::TargetMoved>().is_some()) {
            bail!("checked queue target head changed");
        }
        // A check that waits for a runner of another platform keeps the entry pending; the entries after it wait too.
        if outcome.as_ref().err().is_some_and(|e| e.downcast_ref::<super::mergecmd::ChecksWaiting>().is_some()) {
            if let Err(e) = &outcome {
                eprintln!("layr queue: entry '{}' waits: {e:#}", entry.id);
            }
            break;
        }
        match outcome {
            Ok(0) => {}
            result => {
                let reason = match result {
                    Err(error) => format!("{error:#}"),
                    Ok(code) => format!("merge failed (exit {code})"),
                };
                let _lock = repo.p.lock()?;
                if repo.p.db.queue_entry(&entry.id)?.as_ref() != Some(&entry) {
                    bail!("queue entry '{}' changed", entry.id);
                }
                let mut after = entry.clone();
                after.status = "failed".into();
                after.reason = Some(reason);
                after.attempts = attempts;
                after.probes = probes;
                let mut rec = Record::new("queue.result");
                rec.data = serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:Some(entry.clone()),after:Some(after)}});
                repo.commit_record(ctx, rec)?;
            }
        }
        results.push(repo.p.db.queue_entry(&entry.id)?.ok_or_else(|| anyhow::anyhow!("queue entry disappeared"))?);
        if guard.is_some() {
            break;
        }
    }
    if guard.is_some() && !found_checked_entry {
        bail!("checked queue entry is not pending on the target line");
    }
    Ok(results)
}

/// Why a change cannot be queued or merged: it is closed. None for an open or merged change.
fn closed_reason(model: &layr_model::Project, change: &str) -> Option<String> {
    let view = model.change_view(change)?;
    (view.status == layr_model::changes::Status::Closed).then(|| {
        let why = view.closed.and_then(|c| c.reason).map(|r| format!(" ({r})")).unwrap_or_default();
        format!("{} is closed{why}", super::change::label(model, change))
    })
}

/// The remotes that a queue pass pushes its merged lines to (`queue.push`: a comma-separated list), if any.
fn push_remote(repo: &Repo) -> Result<Option<String>> {
    match repo.p.config_value("queue.push")? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(_) => bail!("queue.push must be a comma-separated list of remotes"),
    }
}

/// Push each line that an entry of `entries` merged into, once, to `remotes` (comma-separated), from the folder of
/// that line, with their push roles (`layerpush::push_all`): the line moved on this machine, and other machines and
/// the site see it only after the push. A failure on an optional remote waits for the next push.
fn push_merged(ctx: &Ctx, repo: &Repo, entries: &[QueueEntry], remotes: &str) -> Result<()> {
    let mut names: Vec<String> = remotes.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();
    names.push("-q".to_string());
    let mut pushed = std::collections::BTreeSet::new();
    for entry in entries.iter().filter(|entry| entry.status == "done") {
        let line = target(repo, entry)?;
        if !pushed.insert(line.id.clone()) {
            continue;
        }
        let push_ctx = Ctx {
            store: crate::store::Store::open(&ctx.store.root)?,
            caller: ctx.caller.clone(),
            cwd: repo.working(&line),
            env: ctx.env.clone(),
            author: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let code = if names.len() == 2 {
            super::remote::push(&push_ctx, &names)?
        } else {
            super::layerpush::push_all(&push_ctx, &names)?
        };
        anyhow::ensure!(code == 0, "push of line {} exited {code}", line.name);
    }
    Ok(())
}

/// The daemon and queue add use the target line owner, with the normal merge policy.
pub fn run_configured(ctx: &Ctx, repo: &Repo, only: Option<&str>) -> Result<()> {
    let configured = match repo.p.config_value("queue.run")? {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(value)) => value,
        Some(_) => bail!("queue.run must be a comma-separated list of line names"),
    };
    for name in configured.split(',').map(str::trim).filter(|name| !name.is_empty()) {
        let line = repo.line(name)?;
        if only.map(|id| id != line.id).unwrap_or(false) {
            continue;
        }
        let run_ctx = Ctx {
            store: crate::store::Store::open(&ctx.store.root)?,
            caller: Caller::from_uid(line.owner),
            cwd: repo.working(&line),
            env: std::collections::HashMap::new(),
            author: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let run_repo = Repo::open(&run_ctx, &repo.p.name)?;
        let entries = run_entries(&run_ctx, &run_repo, Some(&line.id), true, None)?;
        if let Some(remote) = push_remote(&run_repo)? {
            push_merged(&run_ctx, &run_repo, &entries, &remote)?;
        }
    }
    Ok(())
}
