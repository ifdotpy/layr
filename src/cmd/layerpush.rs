//! Remotes per layer (docs/cache-overlays.md, section 7).
//!
//! A layr remote (store or peer) takes the whole repository: the shared set and every overlay.
//! A git remote takes one layer: the shared set goes to the project's git remotes, an overlay to
//! the git remotes in its own records (`overlay.<name>.git.<remote>`, which only its readers
//! see). A folder overlay (a cache) never goes to git. `layr push` without a remote name sends
//! the current line to all of them; `--dry-run` lists layer, remote, kind and size.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::gitbridge;
use crate::ids;
use crate::model::{Line, Record};
use crate::repo::Repo;
use anyhow::Result;
use std::collections::BTreeMap;

/// An overlay's git remote: its URL and, for a later slice, a branch (docs/cache-overlays.md,
/// section 7: the planned orphan branch of the same repository).
pub struct OverlayRemote {
    pub name: String,
    pub url: String,
    pub branch: Option<String>,
}

/// The git remotes of an overlay, from its own records.
pub fn overlay_remotes(repo: &Repo, overlay: &str) -> Result<Vec<OverlayRemote>> {
    let orepo = repo.overlay_repo(overlay)?;
    let prefix = format!("overlay.{overlay}.git.");
    let mut out: BTreeMap<String, OverlayRemote> = BTreeMap::new();
    for (key, _) in orepo.p.db.config_all()? {
        let Some(rest) = key.strip_prefix(&prefix) else { continue };
        let name = rest.split('@').next().unwrap_or("").to_string();
        if name.is_empty() || out.contains_key(&name) {
            continue;
        }
        let Some(v) = orepo.p.db.overlay_config(&format!("{prefix}{name}"))? else { continue };
        let Some(url) = v["url"].as_str() else { continue };
        out.insert(name.clone(), OverlayRemote { name, url: url.to_string(), branch: v["branch"].as_str().map(str::to_owned) });
    }
    Ok(out.into_values().collect())
}

/// `layr overlay remote add <overlay> <remote> <url> | list <overlay> | remove <overlay> <remote>`.
pub fn overlay_remote(ctx: &Ctx, args: &[String]) -> Result<i32> {
    const USAGE: &str =
        "usage: layr overlay remote add <overlay> <remote> <git url> | list <overlay> | remove <overlay> <remote>";
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let (sub, overlay) = match (a.pos.first(), a.pos.get(1)) {
        (Some(s), Some(o)) => (s.as_str(), o.clone()),
        _ => return Err(exit(128, USAGE)),
    };
    let info = repo.need_overlay(&overlay)?;
    if !repo.reads_overlay(&overlay, ctx.caller.uid, ctx.caller.gid)? {
        if info.hidden {
            return Err(exit(1, format!("error: no overlay '{overlay}' in project {}", repo.p.name)));
        }
        return Err(exit(1, format!("permission denied: you do not read overlay '{overlay}'")));
    }
    let orepo = repo.overlay_repo(&overlay)?;
    match sub {
        "list" => {
            for r in overlay_remotes(repo, &overlay)? {
                outln!(ctx, "{}\t{}", r.name, r.url);
            }
            Ok(0)
        }
        "add" | "remove" => {
            if !orepo.is_admin(ctx) {
                return Err(exit(1, format!("permission denied: only the owner of overlay '{overlay}' sets its remotes")));
            }
            let name = a.pos.get(2).ok_or_else(|| exit(128, USAGE))?;
            crate::store::check_name("remote", name)?;
            let value = if sub == "add" {
                if repo.overlay_setting(&overlay, "folder")?.is_some() {
                    return Err(exit(
                        1,
                        format!("error: overlay '{overlay}' is a folder overlay (a cache); caches never go to git"),
                    ));
                }
                let url = a.pos.get(3).ok_or_else(|| exit(128, USAGE))?;
                if url.starts_with('-') || url.chars().any(char::is_control) {
                    return Err(exit(128, "fatal: invalid git URL"));
                }
                serde_json::json!({"url": url, "branch": null})
            } else {
                serde_json::Value::Null
            };
            let _lock = orepo.p.lock()?;
            let mut rec = Record::new("config");
            rec.data = serde_json::json!({"config": {format!("overlay.{overlay}.git.{name}"): value}});
            orepo.commit_record(ctx, rec)?;
            Ok(0)
        }
        _ => Err(exit(128, USAGE)),
    }
}

/// One row of `layr push --dry-run`.
struct Row {
    layer: String,
    remote: String,
    kind: &'static str,
    size: String,
}

/// The git remotes of the project (the shared set).
fn git_remotes(repo: &Repo) -> Result<Vec<String>> {
    Ok(repo
        .p
        .db
        .config_all()?
        .into_iter()
        .filter_map(|(k, _)| k.strip_prefix("remote.").and_then(|s| s.strip_suffix(".url")).map(str::to_owned))
        .collect())
}

