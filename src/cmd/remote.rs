//! Named remotes and the Git bridge.

pub mod git {

    use crate::args::{parse, Spec};
    use crate::cmd::admin::set_config;
    use crate::cmd::{exit, here, mergecmd};
    use crate::ctx::Ctx;
    use crate::gitbridge;
    use crate::ids;
    use crate::model::Record;
    use crate::repo::Repo;
    use crate::revs;
    use anyhow::Result;

    pub fn remote(ctx: &Ctx, args: &[String]) -> Result<i32> {
        let h = here(ctx)?;
        let repo = &h.repo;
        let sub = args.first().map(|s| s.as_str()).unwrap_or("");
        let remotes = || -> Result<Vec<(String, String)>> {
            let mut v = Vec::new();
            for (k, val) in repo.p.db.config_all()? {
                if let Some(n) = k.strip_prefix("remote.").and_then(|x| x.strip_suffix(".url")) {
                    let url: String = serde_json::from_str(&val).unwrap_or(val.clone());
                    v.push((n.to_string(), url));
                }
            }
            Ok(v)
        };
        match sub {
            "" | "-v" | "--verbose" => {
                for (n, u) in remotes()? {
                    if sub.is_empty() {
                        outln!(ctx, "{n}");
                    } else {
                        outln!(ctx, "{n}\t{u} (fetch)\n{n}\t{u} (push)");
                    }
                }
                Ok(0)
            }
            "add" | "set-url" => {
                let (n, u) = match (args.get(1), args.get(2)) {
                    (Some(n), Some(u)) => (n, u),
                    _ => return Err(exit(128, "usage: layr git remote add <name> <url>")),
                };
                repo.require(ctx, "remote")?;
                crate::store::check_name("remote", n)?;
                if n == GIT_SYNC_REMOTE {
                    return Err(exit(128, format!("error: the remote name {n} is reserved for layr git sync")));
                }
                if sub == "add" && repo.p.config_value(&format!("remote.{n}.url"))?.is_some() {
                    return Err(exit(3, format!("error: remote {n} already exists.")));
                }
                let _lock = repo.p.lock()?;
                set_config(ctx, repo, &format!("remote.{n}.url"), serde_json::json!(u))?;
                gitbridge::init_store(repo)?;
                Ok(0)
            }
            "remove" | "rm" => {
                let n = args.get(1).ok_or_else(|| exit(128, "usage: layr git remote remove <name>"))?;
                repo.require(ctx, "remote")?;
                let _lock = repo.p.lock()?;
                set_config(ctx, repo, &format!("remote.{n}.url"), serde_json::Value::Null)?;
                gitbridge::drop_remote_refs(repo, n)?;
                let mut rec = Record::new("remote.remove");
                rec.data = serde_json::json!({"remote": n});
                repo.commit_record(ctx, rec)?;
                Ok(0)
            }
            "get-url" => {
                let n = args.get(1).ok_or_else(|| exit(128, "usage: layr git remote get-url <name>"))?;
                outln!(ctx, "{}", gitbridge::remote_url(repo, n)?);
                Ok(0)
            }
            _ => Err(exit(
                128,
                "usage: layr git remote [-v | add <name> <url> | remove <name> | get-url <name> | set-url <name> <url>]",
            )),
        }
    }

