//! Logical changes and the immutable states of each version.

use super::{here, history};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::diff;
use crate::model::StateRec;
use crate::repo::{skip_path, Repo};
use anyhow::{bail, Result};

/// The change that `prefix` names: `#12` (a number), a bare number such as `12`, a full change id, or
/// at least four characters of its displayed id. A bare number that is also the start of a change id is
/// ambiguous.
pub fn find(repo: &Repo, prefix: &str) -> Result<String> {
    let (digits, explicit) = match prefix.strip_prefix('#') {
        Some(n) => (n, true),
        None => (prefix, false),
    };
    let number = (!digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse::<u64>().ok())
        .flatten()
        .filter(|n| *n > 0 && !digits.starts_with('0'));
    if explicit && number.is_none() {
        bail!("bad change number '{prefix}': use #1, #2, ...");
    }
    let by_number = match number {
        Some(n) => repo.read_model()?.numbering().change(n).map(String::from),
        None => None,
    };
    if explicit {
        return by_number.ok_or_else(|| anyhow::anyhow!("unknown change '{prefix}'"));
    }
    let ids: Vec<String> = repo
        .p
        .db
        .changes()?
        .into_iter()
        .filter(|id| id == prefix || (prefix.len() >= 4 && crate::ids::display(id).starts_with(prefix)))
        .collect();
    match (by_number, ids.as_slice()) {
        (Some(id), []) => Ok(id),
        (Some(id), [other]) if *other == id => Ok(id),
        (Some(_), _) => {
            bail!("'{prefix}' is change #{prefix} and also the start of a change id; use #{prefix} or more of the id")
        }
        (None, [id]) => Ok(id.clone()),
        (None, []) => bail!("unknown change '{prefix}'"),
        (None, _) => bail!("ambiguous change '{prefix}'"),
    }
}

/// On the numbering machine of the project: give a number to each change that has none, oldest first
/// version first, in one record. Other machines get the numbers when they receive that record. Takes the
/// project lock: the caller must not hold it. Returns how many numbers it gave.
pub fn assign_numbers(ctx: &Ctx, repo: &Repo) -> Result<usize> {
    let unnumbered = |model: &layr_model::Project| -> Vec<String> {
        if model.numbering().authority.as_deref() != Some(repo.p.store.machine.id.as_str()) {
            return Vec::new();
        }
        let mut first = std::collections::BTreeMap::<&str, (i64, &str)>::new();
        for s in model.states.values().map(|s| &s.state) {
            if let Some(change) = s.change_id.as_deref().filter(|c| model.numbering().number(c).is_none()) {
                let slot = first.entry(change).or_insert((s.time, &s.id));
                *slot = (*slot).min((s.time, &s.id));
            }
        }
        let mut out: Vec<_> = first.into_iter().map(|(change, at)| (at, change)).collect();
        out.sort();
        out.into_iter().map(|(_, change)| change.to_string()).collect()
    };
    if unnumbered(&repo.read_model()?).is_empty() {
        return Ok(0);
    }
    let _lock = repo.p.lock()?;
    let model = repo.read_model()?;
    let todo = unnumbered(&model);
    if todo.is_empty() {
        return Ok(0);
    }
    let next = model.numbering().next;
    let numbers: serde_json::Map<String, serde_json::Value> =
        todo.iter().zip(next..).map(|(change, n)| (change.clone(), serde_json::json!(n))).collect();
    let mut rec = crate::model::Record::new(layr_model::changes::NUMBER_OP);
    rec.data = serde_json::json!({ "numbers": numbers });
    repo.commit_record(ctx, rec)?;
    Ok(todo.len())
}

/// `assign_numbers` for a command that also works without numbers: a failure is a warning.
pub fn try_assign_numbers(ctx: &Ctx, repo: &Repo) {
    if let Err(e) = assign_numbers(ctx, repo) {
        eprintln!("layr: change numbers were not given: {e:#}");
    }
}

/// The label of a change in messages: `#12` when it has a number, else its id.
pub fn label(model: &layr_model::Project, change: &str) -> String {
    match model.numbering().number(change) {
        Some(n) => format!("#{n}"),
        None => format!("change {change}"),
    }
}