/// The layr remotes of the project (stores and peers; registries take one revision only).
fn layr_remotes(repo: &Repo) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (k, v) in repo.p.db.config_all()? {
        let Some(name) = k.strip_prefix("cloud.remote.").and_then(|s| s.strip_suffix(".url")) else { continue };
        let url: String = serde_json::from_str(&v).unwrap_or_default();
        if !matches!(super::remote::kind(&url), Ok(super::remote::Kind::Registry)) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// States of `repo` from `head` back that `pushed` (a state the remote has) does not reach.
fn new_states(repo: &Repo, head: &str, pushed: Option<&str>) -> Result<usize> {
    const LIMIT: usize = 10_000;
    let mut have = std::collections::BTreeSet::new();
    if let Some(p) = pushed {
        let mut stack = vec![p.to_string()];
        while let Some(s) = stack.pop() {
            if have.len() < LIMIT && have.insert(s.clone()) {
                stack.extend(repo.p.db.state(&s)?.map(|r| r.parents).unwrap_or_default());
            }
        }
    }
    let (mut seen, mut stack) = (std::collections::BTreeSet::new(), vec![head.to_string()]);
    while let Some(s) = stack.pop() {
        if have.contains(&s) || seen.len() >= LIMIT || !seen.insert(s.clone()) {
            continue;
        }
        stack.extend(repo.p.db.state(&s)?.map(|r| r.parents).unwrap_or_default());
    }
    Ok(seen.len())
}

/// The newest state that a push of `repo` sent to `remote`/`branch`.
fn pushed_state(repo: &Repo, remote: &str, branch: &str) -> Result<Option<String>> {
    Ok(repo.p.db.exports()?.into_iter().rev().find(|e| e.2 == remote && e.3 == branch && e.5 == "pushed").map(|e| e.1))
}

fn git_size(repo: &Repo, head: &str, remote: &str, branch: &str) -> Result<String> {
    let n = new_states(repo, head, pushed_state(repo, remote, branch)?.as_deref())?;
    let bytes = crate::overlay::tree_size(&repo.state_root(head)?);
    Ok(format!("{n} new state(s), tree {}", human(bytes)))
}

fn human(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// The state of overlay `name` that line `line` shows or saved: its overlay line's head.
fn overlay_head(repo: &Repo, line: &Line, name: &str) -> Result<Option<String>> {
    let info = repo.need_overlay(name)?;
    let ol = repo.overlay_project(name)?.db.line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))?;
    let head = match ol {
        Some(l) => Some(l.head),
        // An overlay that keeps parts: the part that the line shows.
        None if crate::overlay::parts_overlay(&info) => repo.layers(line)?.into_iter().find(|l| l.overlay == name).map(|l| l.state),
        None => None,
    };
    Ok(head.filter(|h| *h != info.root))
}

/// Export the history of an overlay state to its git store and push it to `url` `branch`.
fn push_overlay_git(ctx: &Ctx, repo: &Repo, name: &str, remote: &OverlayRemote, state: &str, branch: &str) -> Result<String> {
    let orepo = repo.overlay_repo(name)?;
    let _lock = orepo.p.lock()?;
    gitbridge::init_store(&orepo)?;
    let (commit, made) = gitbridge::export_history(&orepo, state)?;
    let commits: serde_json::Map<String, serde_json::Value> =
        made.into_iter().map(|(s, c)| (s, serde_json::Value::String(c))).collect();
    let eid = ids::new_id();
    let mut rec = Record::new("export");
    rec.data = serde_json::json!({"export": eid, "state": state, "remote": remote.name, "branch": branch, "commit": commit, "commits": commits, "status": "prepared"});
    orepo.commit_record(ctx, rec)?;
    gitbridge::push(ctx, &orepo, &remote.url, &commit, branch, false)?;
    let mut rec = Record::new("export");
    rec.data = serde_json::json!({"export": eid, "state": state, "remote": remote.name, "branch": branch, "commit": commit, "status": "pushed"});
    orepo.commit_record(ctx, rec)?;
    Ok(commit)
}

/// The push role of a remote (`remote.<name>.push`). A layr remote is `required` and a git remote `optional`, unless
/// the setting says otherwise; `off` leaves a remote out of a push that names no remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Role {
    Required,
    Optional,
    Off,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Required => "required",
            Role::Optional => "optional",
            Role::Off => "off",
        }
    }
}