    /// The remote refs to import after a fetch: the requested ones, the ones imported before, and the
    /// ones that match a line name or the default branch.
    fn wanted_refs(repo: &Repo, remote: &str, refs: &[(String, String)], only: Option<&str>) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let lines: Vec<String> = repo.lines()?.into_iter().map(|l| l.name).collect();
        for (name, commit) in refs {
            let branch = name.strip_prefix(&format!("{remote}/")).unwrap_or(name);
            if branch == "HEAD" {
                continue;
            }
            let wanted = match only {
                Some(o) => o == branch,
                None => {
                    repo.p.db.remote_ref(name)?.is_some()
                        || lines.iter().any(|l| l == branch)
                        || ["main", "master"].contains(&branch)
                }
            };
            if wanted {
                out.push((name.clone(), commit.clone()));
            }
        }
        Ok(out)
    }

    /// Tries of an import whose remote ref moved while it was planned (`gitbridge::Moved`).
    const IMPORT_TRIES: usize = 3;

    /// Import the wanted refs with no project lock held. Each ref is planned without the lock and
    /// written under it in short chunks (`gitbridge::import_ref`). A ref that moved meanwhile is planned again.
    fn import_after_fetch(
        ctx: &Ctx,
        repo: &Repo,
        remote: &str,
        refs: &[(String, String)],
        only: Option<&str>,
    ) -> Result<Vec<(String, String, bool)>> {
        let mut refs = refs.to_vec();
        for attempt in 1..=IMPORT_TRIES {
            let wanted = wanted_refs(repo, remote, &refs, only)?;
            match import_wanted(ctx, repo, &wanted) {
                Ok(out) => return Ok(out),
                Err(e) if e.is::<gitbridge::Moved>() && attempt < IMPORT_TRIES => {
                    // Plan again from the refs the Git store holds now: a concurrent fetch may have stored a newer tip.
                    refs = gitbridge::stored_remote_refs(repo, remote)?;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("the last try returns")
    }

    fn import_wanted(ctx: &Ctx, repo: &Repo, wanted: &[(String, String)]) -> Result<Vec<(String, String, bool)>> {
        let mut out = Vec::new();
        for (name, commit) in wanted {
            let prev = repo.p.db.remote_ref(name)?.map(|p| p.0);
            let st = gitbridge::import_ref(ctx, repo, commit, name, prev.clone())?;
            out.push((name.clone(), st, prev.is_none()));
        }
        Ok(out)
    }

    pub fn fetch(ctx: &Ctx, args: &[String]) -> Result<i32> {
        let spec = Spec::new().flag("--all").flag("-q|--quiet").flag("-p|--prune").flag("--tags").flag("-v").value("--depth");
        let a = parse(&spec, args)?;
        let h = here(ctx)?;
        let repo = &h.repo;
        let remote = a.pos.first().cloned().unwrap_or_else(|| "origin".into());
        let only = a.pos.get(1).cloned();
        let refs = gitbridge::fetch(ctx, repo, &remote)?;
        let done = import_after_fetch(ctx, repo, &remote, &refs, only.as_deref())?;
        gitbridge::refresh_all_packed_refs(repo)?;
        if !a.has("-q") {
            let url = gitbridge::remote_url(repo, &remote)?;
            if !done.is_empty() {
                outln!(ctx, "From {url}");
            }
            for (name, st, new) in done {
                outln!(
                    ctx,
                    " {} {:<20} -> {} (state {})",
                    if new { "* [new]" } else { "  " },
                    name.split_once('/').map_or(name.as_str(), |(_, branch)| branch),
                    name,
                    ids::short(&st)
                );
            }
        }
        Ok(0)
    }

    pub fn pull(ctx: &Ctx, args: &[String]) -> Result<i32> {
        let spec = Spec::new()
            .flag("--ff-only")
            .flag("--no-ff")
            .flag("--rebase|-r")
            .flag("-q|--quiet")
            .flag("--no-rebase")
            .value("--expect");
        let a = parse(&spec, args)?;
        let h = here(ctx)?;
        let line = h.line()?.clone();
        let remote = a.pos.first().cloned().unwrap_or_else(|| "origin".into());
        let refs = gitbridge::fetch(ctx, &h.repo, &remote)?;
        gitbridge::refresh_all_packed_refs(&h.repo)?;
        let branch = match a.pos.get(1) {
            Some(b) => b.clone(),
            None => {
                if refs.iter().any(|(n, _)| n == &format!("{remote}/{}", line.name)) {
                    line.name.clone()
                } else if refs.iter().any(|(n, _)| n == &format!("{remote}/main")) {
                    "main".into()
                } else {
                    refs.iter()
                        .map(|(n, _)| n.split('/').nth(1).unwrap_or("").to_string())
                        .find(|b| b == "master")
                        .unwrap_or_else(|| "main".into())
                }
            }
        };
        import_after_fetch(ctx, &h.repo, &remote, &refs, Some(&branch))?;
        let rev = format!("{remote}/{branch}");
        if h.repo.p.db.remote_ref(&rev)?.is_none() {
            return Err(exit(1, format!("fatal: couldn't find remote ref {branch}")));
        }
        if a.has("-r") {
            return mergecmd::rebase(ctx, &[rev]);
        }
        let mut margs = vec![rev];
        for f in ["--ff-only", "--no-ff", "-q"] {
            if a.has(f) {
                margs.push(f.into());
            }
        }
        let pa = parse(&Spec::new().flag("--ff-only").flag("--no-ff").flag("-q|--quiet").value("--expect"), &margs[1..])?;
        mergecmd::merge_rev(ctx, &h.repo, &line, &margs[0], &pa)
    }

    /// Give the line's `.git` folder a commit of the line's head without a remote, so tools that
    /// read git HEAD see the current state. The commits are the ones `layr git push` would send for the
    /// same states; nothing leaves the machine.
    /// The remote name of a `git-sync` export record: no remote exists, so `layr git remote add` refuses it.
    const GIT_SYNC_REMOTE: &str = "git-sync";

    pub fn sync(ctx: &Ctx, args: &[String]) -> Result<i32> {
        let spec = Spec::new().flag("-q|--quiet");
        let a = parse(&spec, args)?;
        if !a.pos.is_empty() {
            return Err(exit(128, "usage: layr git sync [-q]"));
        }
        let h = here(ctx)?;
        let repo = &h.repo;
        let line = h.line()?.clone();
        repo.require(ctx, "export").map_err(|e| exit(1, e.to_string()))?;
        repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
        let line = repo.line(&line.id)?;
        gitbridge::check_git_head(repo, &line)?;
        let state = repo.state(&line.head)?;
        // The export is read and written as Git objects: pin the state, hold no project lock.
        repo.hold(repo.pin(&[state.id.as_str()])?);
        if !state.conflicts.is_empty() {
            return Err(exit(
                1,
                format!(
                    "error: state {} has {} open conflict(s); resolve and commit them first",
                    ids::short(&state.id),
                    state.conflicts.len()
                ),
            ));
        }
        let (commit, made) = gitbridge::export_history(repo, &state.id)?;
        if !made.is_empty() || repo.p.db.git_commit_of(&state.id)?.is_none() {
            // An export without a remote: the record keeps the commits of the states, as a push does.
            let commits: serde_json::Map<String, serde_json::Value> =
                made.into_iter().map(|(s, c)| (s, serde_json::Value::String(c))).collect();
            let mut rec = Record::new("export");
            rec.data = serde_json::json!({"export": ids::new_id(), "state": state.id, "remote": GIT_SYNC_REMOTE, "branch": line.name, "commit": commit, "commits": commits, "status": "local"});
            let _lock = repo.p.lock()?;
            repo.commit_record(ctx, rec)?;
        }
        if !repo.working(&line).join(".git").is_dir() {
            return Err(exit(
                1,
                format!("error: line '{}' has no .git folder; commit {} is in the project git store", line.name, &commit[..10]),
            ));
        }
        refresh_line_git(repo, &line)?;
        if !a.has("-q") {
            outln!(ctx, "{} (state {})", commit, ids::short(&state.id));
        }
        Ok(0)
    }

    /// Write the .git folder of `line` under the line lock, not the project lock. A save may move the
    /// head meanwhile: the folder then shows an older head, so it is written again (three tries).
    fn refresh_line_git(repo: &Repo, line: &crate::model::Line) -> Result<()> {
        for _ in 0..3 {
            let current = repo.line(&line.id)?;
            let _line = repo.line_lock(&current)?;
            gitbridge::refresh_git_folder(repo, &current)?;
            if repo.line(&line.id)?.head == current.head {
                return Ok(());
            }
        }
        Err(exit(1, format!("line '{}' moved during the .git refresh; run the command again", line.name)))
    }

    pub fn push(ctx: &Ctx, args: &[String]) -> Result<i32> {
        let spec = Spec::new()
            .flag("-f|--force")
            .flag("--force-with-lease")
            .flag("-n|--dry-run")
            .flag("-u|--set-upstream")
            .flag("-q|--quiet")
            .value("-m|--message")
            .flag("--squash")
            .flag("--no-verify")
            .value("--include-base")
            .flag("--tags");
        let a = parse(&spec, args)?;
        let h = here(ctx)?;
        let repo = &h.repo;
        let line = h.line.clone();
        if let Some(l) = &line {
            gitbridge::check_git_head(repo, l)?;
        }
        let remote = a.pos.first().cloned().unwrap_or_else(|| "origin".into());
        let (rev, branch) = match a.pos.get(1) {
            Some(spec) => match spec.split_once(':') {
                Some((r, b)) => (r.to_string(), b.trim_start_matches("refs/heads/").to_string()),
                None => {
                    if repo.p.db.line_by_name(spec)?.is_some() {
                        (spec.clone(), spec.clone())
                    } else {
                        ("HEAD".to_string(), spec.clone())
                    }
                }
            },
            None => ("HEAD".to_string(), h.line()?.name.clone()),
        };
        repo.require(ctx, "push").map_err(|e| exit(1, e.to_string()))?;
        // The Git bridge keeps Git revision spelling at its public boundary.
        let source = match rev.strip_prefix("HEAD") {
            Some(suffix) if suffix.is_empty() || suffix.starts_with(['~', '^']) || suffix.starts_with("@{") => {
                format!("head{suffix}")
            }
            _ => rev.clone(),
        };
        let state_id = revs::resolve(repo, line.as_ref(), &source)?;
        // A line started on unpushed states of another line would publish that line's work too.
        let owner = match repo.p.db.line_by_name(&source)? {
            Some(l) => Some(repo.localize(l)?),
            None => line.clone(),
        };
        if let Some(l) = &owner {
            // `--include-base <line>[,<line>]` names each line whose work goes out on purpose.
            let named: Vec<&str> = a.get("--include-base").map(|v| v.split(',').collect()).unwrap_or_default();
            let mut foreign = Vec::new();
            for s in repo.foreign_base(l, &state_id)? {
                let from = s.line.as_deref().map(|id| repo.p.db.line_by_id(id)).transpose()?.flatten();
                let from = from.map(|x| x.name).unwrap_or_else(|| "a deleted line".into());
                if !named.contains(&from.as_str()) {
                    foreign.push((s, from));
                }
            }
            if !foreign.is_empty() {
                let mut msg = format!(
                    "error: line '{}' started on {} unpushed state(s) of other lines; this push would publish them:",
                    l.name,
                    foreign.len()
                );
                for (s, from) in foreign.iter().take(10) {
                    msg.push_str(&format!("\n  {} (line {from}) {}", ids::short(&s.id), s.message.lines().next().unwrap_or("")));
                }
                if foreign.len() > 10 {
                    msg.push_str(&format!("\n  ... and {} more", foreign.len() - 10));
                }
                let mut lines: Vec<&str> = foreign.iter().map(|(_, f)| f.as_str()).collect();
                lines.sort();
                lines.dedup();
                msg.push_str(&format!(
                    "\nThat work belongs to its own line and pull request. Start a new line from the remote branch ('layr line new <name> {remote}/main') and pick your states.\nOnly if you mean to publish it here: --include-base {}",
                    lines.join(",")
                ));
                return Err(exit(1, msg));
            }
        }
        crate::secrets::check_history(ctx, repo, &state_id)?;
        let state = repo.state(&state_id)?;
        // The state is read by the export below: keep it on disk until the command ends.
        repo.hold(repo.pin(&[state_id.as_str()])?);
        if !state.conflicts.is_empty() {
            return Err(exit(
                1,
                format!(
                    "error: state {} has {} open conflict(s); resolve and commit them before a push",
                    ids::short(&state_id),
                    state.conflicts.len()
                ),
            ));
        }
        if repo.p.config_bool("push.require_check", false) {
            let ok = repo.p.db.checks(&state_id)?.iter().any(|c| c.1 == 0);
            if !ok {
                return Err(exit(
                    1,
                    format!(
                        "error: state {} has no passing check; run 'layr try {} -- <tests>' first",
                        ids::short(&state_id),
                        ids::short(&state_id)
                    ),
                ));
            }
        }
        let url = gitbridge::remote_url(repo, &remote)?;
        // A push prepared earlier (for example before a lost response): read the remote ref first.
        let mut prepared: Option<(String, String)> = None;
        let squash_commits = repo.p.db.squash_commits_of(&state_id)?;
        for (eid, st, r, b, c, status) in repo.p.db.exports()? {
            // A squash commit answers only a push with --squash; a history commit answers only one without.
            if squash_commits.contains(&c) != a.has("--squash") {
                continue;
            }
            if st == state_id && r == remote && b == branch && status == "prepared" {
                prepared = Some((eid, c.clone()));
            }
            if st == state_id && r == remote && b == branch && status == "pushed" {
                prepared = None;
                if !a.has("-f") && gitbridge::ls_remote(ctx, repo, &url, &branch)?.as_deref() == Some(c.as_str()) {
                    outln!(ctx, "Everything up-to-date ({} is {} on {remote}/{branch})", ids::short(&state_id), &c[..10]);
                    return Ok(0);
                }
            }
        }
        // The network steps (ls-remote, push) run without the store lock: a push takes seconds
        // to minutes, and every other command of the project waited for it. The export records
        // ("prepared", then "pushed") keep a push whose answer was lost recoverable.
        if let Some((eid, c)) = &prepared {
            if gitbridge::ls_remote(ctx, repo, &url, &branch)?.as_deref() == Some(c.as_str()) {
                let _lock = repo.p.lock()?;
                let mut rec = Record::new("export");
                rec.data = serde_json::json!({"export": eid, "state": state_id, "remote": remote, "branch": branch, "commit": c, "status": "pushed", "verified": "remote ref"});
                repo.commit_record(ctx, rec)?;
                outln!(ctx, "Already pushed before: {remote}/{branch} is {} (state {})", &c[..10], ids::short(&state_id));
                return Ok(0);
            }
        }
        // The export commits are Git writes and tree reads: they run with no project lock. Only the
        // prepared record is written under it.
        let (export_id, commit) = match prepared {
            Some((eid, c)) => (eid, c),
            None => {
                if a.get("-m").is_some() && !a.has("--squash") {
                    return Err(exit(128, "fatal: -m names the message of a squashed commit: add --squash"));
                }
                // The line's history as commits (merges stay merges), or with --squash one commit on
                // top of the last exported state.
                let (c, parent, made) = if a.has("--squash") {
                    let (c, parent) = gitbridge::make_commit(repo, &state, a.get("-m"))?;
                    (c, parent, Vec::new())
                } else {
                    let (c, made) = gitbridge::export_history(repo, &state_id)?;
                    (c, None, made)
                };
                let commits: serde_json::Map<String, serde_json::Value> =
                    made.into_iter().map(|(s, c)| (s, serde_json::Value::String(c))).collect();
                let eid = ids::new_id();
                let _lock = repo.p.lock()?;
                let mut rec = Record::new("export");
                rec.data = serde_json::json!({"export": eid, "state": state_id, "remote": remote, "branch": branch, "commit": c, "parent": parent, "squash": a.has("--squash"), "commits": commits, "status": "prepared"});
                repo.commit_record(ctx, rec)?;
                (eid, c)
            }
        };
        if a.has("-n") {
            outln!(ctx, "Would push {} (state {}) to {url} {branch}", &commit[..10], ids::short(&state_id));
            return Ok(0);
        }
        let before = gitbridge::ls_remote(ctx, repo, &url, &branch).ok().flatten();
        gitbridge::push(ctx, repo, &url, &commit, &branch, a.has("-f") || a.has("--force-with-lease"))?;
        {
            let _lock = repo.p.lock()?;
            let mut rec = Record::new("export");
            rec.data = serde_json::json!({"export": export_id, "state": state_id, "remote": remote, "branch": branch, "commit": commit, "status": "pushed"});
            repo.commit_record(ctx, rec)?;
        }
        let _ = gitbridge::git_out(&gitbridge::store(repo), &["update-ref", &format!("refs/remotes/{remote}/{branch}"), &commit]);
        if let Some(l) = &line {
            if let Err(e) = refresh_line_git(repo, l) {
                eprintln!("warning: .git folder not updated: {e:#}");
            }
        }
        if !a.has("-q") {
            outln!(ctx, "To {url}");
            match before {
                None => outln!(ctx, " * [new branch]      {} -> {branch}", &commit[..7]),
                Some(b) => outln!(ctx, "   {}..{}  {} -> {branch}", &b[..7], &commit[..7], ids::short(&state_id)),
            }
            outln!(ctx, "state {} = commit {commit}", ids::short(&state_id));
        }
        Ok(0)
    }

    pub fn clone(ctx: &Ctx, args: &[String]) -> Result<i32> {
        let spec = Spec::new().value("-b|--branch").value("--owner").value("--volume").flag("-q|--quiet").value("--depth");
        let a = parse(&spec, args)?;
        let url = a.pos.first().ok_or_else(|| exit(128, "usage: layr git clone <url> [<project>]"))?.clone();
        let name = match a.pos.get(1) {
            Some(n) => n.clone(),
            None => url.trim_end_matches('/').rsplit('/').next().unwrap_or("project").trim_end_matches(".git").to_string(),
        };
        let mut v = vec![name, "--git".into(), url];
        if let Some(b) = a.get("-b") {
            v.push("--branch".into());
            v.push(b.into());
        }
        if let Some(o) = a.get("--owner") {
            v.push("--owner".into());
            v.push(o.into());
        }
        if let Some(volume) = a.get("--volume") {
            v.push("--volume".into());
            v.push(volume.into());
        }
        if a.has("-q") {
            v.push("-q".into());
        }
        crate::cmd::admin::init(ctx, &v)
    }

    pub fn run(ctx: &Ctx, args: &[String]) -> Result<i32> {
        let (cmd, rest) = args.split_first().ok_or_else(|| exit(128, "usage: layr git <remote|fetch|pull|push|clone|sync>"))?;
        match cmd.as_str() {
            "remote" => remote(ctx, rest),
            "fetch" => fetch(ctx, rest),
            "pull" => pull(ctx, rest),
            "push" => push(ctx, rest),
            "clone" => clone(ctx, rest),
            "sync" => sync(ctx, rest),
            _ => Err(exit(128, "usage: layr git <remote|fetch|pull|push|clone|sync>")),
        }
    }
}

use super::admin::set_config;
use super::{exit, here, mergecmd, replicate};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::oci;
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use serde_json::json;

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Peer { host: String, project: String },
    Store,
    Registry,
}

pub fn kind(url: &str) -> Result<Kind> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| exit(128, "use layr git remote for Git URLs"))?;
    if rest.is_empty() || url.chars().any(char::is_control) {
        bail!("invalid remote URL");
    }
    match scheme {
        "ssh" => {
            let (host, project) = rest.split_once('/').ok_or_else(|| exit(128, "use ssh://<host>/<project>"))?;
            if host.is_empty()
                || host.starts_with('-')
                || host.chars().any(|c| !c.is_ascii_alphanumeric() && !".@_-:[]".contains(c))
            {
                bail!("invalid peer host");
            }
            crate::store::check_name("project", project)?;
            Ok(Kind::Peer { host: host.into(), project: project.into() })
        }
        "file" if rest.starts_with('/') => Ok(Kind::Store),
        "s3" if !rest.starts_with('/') && !rest.contains(['?', '#', '@', ':']) => Ok(Kind::Store),
        "layr+https" => {
            crate::store_remote::validate_gateway_url(url)?;
            Ok(Kind::Store)
        }
        "oci" if !rest.starts_with('/') && rest.contains('/') => Ok(Kind::Registry),
        _ => bail!("unsupported remote URL; use ssh://, file:///, s3://, layr+https:// or oci://"),
    }
}