pub fn version(value: &str) -> Result<usize> {
    let n = value
        .strip_prefix('v')
        .ok_or_else(|| anyhow::anyhow!("bad change version '{value}': use v1, v2, ..."))?
        .parse::<usize>()?;
    if n == 0 {
        bail!("change versions start at v1");
    }
    Ok(n - 1)
}

pub fn change(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(
        &Spec::new()
            .flag("--json")
            .flag("--since-review")
            .flag("--all")
            .flag("--closed")
            .value("--person")
            .value("--from")
            .value("--to")
            .value("--title")
            .value("--description")
            .value("--description-file")
            .value("-m|--message")
            .value("--hand-to"),
        args,
    )?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let usage = "usage: layr change (list [--all|--closed]|show <change>|log <change>|diff <change> [--from vN|--since-review [--person <login>]] [--to vN]|edit <change> [--title <title>] [--description <text>|--description-file <file>]|close <change> [-m <reason>]|reopen <change>|numbering [--hand-to <machine>]) [--json]";
    let command = a.pos.first().map(String::as_str).unwrap_or("");
    let mut names = super::LineNames::new(&repo.p.db);
    let only = |flags: &[&str], commands: &[&str]| -> Result<()> {
        for flag in flags {
            if a.has(flag) && !commands.contains(&command) {
                bail!("{flag} is only for change {}", commands.join(" or "));
            }
        }
        Ok(())
    };
    only(&["--all", "--closed"], &["list"])?;
    only(&["--title", "--description", "--description-file"], &["edit"])?;
    only(&["-m"], &["close"])?;
    only(&["--hand-to"], &["numbering"])?;
    if a.has("--all") && a.has("--closed") {
        bail!("give --all or --closed, not both");
    }
    try_assign_numbers(ctx, repo);
    if command == "list" && a.pos.len() == 1 {
        let model = repo.read_model()?;
        let rows: Vec<_> = model
            .change_views()
            .into_iter()
            .filter(|v| {
                let closed = v.status == layr_model::changes::Status::Closed;
                a.has("--all") || closed == a.has("--closed")
            })
            .collect();
        if a.has("--json") {
            // The line of the newest version and its name, next to the model view (docs/commands.md).
            let mut out = Vec::new();
            for r in &rows {
                let mut row = serde_json::to_value(r)?;
                let line = repo.p.db.change_versions(&r.change_id)?.last().map(|s| s.line.clone());
                row["line"] = serde_json::json!(line);
                names.add(&mut row, "line")?;
                out.push(row);
            }
            outln!(ctx, "{}", serde_json::to_string(&out)?);
        } else {
            for r in rows {
                let number = r.number.map_or("-".to_string(), |n| format!("#{n}"));
                outln!(ctx, "{number}\t{}\tv{}\t{}\t{}", r.change_id, r.versions, r.status.as_str(), r.title);
            }
        }
        return Ok(0);
    }
    if command == "numbering" && a.pos.len() == 1 {
        return numbering(ctx, repo, a.get("--hand-to"), a.has("--json"));
    }
    if a.pos.len() != 2 {
        bail!("{usage}");
    }
    if matches!(command, "edit" | "close" | "reopen") {
        return lifecycle(ctx, repo, command, &a.pos[1], &a);
    }
    let id = find(repo, &a.pos[1])?;
    let versions = repo.p.db.change_versions(&id)?;
    match command {
        "show" => {
            let mut rows = Vec::new();
            let model = repo.read_model()?;
            let view = model.change_view(&id);
            for (n, s) in versions.iter().enumerate() {
                let mut row = serde_json::json!({"version":format!("v{}",n+1),"state":s,"attestations":repo.p.db.attestations(&s.id,None)?,"checks":model.checks(&s.id)});
                names.add(&mut row["state"], "line")?;
                if let Some(mut version) = model.version_info(&s.id) {
                    names.add(&mut version, "target_line")?;
                    row["operation"] = version;
                }
                rows.push(row);
            }
            let behavior = versions.last().map(|state| crate::probes::change_behavior(repo, state)).transpose();
            let behavior_json = match &behavior {
                Ok(Some(value)) => serde_json::to_value(value)?,
                Ok(None) => serde_json::json!({"probes": []}),
                Err(e) => serde_json::json!({"status":"unavailable","reason":format!("{e:#}")}),
            };
            if a.has("--json") {
                let mut out =
                    serde_json::json!({"change_id":id,"versions":rows,"behavior":behavior_json,"comments":model.comments(&id)});
                if let Some(view) = &view {
                    layr_model::changes::add_view_fields(&mut out, view);
                }
                layr_model::Project::add_review(&mut out, model.review(&id));
                outln!(ctx, "{}", out);
            } else {
                outln!(ctx, "change {id}");
                if let Some(view) = &view {
                    if let Some(n) = view.number {
                        outln!(ctx, "number: #{n}");
                    }
                    outln!(ctx, "status: {}", view.status.as_str());
                    if let Some(closed) = &view.closed {
                        let reason = closed.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
                        outln!(ctx, "closed by {} at {}{reason}", closed.actor.name, history::fmt_date(closed.time, "iso"));
                    }
                    outln!(ctx, "title: {}", view.title);
                    if let Some(d) = &view.description {
                        outln!(ctx, "description:");
                        for line in d.lines() {
                            outln!(ctx, "  {line}");
                        }
                    }
                }
                for r in rows {
                    outln!(
                        ctx,
                        "{}\t{}\t{}",
                        r["version"].as_str().unwrap_or(""),
                        r["state"]["id"].as_str().unwrap_or(""),
                        r["state"]["message"].as_str().unwrap_or("").lines().next().unwrap_or("")
                    );
                    for a in r["attestations"].as_array().into_iter().flatten() {
                        outln!(ctx, "  {}\t{}", a["kind"].as_str().unwrap_or(""), a["attestation_id"].as_str().unwrap_or(""));
                    }
                }
                super::review::print(ctx, &model.review(&id));
                outln!(ctx, "Behavior");
                match behavior {
                    Ok(Some(value)) => crate::probes::print(ctx, &value),
                    Ok(None) => outln!(ctx, "  No probes."),
                    Err(e) => outln!(ctx, "  unavailable: {e:#}"),
                }
            }
        }
        "log" => {
            let states: std::collections::BTreeSet<&str> = versions.iter().map(|s| s.id.as_str()).collect();
            let records: Vec<_> = repo
                .p
                .db
                .records(usize::MAX >> 1)?
                .into_iter()
                .filter(|r| {
                    r.states.iter().any(|s| states.contains(s.id.as_str()))
                        || r.data["attestation"]["change_id"].as_str() == Some(&id)
                })
                .collect();
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&records)?);
            } else {
                for r in records {
                    outln!(ctx, "{}\t{}\t{}", r.id, history::fmt_date(r.time, "iso"), super::admin::operation_name(&r.op));
                }
            }
        }
        "diff" => {
            if versions.len() < 2 && (a.get("--from").is_none() || a.get("--to").is_none()) {
                bail!("change {id} has only one version");
            }
            if a.has("--person") && !a.has("--since-review") {
                bail!("--person names whose review --since-review uses");
            }
            let from = if a.has("--since-review") {
                if a.has("--from") {
                    bail!("--since-review and --from name the same version; give one");
                }
                let person = a.get("--person");
                reviewed(repo, ctx, &versions, person)?.ok_or_else(|| match person {
                    Some(login) => anyhow::anyhow!("{login} approved no version of change {id}"),
                    None => anyhow::anyhow!("you approved no version of change {id}"),
                })?
            } else {
                a.get("--from").map(version).transpose()?.unwrap_or_else(|| versions.len().saturating_sub(2))
            };
            let to = a.get("--to").map(version).transpose()?.unwrap_or(versions.len() - 1);
            let old = versions.get(from).ok_or_else(|| anyhow::anyhow!("change {id} has no version v{}", from + 1))?;
            let new = versions.get(to).ok_or_else(|| anyhow::anyhow!("change {id} has no version v{}", to + 1))?;
            let (old, new) = (patch(repo, old)?, patch(repo, new)?);
            let delta = layr_model::interdiff::from_changes(&id, &format!("v{}", from + 1), &format!("v{}", to + 1), &old, &new);
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&delta)?);
            } else {
                ctx.print(&delta.diff);
            }
        }
        _ => bail!("{usage}"),
    }
    Ok(0)
}

