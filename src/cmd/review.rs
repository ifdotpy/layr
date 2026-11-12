//! Review commands: `comment` (threads, replies, resolve), `react` and `review` (reviewer requests). The records
//! and their merge rules are in `layr_model::review`.

use super::change::{find, validate_comment, version};
use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::model::{Comment, Record};
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use layr_model::review::{self as rv, Reaction, ReviewRequest, ThreadStatus};
use serde_json::{json, Value};

const COMMENT_USAGE: &str = "usage: layr comment <change> (-m <text> | <text>) [--file <path> --line <a>[-<b>]] \
[--state <rev> | --version vN] [--json]
   or: layr comment reply <comment> (-m <text> | <text>) [--json]
   or: layr comment (resolve | unresolve) <comment> [--json]";

/// `layr comment`: start a thread, reply to one, or resolve and unresolve one.
pub fn comment(ctx: &Ctx, args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("reply") => return reply(ctx, &args[1..]),
        Some("resolve") => return resolve(ctx, &args[1..], true),
        Some("unresolve") => return resolve(ctx, &args[1..], false),
        _ => {}
    }
    let a = parse(
        &Spec::new()
            .value("--version")
            .value("--state")
            .value("--path|--file")
            .value("--line")
            .value("--reply-to")
            .value("-m|--message")
            .flag("--json"),
        args,
    )?;
    let (change, text) = match (a.pos.as_slice(), a.get("-m")) {
        ([change], Some(text)) => (change, text.to_owned()),
        ([change, text], None) => (change, text.clone()),
        _ => bail!("{COMMENT_USAGE}"),
    };
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    let _lock = h.repo.p.lock()?;
    let id = find(&h.repo, change)?;
    let versions = h.repo.p.db.change_versions(&id)?;
    let state = match (a.get("--state"), a.get("--version")) {
        (Some(_), Some(_)) => bail!("--state and --version both name the version; give one"),
        (Some(rev), None) => {
            let state = crate::revs::resolve(&h.repo, h.line.as_ref(), rev)?;
            if !versions.iter().any(|v| v.id == state) {
                bail!("state {} is no version of change {id}", crate::ids::short(&state));
            }
            state
        }
        (None, n) => {
            let n = n.map(version).transpose()?.unwrap_or(versions.len().saturating_sub(1));
            versions.get(n).context("unknown change version")?.id.clone()
        }
    };
    let (line, line_end) = match a.get("--line") {
        Some(range) => {
            let (start, end) = lines(range)?;
            (Some(start), end)
        }
        None => (None, None),
    };
    let comment = Comment {
        id: crate::ids::new_id(),
        change_id: id,
        version: state,
        path: a.get("--path").map(str::to_owned),
        line,
        text,
        reply_to: a.get("--reply-to").map(str::to_owned),
    };
    write_comment(ctx, &h.repo, comment, line_end, a.has("--json"))
}

/// A line or a line range: `12` or `12-20`.
fn lines(value: &str) -> Result<(u32, Option<u32>)> {
    let bad = || anyhow::anyhow!("bad --line '{value}': use a positive line number or a range such as 12-20");
    let (start, end) = match value.split_once('-') {
        Some((a, b)) => (a.parse::<u32>().map_err(|_| bad())?, Some(b.parse::<u32>().map_err(|_| bad())?)),
        None => (value.parse::<u32>().map_err(|_| bad())?, None),
    };
    if start == 0 || end.is_some_and(|e| e < start) {
        return Err(bad());
    }
    Ok((start, end.filter(|e| *e != start)))
}

fn write_comment(ctx: &Ctx, repo: &Repo, comment: Comment, line_end: Option<u32>, as_json: bool) -> Result<i32> {
    if line_end.is_some() && comment.path.is_none() {
        bail!("--line needs --file");
    }
    validate_comment(repo, &comment)?;
    let mut rec = Record::new("comment");
    rec.data = json!({"comment":comment});
    if let Some(end) = line_end {
        rec.data["line_end"] = json!(end);
    }
    let rec = repo.commit_record(ctx, rec)?;
    if as_json {
        let mut out = rec.data["comment"].clone();
        if let Some(end) = rec.data.get("line_end") {
            out["line_end"] = end.clone();
        }
        outln!(ctx, "{out}");
    } else {
        outln!(ctx, "comment {}", comment.id);
    }
    Ok(0)
}

/// The text of a comment command: `-m <text>` or one positional argument after the comment.
fn text_arg(a: &crate::args::Parsed, usage: &str) -> Result<(String, String)> {
    match (a.pos.as_slice(), a.get("-m")) {
        ([target], Some(text)) => Ok((target.clone(), text.to_owned())),
        ([target, text], None) => Ok((target.clone(), text.clone())),
        _ => bail!("{usage}"),
    }
}