pub fn remote_url(repo: &Repo, name: &str) -> Result<String> {
    if let Some(url) = repo.p.config_value(&format!("cloud.remote.{name}.url"))?.and_then(|v| v.as_str().map(str::to_owned)) {
        return Ok(url);
    }
    if repo.p.config_value(&format!("remote.{name}.url"))?.is_some() {
        return Err(exit(128, format!("{name} is a Git remote; use layr git <fetch|pull|push> {name}")));
    }
    Err(exit(128, format!("remote '{name}' is not configured; use layr remote add {name} <url>")))
}

pub fn remote(ctx: &Ctx, args: &[String]) -> Result<i32> {
    if args.iter().any(|s| matches!(s.as_str(), "-v" | "--verbose")) {
        return Err(exit(128, "use layr git remote -v"));
    }
    let a = parse(
        &Spec::new()
            .flag("--json")
            .value("--public-key")
            .value("--mode")
            .value("--paper-key-out")
            .flag("--paper-key-tty")
            .value("--recipient")
            .value("--person")
            .value("--invite-out")
            .value("--remove")
            .value("--reason")
            .value("--expires")
            .value("--role")
            .value("--overlays")
            .value("--name"),
        args,
    )?;
    let sub = a.pos.first().map(String::as_str).unwrap_or("list");
    if a.has("--paper-key-tty") && sub != "add" {
        bail!("--paper-key-tty requires remote add");
    }
    if sub == "self" {
        if a.pos.len() != 1 {
            bail!("usage: layr remote self [--json]");
        }
        let machine = &ctx.store.machine;
        let public_key = hex::encode(machine.key.verifying_key().to_bytes());
        if a.has("--json") {
            outln!(ctx, "{}", json!({"id": machine.id, "public_key": public_key, "name": machine.name}));
        } else {
            outln!(ctx, "id {}\npublic_key {public_key}\nname {}", machine.id, machine.name);
        }
        return Ok(0);
    }
    if matches!(sub, "-v" | "--verbose" | "get-url" | "set-url" | "rm") {
        return Err(exit(128, format!("use layr git remote {sub}")));
    }
    let h = here(ctx)?;
    let repo = &h.repo;
    match sub {
        "login" if a.pos.len() == 2 => {
            let name = &a.pos[1];
            let url = remote_url(repo, name)?;
            if !url.starts_with("s3://") { bail!("remote login requires an s3:// remote"); }
            let profile = ctx.env("AWS_PROFILE").map(str::to_owned)
                .or(repo.p.config_value(&format!("remote.{name}.aws-profile"))?.and_then(|v| v.as_str().map(str::to_owned)))
                .unwrap_or_else(|| "default".into());
            let mut env = ctx.env.clone();
            env.entry("HOME".into()).or_insert_with(|| ctx.caller.home.to_string_lossy().into_owned());
            crate::privs::as_caller_fs(ctx, || {
                let profile = layr_store::aws::profile::Profile::load_with_env(&profile, &env)?;
                let http=reqwest::blocking::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
                profile.login(&http, |url, code| {
                    // Device authorization must be visible before the command waits.
                    eprintln!("Open: {url}\nCode: {code}");
                })
            })?;
            outln!(ctx, "AWS login complete");
        }
        "list" if a.pos.len() <= 1 => {
            let mut remotes = Vec::new();
            for (k, _) in repo.p.db.config_all()? {
                if let Some(n) = k.strip_prefix("cloud.remote.").and_then(|s| s.strip_suffix(".url")) {
                    if let Some(url) = repo.p.config_value(&k)?.and_then(|v| v.as_str().map(str::to_owned)) {
                        remotes.push(json!({"name": n, "url": url}));
                    }
                }
            }
            let mut machines = Vec::new();
            for (id, name) in repo.p.db.machines()? {
                let key = repo.p.db.machine_key(&id)?.unwrap_or_default();
                let current = id == repo.p.store.machine.id;
                let trusted = current || repo.p.db.trusted(&id)?.as_deref() == Some(key.as_str());
                machines.push(json!({"id": id, "name": name, "key": key, "current": current, "trusted": trusted}));
            }
            if a.has("--json") {
                outln!(ctx, "{}", json!({"remotes": remotes, "machines": machines, "copies":repo.read_model()?.copies()}));
            } else {
                for r in remotes {
                    outln!(ctx, "remote\t{}\t{}", r["name"].as_str().unwrap(), r["url"].as_str().unwrap());
                }
                for m in machines {
                    let id = m["id"].as_str().unwrap();
                    outln!(
                        ctx,
                        "{}{} {:<16} {:<9} id {} key {}",
                        if m["current"] == true { "* " } else { "  " },
                        &crate::ids::display(id)[..8],
                        m["name"].as_str().unwrap(),
                        if m["trusted"] == true { "trusted" } else { "untrusted" },
                        id,
                        m["key"].as_str().unwrap()
                    );
                }
            }
        }
        "add" if a.pos.len() == 3 => {
            let (name, url) = (&a.pos[1], &a.pos[2]);
            let remote_kind = kind(url)?;
            if a.has("--paper-key-out") && remote_kind != Kind::Store {
                bail!("--paper-key-out requires a store remote");
            }
            if a.has("--paper-key-tty") && remote_kind != Kind::Store {
                bail!("--paper-key-tty requires a store remote");
            }
            crate::store::check_name("remote", name)?;
            repo.require(ctx, "remote")?;
            let _lock = repo.p.lock()?;
            let key = format!("cloud.remote.{name}.url");
            if repo.p.config_value(&key)?.is_some() {
                return Err(exit(3, format!("remote {name} already exists")));
            }
            let recovery_key_file = if remote_kind == Kind::Store {
                let mut store = store_remote::open(url, repo)?;
                store.use_aws_profile(repo.p.config_value(&format!("remote.{name}.aws-profile"))?.and_then(|v|v.as_str().map(str::to_owned)));
                let mode = a.get("--mode").map(|m| layr_store::format::Mode::parse(m)).transpose()?;
                store.require_mode(mode);
                let opts = store_remote::ConfigureOptions {
                    paper_key_out: a.get("--paper-key-out").map(|p| ctx.cwd.join(p)),
                    paper_key_tty: a.has("--paper-key-tty"),
                    // A new store is Private unless it asks for a paper key, which only a Secret store has.
                    mode: mode.unwrap_or(if a.has("--paper-key-out") || a.has("--paper-key-tty") {
                        layr_store::format::Mode::Secret
                    } else {
                        layr_store::format::Mode::Private
                    }),
                };
                let configured = store.configure(ctx, &opts)?;
                store.save_pin(ctx, repo, name)?;
                set_config(ctx, repo, &format!("cloud.remote.{name}.mode"), json!(store.store_mode()?.name()))?;
                configured.recovery_key_file
            } else {
                if a.has("--mode") {
                    bail!("--mode requires a store remote");
                }
                None
            };
            set_config(ctx, repo, &key, json!(url))?;
            if a.has("--json") {
                outln!(ctx, "{}", json!({"name": name, "url": url, "recovery_key_file": recovery_key_file}));
            } else if let Some(path) = recovery_key_file {
                outln!(ctx, "paper key file: {path}");
            }
        }
        "remove" if a.pos.len() == 2 => {
            repo.require(ctx, "remote")?;
            let _lock = repo.p.lock()?;
            remote_url(repo, &a.pos[1])?;
            set_config(ctx, repo, &format!("cloud.remote.{}.url", a.pos[1]), serde_json::Value::Null)?;
            set_config(ctx, repo, &format!("cloud.remote.{}.store", a.pos[1]), serde_json::Value::Null)?;
            set_config(ctx, repo, &format!("cloud.remote.{}.mode", a.pos[1]), serde_json::Value::Null)?;
            if a.has("--json") {
                outln!(ctx, "{}", json!({"removed": a.pos[1]}));
            }
        }
        "public" if a.pos.len() == 3 => {
            let (name, mode) = (&a.pos[1], a.pos[2].as_str());
            repo.require(ctx, "remote")?;
            if kind(&remote_url(repo, name)?)? != Kind::Store {
                bail!("remote public requires a store remote");
            }
            if mode == "off" {
                let mut store = store_remote::open_named(name, repo)?;
                let change = store.change_mode(ctx,repo,name,layr_store::epochs::Mode::Private,&[],Some("public off"),false)?;
                if a.has("--json") { outln!(ctx,"{}",json!({"name":name,"public":false,"mode":"private","tightened":change.tightened})); }
                else { outln!(ctx,"project {} is private; earlier epochs remain readable to anyone who already had access",repo.p.name); }
                return Ok(0);
            }
            if mode != "on" { bail!("usage: layr remote public <remote> on|off"); }
            if repo.p.is_overlay() { bail!("overlays cannot be made public"); }
            eprintln!("All current and future project data will be publicly readable; overlays keep their own readers.");
            eprintln!("Type the project name '{}' to confirm:", repo.p.name);
            use std::io::{BufRead, Read};
            let mut confirmation = String::new();
            std::io::stdin().lock().take(4097).read_line(&mut confirmation)?;
            if confirmation.len() > 4096 || confirmation.trim_end_matches(['\r', '\n']) != repo.p.name {
                bail!("project name did not match; nothing was changed");
            }
            let mut store = store_remote::open_named(name, repo)?;
            let created = store.make_public(ctx, repo)?;
            store.change_mode(ctx,repo,name,layr_store::epochs::Mode::Public,&[],None,false)?;
            if a.has("--json") { outln!(ctx, "{}", json!({"name":name,"public":true,"created":created})); }
            else { outln!(ctx, "project {} is public; all current and future project data is readable", repo.p.name); }
        }
        "mode" | "rotate" if a.pos.len() == if sub == "mode" {3} else {2} => {
            let name=&a.pos[1]; repo.require(ctx,"remote")?;
            if kind(&remote_url(repo,name)?)? != Kind::Store { bail!("access modes require a store remote"); }
            let force=sub=="rotate";
            let mode=if force { store_remote::epochs::project_mode(repo)? } else { layr_store::epochs::Mode::parse(&a.pos[2])? };
            let mut store=store_remote::open_named(name,repo)?;
            let change=store.change_mode(ctx,repo,name,mode,&a.all("--remove"),a.get("--reason"),force)?;
            let mode=change.mode;
            if a.has("--json") { outln!(ctx,"{}",json!({"name":name,"mode":mode,"rotated":change.rotated,"tightened":change.tightened})); }
            else {
                outln!(ctx,"project {} mode {}",repo.p.name,mode.name());
                for overlay in change.tightened { outln!(ctx,"overlay {overlay}: tightened to {}",mode.name()); }
                if change.rotated { outln!(ctx,"earlier epochs remain readable to anyone who already had access"); }
                else if change.opened { outln!(ctx,"all existing and future project data is readable to the new audience"); }
            }
        }
        "grant" if a.pos.len() == 2 => {
            repo.require(ctx,"remote")?;
            let person = a.get("--person").context("grant requires --person <login>")?;
            let recipient = a.get("--recipient").context("grant requires --recipient <age1…>")?;
            let mut store = store_remote::open_named(&a.pos[1],repo)?;
            let output = a.get("--invite-out").map(|path|ctx.cwd.join(path));
            let created = store.grant(ctx,repo,person,recipient,output.as_deref())?;
            if a.has("--json") {outln!(ctx,"{}",json!({"person":person,"recipient":recipient,"status":if created {"granted"} else {"already granted"}}));}
            else {outln!(ctx,"{} store to {person}",if created {"granted"} else {"already granted"});}
        }
        "trust-store" if a.pos.len() == 2 => {
            repo.require(ctx, "remote")?;
            let name = &a.pos[1];
            let recipient = a.get("--recipient").context("trust-store requires --recipient <age1…>")?;
            let url = remote_url(repo, name)?;
            if kind(&url)? != Kind::Store { bail!("trust-store requires a store remote"); }
            let _lock = repo.p.lock()?;
            let mut store = store_remote::open_named(&a.pos[1], repo)?;
            store.trust_store(ctx, repo, name, recipient)?;
            if a.has("--json") { outln!(ctx, "{}", json!({"name":name,"recipient":recipient})); }
            else { outln!(ctx, "trusted store recipient {recipient}"); }
        }
        "trust" if a.pos.len() == 3 => {
            repo.require(ctx, "remote")?;
            let url = remote_url(repo, &a.pos[1])?;
            let id = &a.pos[2];
            let full = if crate::model::is_uuid(id) {
                id.clone()
            } else {
                repo.p.db.machine_by_name(id)?.ok_or_else(|| exit(128, format!("unknown machine {id}")))?
            };
            let remote_kind = kind(&url)?;
            if remote_kind == Kind::Registry { bail!("remote trust is not available for an OCI registry"); }
            let key = a.get("--public-key").map(str::to_owned).or(repo.p.db.machine_key(&full)?)
                .ok_or_else(|| exit(128, "unknown machine key; give --public-key <ed25519>"))?;
            let bytes = hex::decode(&key)?;
            let raw: [u8; 32] = bytes.try_into().map_err(|_| exit(128, "invalid machine key"))?;
            ed25519_dalek::VerifyingKey::from_bytes(&raw).map_err(|_| exit(128, "invalid machine key"))?;
            let key = hex::encode(raw);
            let limits = trust_limits(repo, &a, &full)?;
            match remote_kind {
                Kind::Store => {
                    let mut store = store_remote::open_named(&a.pos[1], repo)?;
                    store.configure(ctx, &store_remote::ConfigureOptions::default())?;
                    {
                        let _lock = repo.p.lock()?;
                        store.save_pin(ctx, repo, &a.pos[1])?;
                        replicate::trust(ctx, &repo.p, &full, &key, limits.as_ref())?;
                    }
                    store.trust(repo, &full, Some(&key))?;
                }
                Kind::Peer { .. } => {
                    let _lock = repo.p.lock()?;
                    replicate::trust(ctx, &repo.p, &full, &key, limits.as_ref())?;
                }
                Kind::Registry => unreachable!(),
            }
            if a.has("--json") {
                let mut out = json!({"trusted": full});
                if let Some(serde_json::Value::Object(limits)) = &limits {
                    for (k, v) in limits {
                        out[k] = v.clone();
                    }
                }
                outln!(ctx, "{out}");
            } else {
                outln!(ctx, "trusted machine {}", crate::ids::short(&full));
            }
        }
        "untrust" if a.pos.len() == 3 => {
            // A suspected leak (docs/ci-trust.md): the machine's later records count nowhere, and every key it
            // could read goes to a new epoch now.
            repo.require(ctx, "remote")?;
            let name = &a.pos[1];
            if kind(&remote_url(repo, name)?)? != Kind::Store {
                bail!("remote untrust requires a store remote");
            }
            let id = &a.pos[2];
            let full = if crate::model::is_uuid(id) {
                id.clone()
            } else {
                repo.p.db.machine_by_name(id)?.ok_or_else(|| exit(128, format!("unknown machine {id}")))?
            };
            if full == repo.p.store.machine.id {
                bail!("a machine cannot untrust itself");
            }
            if repo.p.db.trusted(&full)?.is_none() {
                return Err(exit(128, format!("machine {} is not trusted", crate::ids::short(&full))));
            }
            {
                let _lock = repo.p.lock()?;
                let mut record = crate::model::Record::new("machine.untrust");
                record.data = json!({"machine": full});
                repo.p.append(record, &ctx.actor())?;
            }
            let mut store = store_remote::open_named(name, repo)?;
            let mode = store_remote::epochs::project_mode(repo)?;
            let change = store.change_mode(ctx, repo, name, mode, &[format!("machine:{full}")], Some("untrust"), true)?;
            if a.has("--json") {
                outln!(ctx, "{}", json!({"untrusted": full, "rotated": change.rotated, "overlays": change.tightened}));
            } else {
                outln!(ctx, "untrusted machine {}; new key epoch", crate::ids::short(&full));
            }
        }
        _ => {
            return Err(exit(
                128,
                "usage: layr remote add <name> <url> [--mode secret|private|public] [--paper-key-tty | --paper-key-out <path>] | list | self | login <name> | remove <name> | trust <name> <machine> [--public-key <ed25519>] [--expires <time|30m>] [--role reader|writer] [--overlays <a,b>] [--name <machine name>] | untrust <name> <machine> | trust-store <name> --recipient <age1…> | grant <name> --person <login> --recipient <age1…> [--invite-out <path>]",
            ))
        }
    }
    Ok(0)
}