/// `change edit`, `change close` and `change reopen`: one signed record each. The author of the change
/// (the user who made its first version, on that machine) or a maintainer may run them.
fn lifecycle(ctx: &Ctx, repo: &Repo, command: &str, which: &str, a: &crate::args::Parsed) -> Result<i32> {
    use layr_model::changes::{Status, CLOSE_OP, EDIT_OP, MAX_DESCRIPTION, MAX_TITLE, REOPEN_OP};
    let id = find(repo, which)?;
    let description = match (a.get("--description"), a.get("--description-file")) {
        (Some(_), Some(_)) => bail!("give --description or --description-file, not both"),
        (Some(text), None) => Some(text.to_string()),
        (None, Some(file)) => Some(
            String::from_utf8(super::work::read_caller_file(ctx, file, 1 << 20)?)
                .map_err(|_| anyhow::anyhow!("the description file is not UTF-8 text"))?,
        ),
        (None, None) => None,
    };
    let description = description.map(|d| d.trim_end().to_string());
    let title = a.get("--title").map(|t| t.trim().to_string());
    let reason = a.get("-m").map(|r| r.trim().to_string());
    if command == "edit" && title.is_none() && description.is_none() {
        bail!("change edit needs --title, --description or --description-file");
    }
    if let Some(t) = &title {
        if t.contains(['\n', '\r']) {
            bail!("a change title is one line");
        }
        if t.chars().count() > MAX_TITLE {
            bail!("a change title has at most {MAX_TITLE} characters");
        }
    }
    if description.as_ref().is_some_and(|d| d.len() > MAX_DESCRIPTION)
        || reason.as_ref().is_some_and(|r| r.len() > MAX_DESCRIPTION)
    {
        bail!("the text has more than {} KiB", MAX_DESCRIPTION >> 10);
    }
    let _lock = repo.p.lock()?;
    let model = repo.read_model()?;
    let view = model.change_view(&id).ok_or_else(|| anyhow::anyhow!("unknown change '{which}'"))?;
    let name = label(&model, &id);
    let first = model.change_versions(&id).into_iter().next().ok_or_else(|| anyhow::anyhow!("unknown change '{which}'"))?;
    let author =
        first.author.uid == ctx.caller.uid && model.states.get(&first.id).is_some_and(|s| s.machine == repo.p.store.machine.id);
    if !author && repo.require(ctx, "line.manage").is_err() {
        bail!("permission denied: only the author of {name} or a maintainer can {command} it");
    }
    let mut rec = crate::model::Record::new(match command {
        "edit" => EDIT_OP,
        "close" => CLOSE_OP,
        _ => REOPEN_OP,
    });
    rec.data = serde_json::json!({ "change_id": id });
    match (command, view.status) {
        ("close" | "reopen", Status::Merged) => bail!("{name} is merged; a merged change cannot be closed or reopened"),
        ("close", Status::Closed) | ("reopen", Status::Open) => {
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&view)?);
            } else {
                outln!(ctx, "{name} is {} already", view.status.as_str());
            }
            return Ok(0);
        }
        ("close", _) => {
            if let Some(r) = reason.filter(|r| !r.is_empty()) {
                rec.data["reason"] = serde_json::json!(r);
            }
        }
        ("edit", _) => {
            if let Some(t) = title {
                rec.data["title"] = serde_json::json!(t);
            }
            if let Some(d) = description {
                rec.data["description"] = serde_json::json!(d);
            }
        }
        _ => {}
    }
    repo.commit_record(ctx, rec)?;
    let view = repo.read_model()?.change_view(&id).ok_or_else(|| anyhow::anyhow!("unknown change '{which}'"))?;
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&view)?);
    } else {
        match command {
            "edit" => outln!(ctx, "edited {name}: {}", view.title),
            "close" => outln!(ctx, "closed {name}"),
            _ => outln!(ctx, "reopened {name}"),
        }
    }
    Ok(0)
}