fn reply(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("-m|--message").flag("--json"), args)?;
    let (target, text) = text_arg(&a, "usage: layr comment reply <comment> (-m <text> | <text>) [--json]")?;
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    let _lock = h.repo.p.lock()?;
    let model = h.repo.read_model()?;
    let to = model.find_comment(&target)?;
    let change = to["change_id"].as_str().context("comment has no change")?;
    let root = model.thread_root(change, to["id"].as_str().unwrap_or(""));
    let root = model.find_comment(&root)?;
    let comment = Comment {
        id: crate::ids::new_id(),
        change_id: change.to_owned(),
        version: root["version"].as_str().context("comment has no version")?.to_owned(),
        path: None,
        line: None,
        text,
        reply_to: to["id"].as_str().map(String::from),
    };
    write_comment(ctx, &h.repo, comment, None, a.has("--json"))
}

fn resolve(ctx: &Ctx, args: &[String], resolved: bool) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    let [target] = a.pos.as_slice() else {
        bail!("usage: layr comment {} <comment> [--json]", if resolved { "resolve" } else { "unresolve" });
    };
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    let _lock = h.repo.p.lock()?;
    let model = h.repo.read_model()?;
    let comment = model.find_comment(target)?;
    let change = comment["change_id"].as_str().context("comment has no change")?.to_owned();
    let thread = model.thread_root(&change, comment["id"].as_str().unwrap_or(""));
    let (now, prev) = model.thread_status(&change, &thread);
    let word = if resolved { "resolved" } else { "open" };
    if now == resolved {
        if a.has("--json") {
            outln!(ctx, "{}", json!({"thread":thread,"change_id":change,"resolved":now,"written":false}));
        } else {
            outln!(ctx, "thread {thread} is already {word}");
        }
        return Ok(0);
    }
    let mut rec = Record::new(if resolved { rv::RESOLVE_OP } else { rv::UNRESOLVE_OP });
    rec.data = json!({"thread_status": ThreadStatus { change_id: change.clone(), thread: thread.clone(), resolved, prev }});
    h.repo.commit_record(ctx, rec)?;
    if a.has("--json") {
        outln!(ctx, "{}", json!({"thread":thread,"change_id":change,"resolved":resolved,"written":true}));
    } else {
        outln!(ctx, "thread {thread} {word}");
    }
    Ok(0)
}

/// `layr react <comment-or-change> <name> [--remove]`.
pub fn react(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--remove").flag("--json"), args)?;
    let [target, name] = a.pos.as_slice() else {
        bail!("usage: layr react <comment-or-change> <name> [--remove] [--json]");
    };
    if !rv::reaction_name(name) {
        bail!("bad reaction name '{name}': use 1 to 32 of a-z, 0-9, '_', '+' and '-' (for example +1 or heart)");
    }
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    let _lock = h.repo.p.lock()?;
    let model = h.repo.read_model()?;
    let comment = model.find_comment(target);
    let change = find(&h.repo, target);
    let (change, target) = match (comment, change) {
        (Ok(_), Ok(_)) => bail!("'{target}' names a comment and a change; give more characters"),
        (Ok(c), Err(_)) => (
            c["change_id"].as_str().context("comment has no change")?.to_owned(),
            c["id"].as_str().context("comment has no id")?.to_owned(),
        ),
        (Err(_), Ok(change)) => (change.clone(), change),
        (Err(_), Err(_)) => bail!("'{target}' names no comment and no change"),
    };
    let on = !a.has("--remove");
    let writer = layr_model::Project::reaction_writer(ctx.caller.uid, &h.repo.p.store.machine.id);
    let written = model.has_reaction(&change, &writer, &target, name) != on;
    if written {
        let mut rec = Record::new(if on { rv::REACT_OP } else { rv::UNREACT_OP });
        rec.data = json!({"reaction": Reaction { change_id: change.clone(), target: target.clone(), name: name.clone(), on }});
        h.repo.commit_record(ctx, rec)?;
    }
    if a.has("--json") {
        outln!(ctx, "{}", json!({"change_id":change,"target":target,"name":name,"on":on,"written":written}));
    } else {
        let what = match (on, written) {
            (true, true) => "added",
            (true, false) => "is already there",
            (false, true) => "removed",
            (false, false) => "was not there",
        };
        outln!(ctx, "reaction {name} on {target} {what}");
    }
    Ok(0)
}