pub fn sync(ctx: &Ctx, args: &[String]) -> Result<i32> {
    exchange(ctx, args, "sync")
}
pub fn push(ctx: &Ctx, args: &[String]) -> Result<i32> {
    // Remote names and `-n`, `-q`, `--json` only: without a name, with several, or with one git remote, the push
    // roles (`layerpush::push_all`). Any other form (one layr remote, its options, a revision, `--`) is the push
    // of that remote, with all its options.
    let h = here(ctx)?;
    let layr = |n: &str| h.repo.p.config_value(&format!("cloud.remote.{n}.url")).ok().flatten().is_some();
    let git = |n: &str| !layr(n) && h.repo.p.config_value(&format!("remote.{n}.url")).ok().flatten().is_some();
    let names: Vec<&String> = args.iter().filter(|s| !s.starts_with('-')).collect();
    let plain = args.iter().all(|a| {
        matches!(a.as_str(), "-n" | "--dry-run" | "-q" | "--quiet" | "--json") || (!a.starts_with('-') && (layr(a) || git(a)))
    });
    if plain && (names.len() != 1 || git(names[0])) {
        drop(h);
        return super::layerpush::push_all(ctx, args);
    }
    drop(h);
    exchange(ctx, args, "push")
}
pub fn index(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").flag("-q|--quiet").value("--remote"), args)?;
    if a.pos.first().map(String::as_str) != Some("write") || a.pos.len() > 2 || !a.paths.is_empty() {
        bail!("usage: layr index write [<remote>] [--remote <remote>] [--json] [-q]");
    }
    let h = here(ctx)?;
    let configured;
    let name = match a.pos.get(1).map(String::as_str).or(a.get("--remote")) {
        Some(name) => name,
        None => {
            configured = h
                .repo
                .p
                .db
                .config_all()?
                .into_iter()
                .filter_map(|(key, _)| key.strip_prefix("cloud.remote.").and_then(|s| s.strip_suffix(".url")).map(str::to_string))
                .collect::<Vec<_>>();
            if configured.len() != 1 {
                bail!("name one store remote: layr index write <remote>");
            }
            &configured[0]
        }
    };
    if kind(&remote_url(&h.repo, name)?)? != Kind::Store {
        bail!("index write requires a store remote");
    }
    let mut store = store_remote::open_named(name, &h.repo)?;
    let report = store.write_index(ctx, &h.repo)?;
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&report)?);
    } else if !a.has("-q") {
        outln!(
            ctx,
            "index: {} heads, {} paths, {} graphs; {} paths unavailable",
            report.heads,
            report.paths,
            report.graphs,
            report.unavailable_paths.len()
        );
    }
    Ok(0)
}
pub fn fetch(ctx: &Ctx, args: &[String]) -> Result<i32> {
    exchange(ctx, args, "fetch")
}
pub fn pull(ctx: &Ctx, args: &[String]) -> Result<i32> {
    exchange(ctx, args, "pull")
}