/// `change numbering`: the numbering machine and the next number. `--hand-to <machine>` gives the role to
/// another machine (only the numbering machine itself, by an admin).
fn numbering(ctx: &Ctx, repo: &Repo, hand_to: Option<&str>, json: bool) -> Result<i32> {
    let me = repo.p.store.machine.id.clone();
    if let Some(target) = hand_to {
        repo.require(ctx, "config")?;
        let machine = repo.p.db.machine_by_name(target)?.ok_or_else(|| anyhow::anyhow!("unknown machine '{target}'"))?;
        let _lock = repo.p.lock()?;
        let model = repo.read_model()?;
        match model.numbering().authority.as_deref() {
            Some(m) if m == me => {}
            Some(m) => bail!("only the numbering machine ({m}) can hand the role to another machine"),
            None => bail!("the project has no numbering machine: it has no single creator record"),
        }
        if machine == me {
            bail!("this machine gives the numbers already");
        }
        let mut rec = crate::model::Record::new(layr_model::changes::HANDOFF_OP);
        rec.data = serde_json::json!({"machine": machine, "next": model.numbering().next});
        repo.commit_record(ctx, rec)?;
    }
    let model = repo.read_model()?;
    let n = model.numbering();
    let names: std::collections::BTreeMap<String, String> = repo.p.db.machines()?.into_iter().collect();
    let authority_name = n.authority.as_ref().and_then(|m| names.get(m)).cloned();
    if json {
        outln!(
            ctx,
            "{}",
            serde_json::json!({"authority":n.authority,"authority_name":authority_name,"this_machine":n.authority.as_deref()==Some(me.as_str()),"next":n.next,"numbered":n.numbers.len()})
        );
    } else {
        match &n.authority {
            Some(m) => outln!(
                ctx,
                "numbering machine: {m}{}{}",
                authority_name.map(|name| format!(" ({name})")).unwrap_or_default(),
                if *m == me { ", this machine" } else { "" }
            ),
            None => outln!(ctx, "numbering machine: none (the project has no single creator record)"),
        }
        outln!(ctx, "next number: #{}", n.next);
        outln!(ctx, "numbered changes: {}", n.numbers.len());
    }
    Ok(0)
}