/// The push role of remote `name` (`layr`: a store or peer remote, else a git remote).
pub(crate) fn role(repo: &Repo, name: &str, layr: bool) -> Result<Role> {
    match repo.p.config_value(&format!("remote.{name}.push"))? {
        None | Some(serde_json::Value::Null) => Ok(if layr { Role::Required } else { Role::Optional }),
        Some(serde_json::Value::String(v)) if v == "required" => Ok(Role::Required),
        Some(serde_json::Value::String(v)) if v == "optional" => Ok(Role::Optional),
        Some(serde_json::Value::String(v)) if v == "off" => Ok(Role::Off),
        Some(_) => anyhow::bail!("remote.{name}.push must be required, optional or off"),
    }
}

/// The pushes that failed on an optional remote, by remote: the lines of a git remote, `*` for a layr remote. The next
/// push to that remote (and the user daemon's tick) tries them again.
fn pending_path(repo: &Repo) -> std::path::PathBuf {
    repo.p.meta_dir().join("push-pending.json")
}

pub(crate) fn pending(repo: &Repo) -> BTreeMap<String, std::collections::BTreeSet<String>> {
    std::fs::read(pending_path(repo)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Output that a caller reads back (a remote's own `--json` report inside the combined one).
#[derive(Clone, Default)]
struct Capture(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `layr push [<remote>...] [--dry-run] [--json] [-q]`: the line of the command to its remotes. Without names: every
/// remote whose role is not `off`, and each overlay to its own git remotes; with names: those remotes. Required
/// remotes go first. A layr remote takes the whole repository; a git remote takes the line of the command, every
/// protected line and the lines that failed there before, each to the branch of its name. A failure on a required
/// remote fails the command; on an optional one it is a warning, and the next push tries again.
pub fn push_all(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("-n|--dry-run").flag("-q|--quiet").flag("--json"), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line()?.clone();
    let dry = a.has("-n");
    let named = a.pos.clone();
    let layr = layr_remotes(repo)?;
    let git = git_remotes(repo)?;
    if let Some(unknown) = named.iter().find(|n| !layr.contains(n) && !git.contains(n)) {
        return Err(exit(128, format!("fatal: no remote {unknown}")));
    }
    let mut targets = Vec::new();
    for (name, is_layr) in layr.iter().map(|n| (n, true)).chain(git.iter().map(|n| (n, false))) {
        let role = role(repo, name, is_layr)?;
        if (named.is_empty() && role != Role::Off) || named.contains(name) {
            targets.push((role, name.clone(), is_layr));
        }
    }
    targets.sort();
    let mut waiting = pending(repo);
    let policy = repo.policy();
    let protected: Vec<String> =
        repo.lines()?.into_iter().filter(|l| policy.rule_for(&l.name).is_some()).map(|l| l.name).collect();
    let git_lines = |remote: &str, waiting: &BTreeMap<String, std::collections::BTreeSet<String>>| -> Vec<String> {
        let mut lines: std::collections::BTreeSet<String> = waiting.get(remote).cloned().unwrap_or_default();
        lines.insert(line.name.clone());
        lines.extend(protected.iter().cloned());
        lines.into_iter().collect()
    };
    let mut rows = Vec::new();
    for (role, name, is_layr) in &targets {
        if *is_layr {
            rows.push(Row {
                layer: "repository".into(),
                remote: name.clone(),
                kind: "layr",
                size: format!("all layers ({})", role.name()),
            });
            for (overlay, folder) in repo.folder_overlays()? {
                if let Some(head) = overlay_head(repo, &line, &overlay)? {
                    let orepo = repo.overlay_repo(&overlay)?;
                    let platform = orepo.p.db.state(&head)?.and_then(|s| s.cache_platform).unwrap_or_default();
                    let bytes = crate::overlay::tree_size(&orepo.p.state_path(&head));
                    rows.push(Row {
                        layer: format!("{overlay} ({folder})"),
                        remote: name.clone(),
                        kind: "layr",
                        size: format!("1 state, {} ({platform})", human(bytes)),
                    });
                }
            }
        } else {
            for l in git_lines(name, &waiting) {
                let Some(found) = repo.p.db.line_by_name(&l)? else { continue };
                rows.push(Row {
                    layer: format!("shared ({l})"),
                    remote: name.clone(),
                    kind: "git",
                    size: format!("{} ({})", git_size(repo, &found.head, name, &l)?, role.name()),
                });
            }
        }
    }
    // Each overlay to its own git remotes (optional), only when no remote is named; folder overlays never.
    let mut overlay_pushes = Vec::new();
    if named.is_empty() {
        for name in repo.visible_overlay_names(ctx)? {
            if repo.overlay_setting(&name, "folder")?.is_some() || !repo.reads_overlay(&name, ctx.caller.uid, ctx.caller.gid)? {
                continue;
            }
            let Some(head) = overlay_head(repo, &line, &name)? else { continue };
            let orepo = repo.overlay_repo(&name)?;
            for r in overlay_remotes(repo, &name)? {
                let branch = r.branch.clone().unwrap_or_else(|| line.name.clone());
                rows.push(Row {
                    layer: name.clone(),
                    remote: r.name.clone(),
                    kind: "git",
                    size: git_size(&orepo, &head, &r.name, &branch)?,
                });
                overlay_pushes.push((name.clone(), r, head.clone(), branch));
            }
        }
    }
    if dry {
        outln!(ctx, "layer\tremote\tkind\tsize");
        for r in &rows {
            outln!(ctx, "{}\t{}\t{}\t{}", r.layer, r.remote, r.kind, r.size);
        }
        return Ok(0);
    }
    if targets.is_empty() && overlay_pushes.is_empty() {
        return Err(exit(1, "error: no remotes to push to; add one with layr remote add or layr git remote add"));
    }
    let json = a.has("--json");
    let quiet = a.has("-q") || json;
    let mut report = Vec::new();
    let mut required_failed = false;
    for (role, name, is_layr) in &targets {
        let (mut pushed, mut failed) = (Vec::<String>::new(), Vec::<serde_json::Value>::new());
        let mut store_report = serde_json::Value::Null;
        if *is_layr {
            let capture = Capture::default();
            let sub = Ctx {
                store: crate::store::Store::open(&ctx.store.root)?,
                caller: ctx.caller.clone(),
                cwd: ctx.cwd.clone(),
                env: ctx.env.clone(),
                author: Default::default(),
                out: std::cell::RefCell::new(if json {
                    Box::new(capture.clone())
                } else if quiet {
                    Box::new(std::io::sink())
                } else {
                    Box::new(std::io::stdout())
                }),
            };
            let mut v = vec![name.clone(), if json { "--json".to_string() } else { "-q".to_string() }];
            if !json && !quiet {
                v.pop();
            }
            match super::remote::push(&sub, &v) {
                Ok(0) => pushed.push("*".into()),
                Ok(code) => failed.push(serde_json::json!({"line": "*", "error": format!("push exited {code}")})),
                Err(e) => failed.push(serde_json::json!({"line": "*", "error": format!("{e:#}")})),
            }
            if json {
                store_report = serde_json::from_slice(&capture.0.borrow()).unwrap_or(serde_json::Value::Null);
            }
        } else {
            for l in git_lines(name, &waiting) {
                if repo.p.db.line_by_name(&l)?.is_none() {
                    continue;
                }
                let sub = Ctx {
                    store: crate::store::Store::open(&ctx.store.root)?,
                    caller: ctx.caller.clone(),
                    cwd: ctx.cwd.clone(),
                    env: ctx.env.clone(),
                    author: Default::default(),
                    out: std::cell::RefCell::new(Box::new(std::io::sink())),
                };
                match super::remote::git::push(&sub, &[name.clone(), l.clone(), "-q".to_string()]) {
                    Ok(0) => pushed.push(l),
                    Ok(code) => failed.push(serde_json::json!({"line": l, "error": format!("git push exited {code}")})),
                    Err(e) => failed.push(serde_json::json!({"line": l, "error": format!("{e:#}")})),
                }
            }
        }
        let entry = waiting.entry(name.clone()).or_default();
        entry.clear();
        for f in &failed {
            let what = f["line"].as_str().unwrap_or("*");
            if *role == Role::Required {
                required_failed = true;
                eprintln!("error: push to {name} ({what}): {}", f["error"].as_str().unwrap_or(""));
            } else {
                entry.insert(what.to_string());
                eprintln!(
                    "warning: push to {name} ({what}) failed: {}; the next push tries again",
                    f["error"].as_str().unwrap_or("")
                );
            }
        }
        if !quiet {
            outln!(
                ctx,
                "push {name} ({}, {}): {} pushed, {} failed",
                if *is_layr { "layr" } else { "git" },
                role.name(),
                pushed.len(),
                failed.len()
            );
        }
        report.push(serde_json::json!({"name": name, "kind": if *is_layr { "layr" } else { "git" }, "role": role.name(),
            "pushed": pushed, "failed": failed, "report": store_report}));
    }
    waiting.retain(|_, lines| !lines.is_empty());
    if waiting.is_empty() {
        let _ = std::fs::remove_file(pending_path(repo));
    } else {
        crate::fsutil::atomic_write(&pending_path(repo), &serde_json::to_vec(&waiting)?, 0o600)?;
    }
    for (name, r, head, branch) in overlay_pushes {
        match push_overlay_git(ctx, repo, &name, &r, &head, &branch) {
            Ok(c) if !quiet => outln!(ctx, "overlay {name} -> {} {branch}: {}", r.name, &c[..10]),
            Ok(_) => {}
            Err(e) => eprintln!("warning: overlay {name} to {}: {e:#}", r.name),
        }
    }
    if json {
        outln!(ctx, "{}", serde_json::json!({"remotes": report}));
    }
    Ok(if required_failed { 1 } else { 0 })
}