fn exchange(ctx: &Ctx, args: &[String], op: &str) -> Result<i32> {
    // Fail the old transport form before opening a project or starting any process.
    if args.iter().any(|s| s == "--") && !args.iter().any(|s| s == "--command") {
        return Err(exit(128, format!("use layr {op} <remote>; configure it with layr remote add <name> <url>")));
    }
    let a = parse(
        &Spec::new()
            .flag("--index")
            .flag("--auto")
            .flag("--caches")
            .value("--caches-of")
            .flag("--heads")
            .flag("-q|--quiet")
            .flag("--json")
            .flag("--command")
            .value("--runnable")
            .value("--paper-key")
            .value("--expect")
            .flag("--ff-only"),
        args,
    )
    .map_err(|e| exit(128, format!("{e}; Git forms use layr git {op}")))?;
    let name = a.pos.first().ok_or_else(|| exit(128, format!("use layr {op} <remote>; Git remotes use layr git {op}")))?;
    let mut h = here(ctx)?;
    let url = remote_url(&h.repo, name).map_err(|e| exit(128, format!("{e}; Git forms use layr git {op}")))?;
    let remote_kind = kind(&url)?;
    if a.has("--index") && (op != "push" || remote_kind != Kind::Store) {
        bail!("--index requires layr push to a store remote");
    }
    if a.has("--runnable") && remote_kind != Kind::Registry {
        bail!("--runnable requires an OCI remote");
    }
    if a.has("--ff-only") && op != "pull" {
        bail!("--ff-only requires layr pull");
    }
    if a.pos.len() > if op == "push" && remote_kind == Kind::Registry { 2 } else { 1 } {
        return Err(exit(128, format!("use layr {op} <remote>; Git forms use layr git {op}")));
    }
    let public_fetch = op == "fetch"
        && remote_kind == Kind::Store
        && h.repo.p.config_value(&format!("cloud.remote.{name}.public"))? == Some(json!(true));
    h.repo.require(
        ctx,
        if op == "push" {
            "push"
        } else if public_fetch {
            "read"
        } else {
            "sync"
        },
    )?;
    match remote_kind {
        Kind::Peer { host, project } => {
            // Peer exchange transfers signed records as well as states.
            h.repo.require(ctx, "sync")?;
            h.repo.require(ctx, "records")?;
            let command = if a.has("--command") {
                if a.paths.is_empty() {
                    bail!("--command needs -- <command> [<args>]");
                }
                a.paths.clone()
            } else {
                // ssh passes these arguments to a remote shell. Project names need quoting too.
                vec!["ssh".into(), "--".into(), host, format!("layr serve-peer '{}'", project.replace('\'', "'\\''"))]
            };
            let transfer = replicate::exchange(
                ctx,
                &command,
                if op == "pull" { "fetch" } else { op },
                a.has("--auto"),
                a.has("--caches"),
                a.has("-q") || (op == "pull" && a.has("--json")),
                a.has("--json") && op != "pull",
            )?;
            if matches!(op, "push" | "sync") {
                for (overlay, data) in &transfer.overlays {
                    h.repo.overlay_repo(overlay)?.record_transfer(ctx, op, name, &url, data)?;
                }
                h.repo.record_transfer(ctx, op, name, &url, &transfer.transfer)?;
            }
        }
        Kind::Store => {
            if !public_fetch {
                h.repo.require(ctx, "sync")?;
                h.repo.require(ctx, "records")?;
            }
            if a.has("--command") {
                bail!("--command is available only for peer remotes");
            }
            if op == "sync" {
                bail!("store remotes use layr push or layr fetch");
            }
            let opts = store_remote::Options {
                auto: a.has("--auto"),
                caches: a.has("--caches"),
                paper_key: a.get("--paper-key").map(str::to_owned),
                public: false,
                caches_of: caches_of(&a),
                only_caches: None,
                heads: a.has("--heads"),
            };
            let mut store = store_remote::open_named(&a.pos[0], &h.repo)?;
            store.configure(ctx, &store_remote::ConfigureOptions::default())?;
            {
                let _lock = h.repo.p.lock()?;
                store.save_pin(ctx, &h.repo, &a.pos[0])?;
            }
            let report =
                if op == "push" { store.push(ctx, &mut h.repo, &opts)? } else { store.fetch(ctx, &mut h.repo, &opts)? };
            store_remote::backend::trace_requests();
            if a.has("--index") {
                store.write_index(ctx, &h.repo)?;
            }
            if !report.missing_lines.is_empty() {
                print_store_report(ctx, &report, op, a.has("--json"), a.has("-q"))?;
                return Ok(2);
            }
            // A transfer record names the states that the remote holds now (their `store` owners). A push that sent
            // no state writes none: it would be the only news of the next push, and every idle push would make one.
            if op == "push" {
                for (overlay, data) in report.overlay_transfers.iter().filter(|(_, data)| data.states_sent > 0) {
                    h.repo.overlay_repo(overlay)?.record_transfer(ctx, op, name, &url, data)?;
                }
            }
            if op == "push" && !report.state_ids.is_empty() {
                h.repo.record_transfer(
                    ctx,
                    op,
                    name,
                    &url,
                    &crate::model::Transfer {
                        direction: "push".into(),
                        records_sent: report.records
                            - report.overlay_transfers.iter().map(|(_, t)| t.records_sent).sum::<usize>(),
                        states_sent: report.state_ids.len(),
                        bytes_sent: report.bytes - report.overlay_transfers.iter().map(|(_, t)| t.bytes_sent).sum::<u64>(),
                        state_ids_sent: report.state_ids.clone(),
                        overlays: report
                            .overlay_transfers
                            .iter()
                            .filter(|(name, t)| {
                                (t.records_sent > 0 || t.states_sent > 0)
                                    && h.repo.overlay_info(name).ok().flatten().is_some_and(|info| !info.hidden)
                            })
                            .map(|(name, _)| name.clone())
                            .collect(),
                        ..Default::default()
                    },
                )?;
            }
            if op != "pull" || !a.has("--json") {
                print_store_report(ctx, &report, op, a.has("--json"), a.has("-q"))?;
            }
        }
        Kind::Registry => {
            if op != "push" {
                bail!("OCI remotes support only layr push <remote> <rev> --runnable <name>");
            }
            let rev = a.pos.get(1).map(String::as_str).unwrap_or("head");
            return if a.has("--json") {
                oci::push_json(ctx, &url, rev, a.get("--runnable"))
            } else {
                oci::push(ctx, &url, rev, a.get("--runnable"))
            };
        }
    }
    if op != "pull" {
        return Ok(0);
    }
    // Reopen after fetch so localization uses the freshly received records.
    let h = here(ctx)?;
    let line = h.line()?;
    let mut heads = h
        .repo
        .p
        .db
        .conn
        .prepare("SELECT head FROM line_heads WHERE line=?1 AND machine<>?2 ORDER BY time DESC, machine DESC LIMIT 1")?;
    let target: Option<String> =
        heads.query_map(rusqlite::params![line.id, h.repo.p.store.machine.id], |r| r.get(0))?.next().transpose()?;
    let target = target.ok_or_else(|| exit(1, "no head of the current line from another machine"))?;
    let flags = parse(&Spec::new().flag("--ff-only").value("--expect").flag("-q"), &args_for_merge(&a))?;
    if !a.has("--json") {
        return mergecmd::merge_rev(ctx, &h.repo, line, &target, &flags);
    }
    let saved = ctx.out.replace(Box::new(std::io::sink()));
    let result = mergecmd::merge_rev(ctx, &h.repo, line, &target, &flags);
    ctx.out.replace(saved);
    let code = result?;
    let head = h.repo.line(&line.id)?.head;
    let conflicts = h.repo.state(&head)?.conflicts.len();
    outln!(ctx, "{}", json!({"operation": "pull", "target": target, "head": head, "conflicts": conflicts, "exit": code}));
    Ok(code)
}