/// The newest version of a change that the caller approved (an `approval` attestation of this machine and this
/// user on the state of that version), as an index into `versions`.
fn reviewed(repo: &Repo, ctx: &Ctx, versions: &[StateRec], person: Option<&str>) -> Result<Option<usize>> {
    // A person's approval comes through a signed request (`layr request approve --person`), with the person as signer.
    if let Some(login) = person {
        let model = repo.read_model()?;
        return Ok(versions.iter().rposition(|v| !model.person_attestations(&v.id, login).is_empty()));
    }
    for (i, v) in versions.iter().enumerate().rev() {
        for a in repo.p.db.attestations(&v.id, Some("approval"))? {
            let mine = a.signer == repo.p.store.machine.id
                && repo.p.db.record(&a.attestation_id)?.is_some_and(|r| r.actor.uid == ctx.caller.uid);
            if mine {
                return Ok(Some(i));
            }
        }
    }
    Ok(None)
}

fn patch(repo: &Repo, state: &StateRec) -> Result<Vec<diff::FileDiff>> {
    repo.source_state(&state.id)?;
    let new = repo.state_src(&state.id)?;
    let empty = if state.parents.is_empty() {
        let path = std::env::temp_dir().join(format!("layr-change-{}", crate::ids::new_id()));
        std::fs::create_dir(&path)?;
        Some(crate::repo::Temp { path })
    } else {
        None
    };
    let old = match state.parents.first() {
        Some(p) => repo.state_src(p)?,
        None => crate::repo::snap(&empty.as_ref().unwrap().path),
    };
    let changes: Vec<_> = crate::changes::file_changes(&old, &new)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
    diff::file_diffs(&old, &new, &changes)
}

pub fn validate_comment(repo: &Repo, comment: &crate::model::Comment) -> Result<()> {
    let state = repo.source_state(&comment.version)?;
    if state.change_id.as_deref() != Some(&comment.change_id) {
        bail!("comment version does not belong to this change");
    }
    if let Some(reply) = &comment.reply_to {
        if !repo.read_model()?.comments(&comment.change_id).iter().any(|c| c["id"].as_str() == Some(reply)) {
            bail!("reply target does not belong to this change");
        }
    }
    Ok(())
}