/// `layr review (request|cancel) <change> <reviewer>...`.
pub fn review(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let usage = "usage: layr review (request | cancel) <change> <reviewer>... [--json]\n\
reviewer: person:<login>, user:<name or uid>, <user name>, or machine:<name or id>";
    let a = parse(&Spec::new().flag("--json"), args)?;
    let (active, change, names) = match a.pos.as_slice() {
        [cmd, change, names @ ..] if !names.is_empty() && cmd == "request" => (true, change, names),
        [cmd, change, names @ ..] if !names.is_empty() && cmd == "cancel" => (false, change, names),
        _ => bail!("{usage}"),
    };
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    let _lock = h.repo.p.lock()?;
    let change = find(&h.repo, change)?;
    let mut model = h.repo.read_model()?;
    let reviewers = names.iter().map(|n| principal(&model, n)).collect::<Result<Vec<_>>>()?;
    let mut rows = Vec::new();
    for reviewer in reviewers {
        let (now, prev) = model.review_request(&change, &reviewer);
        let written = now != active;
        if written {
            let mut rec = Record::new(if active { rv::REQUEST_OP } else { rv::CANCEL_OP });
            rec.data =
                json!({"review_request": ReviewRequest { change_id: change.clone(), reviewer: reviewer.clone(), active, prev }});
            h.repo.commit_record(ctx, rec)?;
            model = h.repo.read_model()?;
        }
        if !a.has("--json") {
            let what = match (active, written) {
                (true, true) => "requested",
                (true, false) => "is already requested",
                (false, true) => "canceled",
                (false, false) => "was not requested",
            };
            outln!(ctx, "review of {reviewer} {what}");
        }
        rows.push(json!({"reviewer":reviewer,"active":active,"written":written}));
    }
    if a.has("--json") {
        outln!(ctx, "{}", json!({"change_id":change,"reviewers":rows}));
    }
    Ok(0)
}

/// The stored form of a reviewer as typed: `person:<login>`, `user:<name or uid>`, a user name, or
/// `machine:<name or id>` of a machine that wrote records in this project.
fn principal(model: &layr_model::Project, typed: &str) -> Result<String> {
    if let Some(machine) = typed.strip_prefix("machine:") {
        if crate::model::is_uuid(machine) {
            return Ok(typed.to_owned());
        }
        let ids: Vec<&String> =
            model.machines.iter().filter(|(_, d)| d["name"].as_str() == Some(machine)).map(|(id, _)| id).collect();
        return match ids.as_slice() {
            [id] => Ok(format!("machine:{id}")),
            [] => bail!("no machine '{machine}' wrote records in this project; give its id"),
            _ => bail!("two machines are named '{machine}'; give the id"),
        };
    }
    match crate::access::parse_principal(typed) {
        Some(p) if rv::reviewer(&p) => Ok(p),
        _ => bail!("bad reviewer '{typed}': use person:<login>, user:<name or uid>, a user name, or machine:<name or id>"),
    }
}

/// The review part of human `change show`: threads, reactions and reviewers.
pub fn print(ctx: &Ctx, review: &Value) {
    let text = |c: &Value| -> String {
        let mut line = format!(
            "comment {} by {}: {}",
            c["id"].as_str().unwrap_or(""),
            c["actor"]["name"].as_str().unwrap_or(""),
            c["text"].as_str().unwrap_or("")
        );
        let reactions = counts(&c["reactions"]);
        if !reactions.is_empty() {
            line.push_str(&format!(" [{reactions}]"));
        }
        line
    };
    let threads = review["threads"].as_array().map(Vec::as_slice).unwrap_or_default();
    if !threads.is_empty() {
        outln!(ctx, "Threads");
    }
    for t in threads {
        let mut place = t["version_label"].as_str().unwrap_or("").to_owned();
        if let Some(path) = t["path"].as_str() {
            place.push_str(&format!(" {path}"));
            match (t["line"].as_u64(), t["line_end"].as_u64()) {
                (Some(a), Some(b)) => place.push_str(&format!(":{a}-{b}")),
                (Some(a), None) => place.push_str(&format!(":{a}")),
                _ => {}
            }
        }
        let status = if t["resolved"] == true {
            format!("resolved by {}", t["resolved_by"]["name"].as_str().unwrap_or(""))
        } else {
            "open".to_owned()
        };
        outln!(ctx, "  thread {} {status}, {}", t["id"].as_str().unwrap_or(""), place.trim());
        outln!(ctx, "    {}", text(&t["comment"]));
        for r in t["replies"].as_array().into_iter().flatten() {
            outln!(ctx, "      {}", text(r));
        }
    }
    let reactions = counts(&review["reactions"]);
    if !reactions.is_empty() {
        outln!(ctx, "Reactions: {reactions}");
    }
    let reviewers = review["reviewers"].as_array().map(Vec::as_slice).unwrap_or_default();
    if !reviewers.is_empty() {
        outln!(ctx, "Reviewers");
    }
    for r in reviewers {
        let version = r["approved_version"].as_str().map(|v| format!(" {v}")).unwrap_or_default();
        outln!(ctx, "  {}\t{}{version}", r["reviewer"].as_str().unwrap_or(""), r["status"].as_str().unwrap_or(""));
    }
}

/// `+1 2, heart 1` of a name-to-count object.
fn counts(map: &Value) -> String {
    map.as_object().into_iter().flatten().map(|(name, n)| format!("{name} {n}")).collect::<Vec<_>>().join(", ")
}