/// The lines of `--caches-of <line>[,<line>]`.
fn caches_of(a: &crate::args::Parsed) -> Vec<String> {
    a.get("--caches-of").map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_owned).collect()).unwrap_or_default()
}

fn print_store_report(ctx: &Ctx, report: &store_remote::Report, op: &str, json_output: bool, quiet: bool) -> Result<()> {
    if json_output {
        outln!(ctx, "{}", serde_json::to_string(report)?);
    } else if !quiet {
        outln!(ctx, "{op}: {} records, {} states, {} bytes", report.records, report.states, report.bytes);
        for line in &report.overlays {
            outln!(ctx, "  overlay {line}");
        }
        for item in omitted_summaries(&report.omitted) {
            outln!(ctx, "omitted: {item}");
        }
        for gap in &report.gaps {
            outln!(ctx, "record gap: {gap}");
        }
        if report.overlaps > 0 {
            outln!(ctx, "overlapping record segments: {}", report.overlaps);
        }
    }
    if !json_output {
        for line in &report.missing_lines {
            outln!(ctx, "{line}");
        }
    }
    Ok(())
}

fn omitted_summaries(omitted: &[String]) -> Vec<String> {
    let mut summaries = Vec::new();
    let mut states: std::collections::BTreeMap<&str, std::collections::BTreeSet<&str>> = Default::default();
    let mut other = std::collections::BTreeSet::new();
    for item in omitted {
        let fields: Vec<_> = item.split_whitespace().collect();
        if fields.len() == 2 && crate::model::is_uuid(fields[0]) {
            states.entry(fields[1].trim_matches('"')).or_default().insert(fields[0]);
        } else if fields.len() >= 3 && fields[0] == "cache" && fields[1] == "state" && crate::model::is_uuid(fields[2]) {
            states.entry("cache").or_default().insert(fields[2]);
        } else {
            other.insert(item);
        }
    }
    for (kind, ids) in states {
        let flag = match kind {
            "cache" => "--caches",
            "auto" => "--auto",
            _ => "",
        };
        let hint = if flag.is_empty() { String::new() } else { format!(" (use {flag})") };
        let noun = if ids.len() == 1 { "state" } else { "states" };
        summaries.push(format!("{} {kind} {noun}{hint}", ids.len()));
    }
    for item in other {
        summaries.push(item.clone());
    }
    summaries
}

fn args_for_merge(a: &crate::args::Parsed) -> Vec<String> {
    let mut args = Vec::new();
    if a.has("--ff-only") {
        args.push("--ff-only".into());
    }
    if a.has("-q") {
        args.push("-q".into());
    }
    if let Some(expect) = a.get("--expect") {
        args.extend(["--expect".into(), expect.into()]);
    }
    args
}

pub fn clone(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(
        &Spec::new()
            .value("--mode")
            .value("--paper-key")
            .value("--volume")
            .flag("--public")
            .flag("--auto")
            .flag("--caches")
            .value("--caches-of")
            .flag("--heads")
            .flag("--json")
            .flag("-q|--quiet"),
        args,
    )
    .map_err(|e| exit(128, format!("{e}; Git forms use layr git clone")))?;
    let url = a.pos.first().ok_or_else(|| exit(128, "usage: layr clone <url> [<project>]; Git uses layr git clone"))?;
    if kind(url).map_err(|e| exit(128, format!("{e}; use layr git clone for Git URLs")))? != Kind::Store {
        bail!("clone requires a file://, s3:// or layr+https:// store; Git uses layr git clone");
    }
    if a.pos.len() > 2 {
        bail!("usage: layr clone <url> [<project>] [--mode secret|private|public]");
    }
    let name = a.pos.get(1).cloned().unwrap_or_else(|| url.trim_end_matches('/').rsplit('/').next().unwrap_or("project").into());
    // Remove a new project if store open or clone fails.
    let p = crate::store::Project::create_empty_on(&ctx.store, &name, a.get("--volume"))?;
    let mut repo = Repo { p, ctx: Some(ctx) };
    let result = (|| -> Result<i32> {
        let mut store = store_remote::open(url, &repo)?;
        store.use_public_access(a.has("--public"));
        // `--mode secret` refuses a server that serves this project as plain, and the reverse.
        store.require_mode(a.get("--mode").map(layr_store::format::Mode::parse).transpose()?);
        store.configure(ctx, &store_remote::ConfigureOptions::default())?;
        let opts = store_remote::Options {
            public: a.has("--public"),
            auto: a.has("--auto"),
            caches: a.has("--caches"),
            paper_key: a.get("--paper-key").map(str::to_owned),
            caches_of: caches_of(&a),
            only_caches: None,
            heads: a.has("--heads"),
        };
        let report = store.clone_into(ctx, &mut repo, &opts)?;
        store_remote::backend::trace_requests();
        let _lock = repo.p.lock()?;
        set_config(ctx, &repo, "cloud.remote.origin.url", json!(url))?;
        store.save_pin(ctx, &repo, "origin")?;
        set_config(ctx, &repo, "cloud.remote.origin.mode", json!(store.store_mode()?.name()))?;
        if store.is_public_reader() {
            set_config(ctx, &repo, "cloud.remote.origin.public", json!(true))?;
        }
        // This new copy belongs to the cloning OS user. Its local policy never
        // selects automatic check commands, which use the verified creator.
        let mut access = repo.policy().access_json();
        access[format!("user:{}", ctx.caller.uid)] = json!("admin");
        set_config(ctx, &repo, "access", access)?;
        print_store_report(ctx, &report, "clone", a.has("--json"), a.has("-q"))?;
        Ok(if report.missing_lines.is_empty() { 0 } else { 2 })
    })();
    if result.is_err() {
        let dir = repo.p.dir.clone();
        drop(repo);
        let _ = crate::storage::delete_tree(&dir);
    }
    result
}

use crate::store_remote;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn omitted_state_summary_counts_each_state_once() {
        let auto = "01a12290-0000-7000-8000-000000000001";
        let cache = "01a12290-0000-7000-8000-000000000002";
        let other = "01a12290-0000-7000-8000-000000000003";
        let omitted = vec![
            format!("{auto} \"auto\""),
            format!("{cache} \"cache\""),
            format!("{other} \"cache\""),
            format!("cache state {cache} of ref caches/one: use --caches"),
            format!("cache state {cache} of ref caches/two: use --caches"),
            "private overlay files".into(),
        ];
        assert_eq!(
            omitted_summaries(&omitted),
            vec!["1 auto state (use --auto)", "2 cache states (use --caches)", "private overlay files"]
        );
    }
    #[test]
    fn remote_url_dispatch() {
        assert_eq!(kind("ssh://user@host/project").unwrap(), Kind::Peer { host: "user@host".into(), project: "project".into() });
        assert_eq!(kind("file:///tmp/store").unwrap(), Kind::Store);
        assert_eq!(kind("s3://bucket/path").unwrap(), Kind::Store);
        assert_eq!(kind("layr+https://gateway.example/project").unwrap(), Kind::Store);
        assert_eq!(kind("oci://host/project").unwrap(), Kind::Registry);
        for url in [
            "/tmp/repo.git",
            "https://host/repo",
            "ssh://-oBad/p",
            "ssh://host/../p",
            "ssh://host/p;id",
            "ssh://host/",
            "ssh://host",
            "file://relative",
            "s3:///bucket",
            "s3://access:secret@bucket/p",
            "s3://bucket/p?AWS_SECRET_ACCESS_KEY=x",
            "oci://host",
        ] {
            assert!(kind(url).is_err(), "{url}");
        }
        assert!(!kind("s3://access:secret@bucket/p").unwrap_err().to_string().contains("secret"));
    }
}

/// The limits of `remote trust` (docs/ci-trust.md): `--expires <RFC 3339 time or duration>`, `--role reader|writer`
/// (reader by default) and `--overlays <name>,<name>`. None without any of them: a full trust without an end.
fn trust_limits(repo: &crate::repo::Repo, a: &crate::args::Parsed, machine: &str) -> Result<Option<serde_json::Value>> {
    if !["--expires", "--role", "--overlays", "--name"].iter().any(|f| a.get(f).is_some()) {
        if repo.p.trust_terms(&[])?.brokers.contains_key(&repo.p.store.machine.id) {
            return Err(exit(128, "this machine is a broker: it trusts only with --expires inside its delegation"));
        }
        return Ok(None);
    }
    let mut limits = json!({});
    if let Some(text) = a.get("--expires") {
        let now = crate::ids::now_ms();
        let at = match chrono::DateTime::parse_from_rfc3339(text) {
            Ok(time) => time.timestamp_millis(),
            Err(_) => now + super::admin::duration_ms(text)?,
        };
        if at <= now {
            return Err(exit(128, format!("--expires {text} is not in the future")));
        }
        limits["expires_at"] = json!(at);
    }
    let role = a.get("--role").unwrap_or("reader");
    if layr_store::trust::Role::parse(role).is_none() {
        return Err(exit(128, format!("--role {role}: use reader or writer")));
    }
    limits["role"] = json!(role);
    if let Some(list) = a.get("--overlays") {
        let names: Vec<&str> = list.split(',').map(str::trim).filter(|n| !n.is_empty()).collect();
        for name in &names {
            if repo.overlay_info(name)?.is_none() {
                return Err(exit(128, format!("unknown overlay {name}")));
            }
        }
        limits["overlays"] = json!(names);
    }
    // The machine's name, for a delegation with a name prefix: given (a new machine sends `remote self --json`), or
    // from its records.
    let name = match a.get("--name") {
        Some(name) => Some(name.to_string()),
        None => repo.p.db.machine_name(machine)?,
    };
    if let Some(name) = name {
        limits["name"] = json!(name);
    }
    // A broker (docs/ci-trust.md) trusts only inside its delegation: a trust outside it would count nowhere.
    let rules = repo.p.trust_terms(&[])?;
    let mut data = limits.clone();
    data["machine"] = json!(machine);
    if !rules.trust_counts(&repo.p.store.machine.id, crate::ids::now_ms(), &data) {
        return Err(exit(128, "this broker's delegation does not cover the trust (end, role, overlays or name prefix)"));
    }
    Ok(Some(limits))
}
