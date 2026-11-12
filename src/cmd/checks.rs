//! Automatic checks use the same immutable inputs, argv and attestations as try and queue.

use super::{exit, here};
use crate::args::{parse, Parsed, Spec};
use crate::ctx::Ctx;
use crate::model::{RefChange, RefState, StateKind, Subject};
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;

const LOCAL_KEY: &str = "checks.privileged-runner";
const RUNNER_KEY: &str = "checks.runner";

#[derive(Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
struct Local {
    #[serde(rename = "checks.runner")]
    runner: bool,
    #[serde(rename = "checks.privileged-runner")]
    privileged: bool,
}

pub fn validate_config(key: &str, value: &Value) -> Result<()> {
    if value.is_null() || !key.starts_with("checks.") {
        return Ok(());
    }
    match key {
        "checks.changes" | "checks.privileged" => {
            names(key, value)?;
        }
        "checks.lines" => {
            for (line, checks) in value.as_object().context("checks.lines must be an object of line names to check arrays")? {
                crate::store::check_name("line", line)?;
                names(key, checks)?;
            }
        }
        "checks.platforms" => {
            for (name, entries) in value.as_object().context("checks.platforms must be an object")? {
                crate::oci::valid_component(name)?;
                let list = match entries {
                    Value::Array(list) if !list.is_empty() => list.iter().collect(),
                    Value::String(_) => vec![entries],
                    _ => bail!("checks.platforms maps a check name to a platform or a list of platforms"),
                };
                for entry in list {
                    if !entry.as_str().is_some_and(|e| PLATFORMS.contains(&e)) {
                        bail!("check platform must be one of {}", PLATFORMS.join(", "));
                    }
                }
            }
        }
        "checks.after" => {
            for (name, needs) in value.as_object().context("checks.after must be an object of check names to check arrays")? {
                crate::oci::valid_component(name)?;
                for need in needs.as_array().context("checks.after maps a check name to an array of checks")? {
                    let need = need.as_str().context("checks.after needs string check names")?;
                    let (check, platform) = split_need(need);
                    crate::oci::valid_component(check)?;
                    if platform.is_some_and(|p| !PLATFORMS.contains(&p)) {
                        bail!("check platform must be one of {}", PLATFORMS.join(", "));
                    }
                }
            }
        }
        "checks.timeout" => {
            number(key, value, 1, 86400)?;
        }
        "checks.memory" => {
            number(key, value, 16 << 20, 1 << 40)?;
        }
        "checks.processes" => {
            number(key, value, 1, 65536)?;
        }
        "checks.log-bytes" => {
            number(key, value, 1, 16 << 20)?;
        }
        LOCAL_KEY | RUNNER_KEY => bail!("{key} is machine-local; use config --local"),
        _ => bail!("unknown automatic check setting '{key}'"),
    }
    Ok(())
}

/// The entries of `checks.platforms`: an OS takes any architecture, `<os>-<arch>` only that one.
const PLATFORMS: [&str; 6] = ["linux", "macos", "linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64"];

/// The platform of this machine in a check attestation: `<os>-<arch>`, as `macos-aarch64`.
pub fn platform() -> String {
    // The same test hook as cache platforms (`LAYR_TEST_PLATFORM`): a check result of another platform.
    crate::model::cache_platform()
}

/// Whether the platform `have` (`<os>-<arch>`) meets the entry `want` of `checks.platforms`.
pub(crate) fn meets(want: &str, have: &str) -> bool {
    have == want || have.strip_prefix(want).is_some_and(|arch| arch.starts_with('-'))
}

/// The platform entries of the check `name`; None: one run on any platform.
pub(crate) fn platforms_of(platforms: &Value, name: &str) -> Vec<Option<String>> {
    match platforms.get(name) {
        Some(Value::String(one)) => vec![Some(one.clone())],
        Some(Value::Array(list)) => list.iter().filter_map(|e| e.as_str().map(|e| Some(e.to_owned()))).collect(),
        _ => vec![None],
    }
}

fn names(key: &str, value: &Value) -> Result<Vec<String>> {
    value
        .as_array()
        .with_context(|| format!("{key} must be an array of check names"))?
        .iter()
        .map(|name| {
            let name = name.as_str().with_context(|| format!("{key} needs string check names"))?;
            crate::oci::valid_component(name)?;
            Ok(name.to_string())
        })
        .collect()
}

fn number(key: &str, value: &Value, min: u64, max: u64) -> Result<u64> {
    match value.as_u64() {
        Some(n) if n >= min && n <= max => Ok(n),
        _ => bail!("{key} must be an integer from {min} to {max}"),
    }
}

fn local_settings(repo: &Repo) -> Result<Local> {
    let path = repo.p.meta_dir().join("checks.local.json");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Local::default()),
        Err(e) => return Err(e.into()),
    };
    let owner = if cfg!(target_os = "macos") { std::fs::metadata(repo.p.meta_dir())?.uid() } else { 0 };
    if !meta.is_file() || meta.uid() != owner || meta.mode() & 0o077 != 0 || meta.len() > 1024 {
        bail!("{} must be a private regular file owned by the machine administrator", path.display());
    }
    serde_json::from_slice(&std::fs::read(path)?).context("invalid local check runner settings")
}

/// Only the machine administrator may enable root execution; this file is never replicated.
pub fn local_config(ctx: &Ctx, repo: &Repo, a: &Parsed) -> Result<i32> {
    let key = a.get("--get").or(a.get("--unset")).or_else(|| a.pos.first().map(String::as_str));
    if !matches!(key, Some(LOCAL_KEY | RUNNER_KEY)) || a.has("-l") {
        bail!("config --local supports only {RUNNER_KEY} and {LOCAL_KEY}");
    }
    if a.has("--get") || (a.pos.len() == 1 && !a.has("--unset")) {
        let settings = local_settings(repo)?;
        outln!(ctx, "{}", if key == Some(RUNNER_KEY) { settings.runner } else { settings.privileged });
        return Ok(0);
    }
    let owner = if cfg!(target_os = "macos") { std::fs::metadata(repo.p.meta_dir())?.uid() } else { 0 };
    if ctx.caller.uid != owner {
        bail!("only the machine administrator may change machine-local check execution");
    }
    let enabled = if a.has("--unset") {
        false
    } else {
        if a.pos.len() != 2 {
            bail!("usage: layr config --local (checks.runner|checks.privileged-runner) (true|false)");
        }
        a.pos[1].parse::<bool>().context("privileged runner must be true or false")?
    };
    if cfg!(target_os = "macos") && key == Some(LOCAL_KEY) && enabled {
        bail!("privileged automatic checks are unavailable on macOS");
    }
    let _lock = repo.p.lock()?;
    let mut settings = local_settings(repo)?;
    if key == Some(RUNNER_KEY) {
        settings.runner = enabled;
    } else {
        settings.privileged = enabled;
    }
    crate::fsutil::atomic_write(&repo.p.meta_dir().join("checks.local.json"), &serde_json::to_vec(&settings)?, 0o600)?;
    Ok(0)
}

const USAGE: &str = "usage: layr checks watch [--once] [--privileged-only] [--interval <seconds>] [--json]\n       layr checks join <gateway url> (--code <join code>|--code-file <path>) --project <a,b> [--name <runner name>] [--no-start] [--json]\n       layr checks run --state <rev> [--required-checks <names> | --planned-only] [--privileged-only] [--json]\n       layr checks pending [--state <rev>] [--platform <os>-<arch>] [--passing] [--required-checks <names>] [--json]\nTo list check results, use layr attestations.";

pub fn checks(ctx: &Ctx, args: &[String]) -> Result<i32> {
    // `join`: make this store a check runner of a Layr Cloud gateway (src/runner.rs).
    if args.first().is_some_and(|a| a == "join") {
        return crate::runner::join(ctx, &args[1..]);
    }
    let a = parse(
        &Spec::new()
            .flag("--once")
            .flag("--json")
            .flag("--privileged-only")
            .flag("--passing")
            .flag("--planned-only")
            .value("--interval")
            .value("--state")
            .value("--platform")
            .value("--required-checks"),
        args,
    )?;
    if a.pos == ["pending"]
        && a.paths.is_empty()
        && !a.has("--once")
        && !a.has("--privileged-only")
        && !a.has("--planned-only")
        && a.get("--interval").is_none()
    {
        return pending(ctx, &a);
    }
    // `run`: one pass over the jobs of one exact state, such as a commit that CI fetched from Git.
    let run = a.pos == ["run"];
    if !(a.pos == ["watch"] || run)
        || !a.paths.is_empty()
        || a.get("--state").is_some() != run
        || a.get("--platform").is_some()
        || a.has("--passing")
        || (a.get("--required-checks").is_some() && !run)
        || (a.has("--planned-only") && (!run || a.get("--required-checks").is_some()))
    {
        return Err(exit(128, USAGE));
    }
    if run && (a.has("--once") || a.get("--interval").is_some()) {
        return Err(exit(128, USAGE));
    }
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        bail!("automatic checks require Linux or macOS");
    }
    if ctx.caller.is_root() {
        bail!("checks {} needs a non-root service user through the layr daemon", a.pos[0]);
    }
    let interval: u64 = a.get("--interval").unwrap_or("5").parse().context("invalid check interval")?;
    if !(1..=3600).contains(&interval) {
        bail!("check interval must be from 1 to 3600 seconds");
    }
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    let state = a.get("--state").map(|rev| crate::revs::resolve(&h.repo, h.line.as_ref(), rev)).transpose()?;
    let required = required_checks(a.get("--required-checks"), state.as_deref(), true)?;
    // Match the queue runner: one watcher per project without holding the project write lock.
    let lock =
        std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(h.repo.p.meta_dir().join("checks.lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .context("a check watcher is already active for this project")?;
    loop {
        let h = here(ctx)?;
        h.repo.require(ctx, "try")?;
        let results = scan(ctx, &h.repo, a.has("--privileged-only"), state.as_deref(), &required, a.has("--planned-only"))?;
        if a.has("--json") {
            outln!(
                ctx,
                "{}",
                json!({"work_done": !results.checks.is_empty(), "count": results.checks.len(),
                "pending": results.pending, "pending_privileged": results.pending_privileged, "checks": results.checks})
            );
        } else {
            for result in &results.checks {
                outln!(
                    ctx,
                    "{}\t{}\texit {}\t{}",
                    result["name"].as_str().unwrap_or(""),
                    result["state"].as_str().unwrap_or(""),
                    result["exit"],
                    result["log_ref"].as_str().unwrap_or("")
                );
            }
        }
        // Flush each cycle: service output otherwise stays buffered throughout a watch.
        use std::io::Write;
        ctx.out.borrow_mut().flush()?;
        if a.has("--once") || run {
            return Ok(if results.checks.iter().any(|r| r["exit"] != 0) { 1 } else { 0 });
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

/// `checks pending`: the check jobs that no active executor attestation covers for the platform
/// `--platform` (default: this machine's). A CI job runs only these; a result from another trusted
/// machine on a matching platform, such as a laptop, counts.
fn pending(ctx: &Ctx, a: &Parsed) -> Result<i32> {
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    let state = a.get("--state").map(|rev| crate::revs::resolve(&h.repo, h.line.as_ref(), rev)).transpose()?;
    let have = a.get("--platform").map_or_else(platform, str::to_owned);
    if !PLATFORMS.iter().any(|p| p.contains('-') && *p == have) {
        bail!(
            "--platform must be one of {}",
            PLATFORMS.iter().filter(|p| p.contains('-')).copied().collect::<Vec<_>>().join(", ")
        );
    }
    let required = required_checks(a.get("--required-checks"), state.as_deref(), a.has("--passing"))?;
    let Plan { policy, jobs, commands, privileged, change_jobs, .. } = plan(&h.repo, state.as_deref(), &required, false)?;
    let mut out = Vec::new();
    for (state, name, want) in jobs {
        if want.as_deref().is_some_and(|want| !meets(want, &have)) {
            continue;
        }
        let argv = layr_model::protection::check_argv(&commands[&name]);
        let is_privileged = privileged.contains(&name);
        if !attested_result(&h.repo, &state, &argv, is_privileged, want.as_deref(), a.has("--passing"))? {
            let change = change_jobs.contains(&(state.clone(), name.clone()));
            out.push(json!({"state":state,"name":name,"command":commands[&name],"argv":argv,"privileged":is_privileged,"platform":want,"change":change}));
        }
    }
    if a.has("--json") {
        let mut result = json!({"platform":have,"pending":out});
        if !required.is_empty() {
            let platforms = setting(&policy, "checks.platforms", json!({}))?;
            result["required_definitions"] = json!(required_definitions(&required, &commands, &platforms));
        }
        outln!(ctx, "{}", result);
    } else {
        for job in &out {
            outln!(ctx, "{}\t{}", job["name"].as_str().unwrap_or(""), job["state"].as_str().unwrap_or(""));
        }
    }
    Ok(0)
}

fn required_definitions(required: &[String], commands: &BTreeMap<String, String>, platforms: &Value) -> BTreeMap<String, Value> {
    required
        .iter()
        .map(|name| (name.clone(), json!({"command": commands[name], "platforms": platforms.get(name.as_str()).cloned()})))
        .collect()
}

fn setting(policy: &layr_model::Project, key: &str, default: Value) -> Result<Value> {
    let value = policy.setting(key).cloned().unwrap_or(default);
    validate_config(key, &value)?;
    Ok(value)
}

struct Scan {
    checks: Vec<Value>,
    pending: usize,
    pending_privileged: usize,
}

/// The check jobs of a project: each state and check name that its policy asks for, once per
/// platform entry of the check (`checks.platforms`; None: any platform).
struct Plan {
    policy: layr_model::Project,
    main_states: BTreeSet<String>,
    /// The heads (also intermediate) of the protected lines in `checks.lines`, with the line name.
    heads: BTreeMap<String, String>,
    jobs: Vec<(String, String, Option<String>)>,
    /// State and check pairs for the newest version of an open change.
    change_jobs: BTreeSet<(String, String)>,
    commands: BTreeMap<String, String>,
    privileged: BTreeSet<String>,
}

/// `state`: also plan the change checks (`checks.changes`) of that one state.
fn required_checks(value: Option<&str>, state: Option<&str>, passing: bool) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if state.is_none() || !passing || value.is_empty() {
        bail!("--required-checks needs --state, --passing and a comma-separated list of check names");
    }
    value
        .split(',')
        .map(|name| {
            crate::oci::valid_component(name)?;
            Ok(name.to_string())
        })
        .collect()
}

fn plan(repo: &Repo, state: Option<&str>, required: &[String], planned_only: bool) -> Result<Plan> {
    let records = repo
        .p
        .db
        .all_lines()?
        .iter()
        .map(|line| crate::oplog::parse_line(line, None).map(|s| s.record))
        .collect::<Result<Vec<_>>>()?;
    let (policy, main_states) = creator_policy(&repo.p.id, records)?;
    let changes = names("checks.changes", &setting(&policy, "checks.changes", json!([]))?)?;
    let lines = setting(&policy, "checks.lines", json!({}))?;
    let platforms = setting(&policy, "checks.platforms", json!({}))?;
    let privileged: BTreeSet<_> =
        names("checks.privileged", &setting(&policy, "checks.privileged", json!([]))?)?.into_iter().collect();
    let mut jobs = BTreeSet::new();
    let mut change_jobs = BTreeSet::new();
    if let Some(state) = state.filter(|_| !planned_only) {
        for name in changes.iter().chain(required) {
            jobs.insert((state.to_owned(), name.clone()));
        }
    }
    // Change checks: the newest version of each open change. A change whose newest version a protected line holds
    // already, or whose line is deleted, is done; an older version is superseded.
    if !changes.is_empty() {
        let merged = protected_history(repo, &policy)?;
        for newest in repo.p.db.newest_change_versions()? {
            if merged.contains(&newest.id)
                || newest.line.as_deref().map(|l| repo.p.db.line_deleted(l)).transpose()?.unwrap_or(false)
            {
                continue;
            }
            for name in &changes {
                let job = (newest.id.clone(), name.clone());
                jobs.insert(job.clone());
                change_jobs.insert(job);
            }
        }
    }
    let mut watched_lines = BTreeMap::new();
    let mut heads = BTreeMap::new();
    for (line, checks) in lines.as_object().context("checks.lines must be an object")? {
        if policy.access_policy(crate::ids::now_ms(), false).rule_for(line).is_none() {
            bail!("automatic line '{line}' must be protected");
        }
        let checks = names("checks.lines", checks)?;
        // A line that the setting names but that is gone (deleted) plans nothing; the other lines still run.
        let Some(line) = repo.p.db.line_by_name(line)? else {
            eprintln!("layr checks: checks.lines names line '{line}', which does not exist; it is skipped");
            continue;
        };
        for (_, head) in repo.p.db.line_heads(&line.id)? {
            for name in &checks {
                jobs.insert((head.clone(), name.clone()));
            }
            heads.insert(head.clone(), line.name.clone());
        }
        watched_lines.insert(line.id, (line.name.clone(), checks));
    }
    // Include intermediate heads when more than one version arrived between scans.
    if !watched_lines.is_empty() {
        for record in repo.p.db.records(usize::MAX >> 1)? {
            for (id, movement) in record.lines {
                if let (Some((line, names)), Some(after)) = (watched_lines.get(&id), movement.after) {
                    for name in names {
                        jobs.insert((after.head.clone(), name.clone()));
                    }
                    heads.insert(after.head.clone(), line.clone());
                }
            }
        }
    }
    // A pending fast-forward entry of the queue into a watched line is that line's next head: a server queue
    // (Layr Cloud) merges an entry only after its checks passed, so they never run again on the head. Its passing
    // check gives the caches back as the head's, and main is warm the moment it moves. A titled entry merges as a
    // new squash state, which these caches would not be saved with.
    for (id, (line, _)) in &watched_lines {
        let line_heads = repo.p.db.line_heads(id)?;
        for entry in repo.p.db.queue_entries(Some(id))? {
            if entry.status != "pending" || entry.title.is_some() || heads.contains_key(&entry.state) {
                continue;
            }
            let mut fast_forward = false;
            for (_, head) in &line_heads {
                if repo.is_ancestor(head, &entry.state)? {
                    fast_forward = true;
                    break;
                }
            }
            if fast_forward {
                heads.insert(entry.state, line.clone());
            }
        }
    }
    if let Some(state) = state {
        jobs.retain(|(s, _)| s == state);
    }
    let mut commands = BTreeMap::new();
    for (_, name) in &jobs {
        if !commands.contains_key(name) {
            let command = policy
                .setting(&format!("check.{name}"))
                .and_then(|v| v.as_str().map(String::from))
                .with_context(|| format!("check '{name}' is not defined (layr config check.{name} '<command>')"))?;
            commands.insert(name.clone(), command);
        }
    }
    // Newest states first (state ids grow with time): a new change is checked before an old one.
    let jobs = order_jobs(jobs, &platforms);
    Ok(Plan { policy, main_states, heads, jobs, change_jobs, commands, privileged })
}

fn order_jobs(jobs: BTreeSet<(String, String)>, platforms: &Value) -> Vec<(String, String, Option<String>)> {
    let mut seen = BTreeSet::new();
    jobs.into_iter()
        .rev()
        .flat_map(|(state, name)| platforms_of(platforms, &name).into_iter().map(move |p| (state.clone(), name.clone(), p)))
        .filter(|job| seen.insert(job.clone()))
        .collect()
}

/// The states in the history of the heads of the protected lines.
fn protected_history(repo: &Repo, policy: &layr_model::Project) -> Result<BTreeSet<String>> {
    let access = policy.access_policy(crate::ids::now_ms(), false);
    let mut seen = BTreeSet::new();
    let mut pending: Vec<String> =
        repo.lines()?.into_iter().filter(|l| access.rule_for(&l.name).is_some()).map(|l| l.head).collect();
    while let Some(id) = pending.pop() {
        if seen.insert(id.clone()) {
            if let Some(state) = repo.p.db.state(&id)? {
                pending.extend(state.parents);
            }
        }
    }
    Ok(seen)
}

/// Whether an active executor check of `state` ran the argv `argv` with the privileged mode
/// `privileged` on a platform that meets `want` (None: any). A trusted machine signed it, so it
/// counts on every machine, the laptop and CI alike.
fn attested(repo: &Repo, state: &str, argv: &[String], privileged: bool, want: Option<&str>) -> Result<bool> {
    attested_result(repo, state, argv, privileged, want, false)
}

fn attested_result(
    repo: &Repo,
    state: &str,
    argv: &[String],
    privileged: bool,
    want: Option<&str>,
    passing: bool,
) -> Result<bool> {
    if repo.p.db.check_exit(state, argv, if passing { Some(0) } else { None })?.is_none() {
        return Ok(false);
    }
    let subject = Subject { kind: "state".into(), id: state.to_owned() };
    Ok(crate::probes::active_checks(repo, &subject)?
        .into_values()
        .any(|(_, r)| matches_check(&r.data["check"], argv, privileged, want, passing)))
}

/// A queue gate uses the creator's privilege policy and requires a passing executor result.
pub(crate) fn passing_required(repo: &Repo, state: &str, name: &str, argv: &[String], want: Option<&str>) -> Result<bool> {
    let records = repo
        .p
        .db
        .all_lines()?
        .iter()
        .map(|line| crate::oplog::parse_line(line, None).map(|s| s.record))
        .collect::<Result<Vec<_>>>()?;
    let (policy, _) = creator_policy(&repo.p.id, records)?;
    let privileged = names("checks.privileged", &setting(&policy, "checks.privileged", json!([]))?)?;
    attested_result(repo, state, argv, privileged.iter().any(|check| check == name), want, true)
}

pub(crate) fn creator_project(repo: &Repo) -> Result<layr_model::Project> {
    let records = repo
        .p
        .db
        .all_lines()?
        .iter()
        .map(|line| crate::oplog::parse_line(line, None).map(|s| s.record))
        .collect::<Result<Vec<_>>>()?;
    let (policy, _) = creator_policy(&repo.p.id, records)?;
    Ok(policy)
}

fn matches_check(check: &Value, argv: &[String], privileged: bool, want: Option<&str>, passing: bool) -> bool {
    check["executor"] == true
        && check["argv"] == json!(argv)
        && check["privileged"] == privileged
        && check["exit"].as_i64().is_some_and(|exit| !passing || exit == 0)
        && want.is_none_or(|want| check["platform"].as_str().is_some_and(|have| meets(want, have)))
}

/// An entry of `checks.after`: `<check>` or `<check>@<platform>`.
fn split_need(need: &str) -> (&str, Option<&str>) {
    match need.split_once('@') {
        Some((check, platform)) => (check, Some(platform)),
        None => (need, None),
    }
}

/// What the checks that a job needs (`checks.after`) say about its state.
#[derive(Debug, PartialEq)]
enum Needs {
    /// Each one passed (on its platform, when the entry names one).
    Ready,
    /// One has no result yet: the job waits, with no attestation.
    Wait(String),
    /// One failed: the job does not run, with no attestation.
    Skip(String),
}

/// The state of the checks that the check `name` needs on `state`. A passing result on any platform counts unless
/// the entry names one.
fn needs(repo: &Repo, state: &str, name: &str, after: &Value, commands: &BTreeMap<String, String>) -> Result<Needs> {
    let Some(list) = after.get(name).and_then(Value::as_array) else {
        return Ok(Needs::Ready);
    };
    let subject = Subject { kind: "state".into(), id: state.to_owned() };
    let results: Vec<Value> =
        crate::probes::active_checks(repo, &subject)?.into_values().map(|(_, r)| r.data["check"].clone()).collect();
    for need in list.iter().filter_map(Value::as_str) {
        let (check, platform) = split_need(need);
        let Some(command) = commands.get(check) else {
            return Ok(Needs::Skip(format!("it needs check '{check}', which is not configured")));
        };
        let argv = json!(layr_model::protection::check_argv(command));
        let on = |r: &&Value| {
            r["argv"] == argv && platform.is_none_or(|want| r["platform"].as_str().is_some_and(|have| meets(want, have)))
        };
        let exits: Vec<i64> = results.iter().filter(on).filter_map(|r| r["exit"].as_i64()).collect();
        if exits.contains(&0) {
            continue;
        }
        if !exits.is_empty() {
            return Ok(Needs::Skip(format!("check '{need}' failed")));
        }
        return Ok(Needs::Wait(format!("check '{need}' has no result yet")));
    }
    Ok(Needs::Ready)
}

/// Print once per process why a job stays pending here, so a watcher does not repeat it each pass.
fn tell_pending(state: &str, name: &str, why: &str) {
    static TOLD: std::sync::Mutex<BTreeSet<(String, String)>> = std::sync::Mutex::new(BTreeSet::new());
    if TOLD.lock().map(|mut told| told.insert((state.to_string(), name.to_string()))).unwrap_or(true) {
        eprintln!("layr checks: {name} of {} does not run on this machine: {why}", crate::ids::short(state));
    }
}

/// One pass of the executor: runs the jobs that no attestation covers, of every state or of `state` only.
fn scan(
    ctx: &Ctx,
    repo: &Repo,
    privileged_only: bool,
    state: Option<&str>,
    required: &[String],
    planned_only: bool,
) -> Result<Scan> {
    let explicit_state = state.is_some();
    let Plan { policy, main_states, heads, jobs, commands, privileged, .. } = plan(repo, state, required, planned_only)?;
    let here = platform();
    let limits = Limits {
        timeout: setting(&policy, "checks.timeout", json!(900))?.as_u64().context("invalid checks.timeout")?,
        memory: setting(&policy, "checks.memory", json!(4_u64 << 30))?.as_u64().context("invalid checks.memory")?,
        processes: setting(&policy, "checks.processes", json!(if cfg!(target_os = "macos") { 4096 } else { 256 }))?
            .as_u64()
            .context("invalid checks.processes")?,
        log_bytes: setting(&policy, "checks.log-bytes", json!(1 << 20))?.as_u64().context("invalid checks.log-bytes")? as usize,
    };
    let local = local_settings(repo)?;
    if privileged_only && !(local.runner && local.privileged) {
        bail!("privileged watcher requires local checks.runner and checks.privileged-runner true");
    }
    let after = setting(&policy, "checks.after", json!({}))?;
    // The jobs that need other checks (`checks.after`) go last, so in one pass the checks they need run first.
    let (mut jobs, later): (Vec<_>, Vec<_>) = jobs.into_iter().partition(|(_, name, _)| after.get(name.as_str()).is_none());
    jobs.extend(later);
    let mut results = Vec::new();
    let mut pending = 0;
    let mut pending_privileged = 0;
    for (state, name, want) in jobs {
        if want.as_deref().is_some_and(|want| !meets(want, &here)) {
            continue;
        }
        let command = &commands[&name];
        let argv = layr_model::protection::check_argv(command);
        let is_privileged = privileged.contains(&name);
        if privileged_only && !is_privileged {
            continue;
        }
        if attested(repo, &state, &argv, is_privileged, want.as_deref())? {
            continue;
        }
        match needs(repo, &state, &name, &after, &commands)? {
            Needs::Ready => {}
            Needs::Wait(why) | Needs::Skip(why) => {
                eprintln!("layr checks: {name} of {} does not run now: {why}", crate::ids::short(&state));
                continue;
            }
        }
        // Explicit requests can restore an older source from the remote. A watcher must not
        // fetch every historical source as it scans for work.
        if !repo.tree_available(&state) {
            if explicit_state {
                repo.check_view_read(ctx, &crate::overlay::View::new(state.clone(), vec![]))?;
                repo.state_root(&state).with_context(|| format!("cannot restore source for {state} ({name})"))?;
            } else {
                eprintln!("layr checks: source unavailable for {state} ({name})");
                continue;
            }
        }
        repo.check_view_read(ctx, &crate::overlay::View::new(state.clone(), vec![]))?;
        let host_allowed = (!cfg!(target_os = "macos") || !is_privileged)
            && (!(cfg!(target_os = "macos") || is_privileged) || approved(&policy, &state, &main_states));
        if !local.runner || (is_privileged && !local.privileged) || !host_allowed {
            pending += 1;
            if is_privileged {
                pending_privileged += 1;
            }
            let why = if !local.runner {
                "this machine is no check runner (layr config --local checks.runner true)".to_string()
            } else if is_privileged && cfg!(target_os = "macos") {
                "privileged checks run only on Linux".to_string()
            } else if is_privileged && !local.privileged {
                "this machine is no privileged runner (layr config --local checks.privileged-runner true)".to_string()
            } else {
                let host = if cfg!(target_os = "macos") {
                    "macOS runs checks without a sandbox"
                } else {
                    "a privileged check runs as root"
                };
                format!("{host}, so only a state of main or a state that a maintainer approved runs here (layr approve, or layr try <state> --check {name})")
            };
            tell_pending(&state, &name, &why);
            continue;
        }
        let result = super::admin::with_try_copy(ctx, repo, &state, &[], false, true, |dir| {
            let started = std::time::Instant::now();
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let back = heads.get(&state).map(String::as_str);
            let (code, log, truncated) = execute(ctx, repo, dir, &state, &argv, is_privileged, &limits, back)?;
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let (code, log, truncated): (i32, Vec<u8>, bool) = {
                bail!("automatic checks require Linux");
            };
            let work = repo.p.work_dir().join(crate::store::temp_name());
            crate::storage::create_subvolume(&work)?;
            let cleanup = crate::repo::Temp { path: work.clone() };
            std::fs::write(work.join("check.log"), &log)?;
            let _lock = repo.p.lock()?;
            let mut output =
                repo.new_state(ctx, &work, StateKind::Derived, vec![state.clone()], &format!("check {name} log"), None)?;
            output.derived_from = Some(state.clone());
            output.context = Some("check-v1".into());
            let log_ref = format!("derived/check-v1/{}", output.id);
            let result = json!({"name":name,"state":state,"command":command,"argv":argv,"exit":code,
                "privileged":is_privileged,"executor":true,"platform":here,"log_ref":log_ref,"output_state":output.id,"truncated":truncated,
                "duration_ms":started.elapsed().as_millis() as u64,"limits":{"timeout":limits.timeout,"memory":limits.memory,
                    "processes":limits.processes,"log_bytes":limits.log_bytes}});
            let mut record = super::attest::record(repo, &state, "check", &serde_json::to_vec(&result)?)?;
            record.states.push(output.clone());
            record.data["check"] = result.clone();
            record.data["ref_change"] = serde_json::to_value(RefChange {
                name: log_ref.clone(),
                before: None,
                after: Some(RefState { name: log_ref, target: output.id, expires_at: None }),
            })?;
            repo.commit_record(ctx, record)?;
            drop(cleanup);
            Ok(result)
        })?;
        results.push(result);
    }
    Ok(Scan { checks: results, pending, pending_privileged })
}

fn creator_policy(project: &str, records: Vec<crate::model::Record>) -> Result<(layr_model::Project, BTreeSet<String>)> {
    let creator = layr_model::project_perspective(project, &records)
        .context("automatic checks refuse to run without an unambiguous verified project creator")?;
    let model = layr_model::Project::from_records(project, records.clone(), &creator)?;
    let mut states = BTreeSet::new();
    for record in &records {
        if model.admins.contains(&record.machine) {
            for movement in record.lines.values() {
                if let Some(after) = &movement.after {
                    if after.name == "main" {
                        states.insert(after.head.clone());
                    }
                }
            }
        }
    }
    Ok((model, states))
}

/// Only exact-state, active approvals from current maintainers authorize host code.
fn approved(project: &layr_model::Project, state: &str, main: &BTreeSet<String>) -> bool {
    if main.contains(state) {
        return true;
    }
    let policy = project.access_policy(crate::ids::now_ms(), false);
    for (uid, _) in project.approvals(state) {
        // Uids refer to the creator's machine, so do not infer groups from this host.
        let who = layr_model::policy::Who { uid, groups: vec![] };
        if policy.role(&who) >= crate::access::Role::Maintainer && policy.denied_by(&who, "approve").is_none() {
            return true;
        }
    }
    project
        .person_approvals(state)
        .iter()
        .any(|(login, _)| policy.person_role(login) >= crate::access::Role::Maintainer && !policy.person_denied(login, "approve"))
}

struct Limits {
    timeout: u64,
    memory: u64,
    processes: u64,
    log_bytes: usize,
}

#[cfg(target_os = "linux")]
fn execute(
    ctx: &Ctx,
    repo: &Repo,
    dir: &std::path::Path,
    state: &str,
    argv: &[String],
    privileged: bool,
    limits: &Limits,
    back: Option<&str>,
) -> Result<(i32, Vec<u8>, bool)> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::process::Command;

    let private = if privileged {
        if !rustix::process::geteuid().is_root() {
            bail!("privileged checks require the root layr daemon");
        }
        let path = std::path::PathBuf::from(format!("/var/tmp/layr-check-{}", crate::ids::new_id()));
        // Filesystem suites start service clients under test users. They must
        // traverse this credential-free source copy to reach their own fixtures.
        std::fs::DirBuilder::new().mode(0o711).create(&path)?;
        Some(crate::repo::Temp { path })
    } else {
        None
    };
    let mut command;
    let home;
    if let Some(private) = &private {
        // Freeze the copy before copying it as root, without following source symlinks.
        let snapshot = repo.temp_snapshot(dir, true)?;
        let status = Command::new("/bin/cp")
            .args(["-a", "--no-preserve=ownership", "--reflink=auto", "--"])
            .arg(snapshot.path.join("."))
            .arg(&private.path)
            .status()?;
        if !status.success() {
            bail!("cannot prepare private privileged check folder");
        }
        // cp -a also copies the source root's mode over the prepared folder.
        // Restore traversal for the suite's unprivileged service clients.
        std::fs::set_permissions(&private.path, std::fs::Permissions::from_mode(0o711))?;
        // In its mount namespace the check sees its copy at /work and its home at /home/check, as a sandboxed
        // check does: the caches that a sandboxed check gives back (target/, the dependencies' sources below the
        // home) fit without a rebuild. The two host folders are only mount points.
        for point in ["/work", "/home/check"] {
            std::fs::DirBuilder::new().recursive(true).mode(0o755).create(point)?;
        }
        relocate_copy(repo, 0, &private.path, std::path::Path::new("/work"))?;
        home = private.path.join(".check-home");
        std::fs::DirBuilder::new().mode(0o700).create(&home)?;
        // Keep test mounts private so killing a timed-out suite releases its mounts too.
        // This is cleanup isolation, not a security boundary for code running as root.
        command = Command::new("/usr/bin/unshare");
        command
            .args(["--mount", "--propagation", "private", "--fork", "--kill-child", "--", "/bin/sh", "-c"])
            .arg(r#"mount --bind "$1" /work && mount --bind "$2" /home/check && cd /work && shift 2 && exec "$@""#)
            .arg("sh")
            .arg(&private.path)
            .arg(&home)
            .args(argv)
            .current_dir(&private.path);
        crate::privs::command_as(
            &mut command,
            0,
            &[("HOME".into(), "/home/check".into()), ("LAYR_TRY_STATE".into(), state.to_string())],
        )?;
    } else {
        // The check sees the copy at /work: its warm caches must not name the line they were built in.
        relocate_copy(repo, ctx.caller.uid, dir, std::path::Path::new("/work"))?;
        if !std::path::Path::new("/usr/bin/bwrap").is_file() {
            bail!("bubblewrap is required for automatic checks; install bubblewrap");
        }
        command = Command::new("/usr/bin/bwrap");
        command.args(["--unshare-all", "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv"]);
        // The users and groups (never /etc/shadow): a check's tools look up its own user, as git,
        // ssh-keygen and layr's `privs::command_as` do.
        for system in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc/alternatives", "/etc/passwd", "/etc/group"] {
            if std::path::Path::new(system).exists() {
                command.args(["--ro-bind", system, system]);
            }
        }
        command
            .args([
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--dir",
                "/home/check",
                "--setenv",
                "HOME",
                "/home/check",
                "--setenv",
                "PATH",
                "/usr/local/bin:/usr/bin:/bin",
                "--setenv",
                "USER",
                &ctx.caller.user,
                "--setenv",
                "LOGNAME",
                &ctx.caller.user,
                "--setenv",
                "LAYR_TRY_STATE",
                state,
                "--bind",
            ])
            .arg(dir)
            .args(["/work", "--chdir", "/work", "--"])
            .args(argv)
            .current_dir("/");
        crate::privs::command_as(&mut command, ctx.caller.uid, &[])?;
    }
    let result = run_check(command, limits)?;
    // A privileged copy is a plain folder of root: only a sandboxed check gives its caches back.
    if result.0 == 0 && private.is_none() {
        if let Some(line) = back {
            save_back(ctx, repo, line, state, dir);
        }
    }
    Ok(result)
}

/// The caches that a passing check of a head of the protected line `line` built in its copy `dir`, saved with that
/// head as the caches of the line `<line>.checks-<os>-<arch>` (made at the first such check; its head stays there).
/// `line new X <line>` on any machine of the platform then takes them (`cache_source`): one build per head warms
/// the next check and every machine. A protected head is reviewed code built in the sandbox, so the caches are
/// shared like the caches of a protected line. A pull request state never gives caches back. A failure here
/// leaves the check result as it is.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn save_back(ctx: &Ctx, repo: &Repo, line: &str, state: &str, dir: &std::path::Path) {
    match save_back_caches(ctx, repo, line, state, dir) {
        Ok((_, saved)) if saved.is_empty() => {
            eprintln!("layr checks: caches of {state} not saved: the check left no cache folder")
        }
        Ok(_) => {}
        Err(e) => eprintln!("layr checks: caches of {state} not saved: {e:#}"),
    }
}

/// The state, a descendant of `state`, whose caches the overlay lines of `checks_line` hold now for one of
/// `folders`.
fn newer_cache_given_back(
    repo: &Repo,
    checks_line: &crate::model::Line,
    state: &str,
    folders: &[String],
) -> Result<Option<String>> {
    for (name, folder) in repo.folder_overlays()? {
        if !folders.contains(&folder) {
            continue;
        }
        let info = repo.need_overlay(&name)?;
        let orepo = repo.overlay_repo(&name)?;
        let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, &info, checks_line)? else { continue };
        let Some(head) = orepo.p.db.state(&ol.head)? else { continue };
        if let Some(of) = head.cache_of {
            if of != state && repo.is_ancestor(state, &of)? {
                return Ok(Some(of));
            }
        }
    }
    Ok(None)
}

/// `save_back` for any caller (`checks run`, `save --overlays --save-back`): the save-back line's name and the saved
/// caches (folder, overlay, cache state). Empty when `dir` has no cache folder.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn save_back_caches(
    ctx: &Ctx,
    repo: &Repo,
    line: &str,
    state: &str,
    dir: &std::path::Path,
) -> Result<(String, Vec<(String, String, String)>)> {
    // The cache folders of the copy. A check that started cold made its folder as a plain folder (no cache
    // could warm it): it becomes a cache first, as in `save --overlays`, so the first build of a platform is saved.
    let mut paths: Vec<String> = repo.folder_overlays()?.into_iter().map(|(_, folder)| folder).collect();
    for p in repo.cache_paths(dir)? {
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    let gid = crate::ctx::Caller::from_uid(ctx.caller.uid).gid;
    let mut folders = Vec::new();
    for folder in paths {
        let Ok(path) = crate::fsutil::safe_join(dir, &folder) else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        if !crate::storage::is_subvolume(&path) {
            crate::repo::convert_cache(dir, &folder, ctx.caller.uid, gid)?;
        }
        if crate::storage::is_subvolume(&path) {
            folders.push(folder);
        }
    }
    let name = format!("{line}.checks-{}", platform());
    if folders.is_empty() {
        return Ok((name, Vec::new()));
    }
    // The source times of this copy go with its build output (docs/cache-overlays.md, section 20). The
    // `.layr-times` that came with the cache names the sources of an older build: a line that takes this
    // cache would give a changed source an older time than the new build output, and the tool would skip it.
    crate::source_times::record(dir, &folders)?;
    let _lock = repo.p.lock()?;
    let checks_line = match repo.p.db.line_by_name(&name)? {
        Some(l) => l,
        None => repo.create_line(ctx, &name, state, ctx.caller.uid)?,
    };
    // Checks run newest state first: the cache of an older head saved last would replace (and free) the
    // cache that a newer head gave back, and `line new` from that head would start cold.
    if let Some(newer) = newer_cache_given_back(repo, &checks_line, state, &folders)? {
        bail!("the newer head {} gave its caches back already", crate::ids::short(&newer));
    }
    let saved = repo.save_cache_overlays_in(ctx, &checks_line, dir, &folders, true, state)?;
    Ok((name, saved))
}

/// The cache folders of a check copy `dir`, which the check sees at `at`: units that name the line where they were
/// built (a compiled-in path) build again, as in a new line (`relocate_caches`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn relocate_copy(repo: &Repo, uid: u32, dir: &std::path::Path, at: &std::path::Path) -> Result<()> {
    let folders: Vec<String> = repo.folder_overlays()?.into_iter().map(|(_, folder)| folder).collect();
    repo.relocate_folders(uid, &folders, dir, at);
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_profile(work: &std::path::Path, seed: &std::path::Path) -> Result<String> {
    let quote = |path: &std::path::Path| serde_json::to_string(&path.to_string_lossy());
    Ok(format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow signal (target same-sandbox))
(allow mach-lookup)
(allow sysctl-read)
(allow system-info (info-type "vfs.disk-space"))
(allow network-bind (local unix-socket (path-prefix {socket_prefix})))
(allow network-outbound (remote unix-socket (path-prefix {socket_prefix})))
(allow file-read-metadata)
(allow file-read*
    (literal "/")
    (literal "/Volumes") (path-ancestors {work})
    (subpath "/System") (subpath "/usr") (subpath "/bin") (subpath "/sbin")
    (subpath "/Library/Apple") (subpath "/Library/Developer")
    (subpath "/Applications/Xcode.app/Contents")
    (subpath "/opt/homebrew") (subpath "/private/etc")
    (subpath "/private/var/db/dyld") (subpath "/private/var/db/timezone") (subpath "/dev")
    (subpath {work}) (subpath {seed})
    (subpath "/Volumes/LayrRunner") (subpath "/Volumes/LayrRunnerCase"))
(allow file-write*
    (subpath {work}) (literal "/dev/null")
    (subpath "/Volumes/LayrRunner") (subpath "/Volumes/LayrRunnerCase"))
; Sandbox regexes take no repetition counts.
(allow file-write-create file-write-unlink (regex #"^/private/tmp/lct-[0-9a-f]+$"))
; One rule per path: a unix-socket address takes only its first filter.
(allow network-bind (local unix-socket (subpath {work})))
(allow network-outbound (remote unix-socket (subpath {work})))
(allow network-bind (local unix-socket (subpath "/Volumes/LayrRunner")))
(allow network-outbound (remote unix-socket (subpath "/Volumes/LayrRunner")))
(allow network-bind (local unix-socket (subpath "/Volumes/LayrRunnerCase")))
(allow network-outbound (remote unix-socket (subpath "/Volumes/LayrRunnerCase")))
"#,
        work = quote(work)?,
        seed = quote(seed)?,
        socket_prefix = serde_json::to_string(&format!("{}/", work.display()))?
    ))
}

#[cfg(target_os = "macos")]
fn execute(
    ctx: &Ctx,
    repo: &Repo,
    dir: &std::path::Path,
    state: &str,
    argv: &[String],
    privileged: bool,
    limits: &Limits,
    back: Option<&str>,
) -> Result<(i32, Vec<u8>, bool)> {
    use std::os::unix::fs::DirBuilderExt;
    struct TempLink(std::path::PathBuf);
    impl Drop for TempLink {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    if privileged {
        bail!("privileged automatic checks are unavailable on macOS");
    }
    // Keep the writable tree outside the personal home. The no-follow APFS
    // helpers need readable ancestor directories, not access to the home tree.
    // A clone needs one volume: a store on another volume (a disk image) keeps the copy in its own
    // work folder, so a warm cache folder is cloned and not copied.
    let tmp = std::path::Path::new("/private/var/tmp");
    let base = if std::fs::metadata(dir)?.dev() == std::fs::metadata(tmp)?.dev() { tmp.to_path_buf() } else { repo.p.work_dir() };
    let private = crate::repo::Temp { path: base.join(format!("layr-check-{}", crate::ids::new_id())) };
    std::fs::DirBuilder::new().mode(0o700).create(&private.path)?;
    let same_volume = std::fs::metadata(dir)?.dev() == std::fs::metadata(&private.path)?.dev();
    let status = std::process::Command::new("/bin/cp")
        .args([if same_volume { "-cR" } else { "-R" }, "--"])
        .arg(dir.join("."))
        .arg(&private.path)
        .status()?;
    if !status.success() {
        bail!("cannot prepare private macOS check folder");
    }
    let dir = std::fs::canonicalize(&private.path)?;
    relocate_copy(repo, ctx.caller.uid, &dir, &dir)?;
    let home = dir.join(format!(".check-home-{}", crate::ids::new_id()));
    std::fs::DirBuilder::new().mode(0o700).create(&home)?;
    let scratch = home.join("tmp");
    std::fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    let link = std::path::Path::new("/tmp").join(format!("lct-{:016x}", rand::random::<u64>()));
    std::os::unix::fs::symlink(&scratch, &link)?;
    let _link = TempLink(link);
    let seed = std::path::PathBuf::from(crate::privs::user(ctx.caller.uid)?.home).join("Library/Application Support/Layr/checks");
    let mut command = std::process::Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &macos_profile(&dir, &seed)?, "--"]).args(argv).current_dir(&dir);
    crate::privs::command_as(&mut command, ctx.caller.uid, &[
        ("HOME".into(), home.display().to_string()),
        ("TMPDIR".into(), _link.0.display().to_string()),
        ("LAYR_CHECK_TMPDIR".into(), _link.0.display().to_string()),
        ("LAYR_TRY_STATE".into(), state.into()),
        ("LAYR_CHECK_SEED".into(), seed.display().to_string()),
        ("PATH".into(), "/opt/homebrew/opt/coreutils/libexec/gnubin:/opt/homebrew/opt/gnu-sed/libexec/gnubin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into()),
    ])?;
    let result = run_check(command, limits)?;
    if result.0 == 0 {
        if let Some(line) = back {
            save_back(ctx, repo, line, state, &dir);
        }
    }
    Ok(result)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_check(mut command: std::process::Command, limits: &Limits) -> Result<(i32, Vec<u8>, bool)> {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    // Limits are hard limits inherited by descendants; the sandbox also has a PID namespace.
    crate::sys::check_limits_before_exec(&mut command, limits.memory, limits.processes, limits.timeout)?;
    command.process_group(0).stdin(Stdio::null());
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    command.stdout(Stdio::from(OwnedFd::from(writer.try_clone()?))).stderr(Stdio::from(OwnedFd::from(writer)));
    let started = Instant::now();
    let mut child = command.spawn().context("cannot start automatic check")?;
    // Command owns its output descriptors; close them so EOF can be observed.
    drop(command);
    let pid = rustix::process::Pid::from_raw(child.id() as i32).context("invalid check pid")?;
    let mut log = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    let mut drain = |reader: &mut UnixStream| -> Result<()> {
        for _ in 0..128 {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let keep = n.min(limits.log_bytes.saturating_sub(log.len()));
                    log.extend_from_slice(&buffer[..keep]);
                    truncated |= keep < n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    };
    let outcome = (|| -> Result<i32> {
        loop {
            drain(&mut reader)?;
            if let Some(status) = child.try_wait()? {
                return Ok(status.code().unwrap_or(128 + status.signal().unwrap_or(0)));
            }
            if started.elapsed().as_secs() >= limits.timeout {
                return Ok(124);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // Stop any children left by the command, even when its immediate process succeeded.
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    let _ = child.kill();
    let _ = child.wait();
    drain(&mut reader)?;
    drop(drain);
    let code = outcome?;
    Ok((code, log, truncated))
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn sandbox_allows_only_unix_sockets_inside_work() -> Result<()> {
        use std::os::unix::net::UnixListener;
        let root = std::path::Path::new("/private/var/tmp").join(format!("lsc-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&root)?;
        let cleanup = crate::repo::Temp { path: root.clone() };
        let work = root.join("work");
        std::fs::create_dir(&work)?;
        let profile = macos_profile(&work, &work)?;
        let inside = work.join("inside.sock");
        let outside = root.join("outside.sock");
        let _inside = UnixListener::bind(&inside)?;
        let _outside = UnixListener::bind(&outside)?;
        let escape = work.join("escape.sock");
        std::os::unix::fs::symlink(&outside, &escape)?;
        let run = |script: &str, path: &std::path::Path| -> Result<bool> {
            Ok(Command::new("/usr/bin/sandbox-exec")
                .env_clear()
                .env("HOME", &work)
                .env("PATH", "/usr/bin:/bin")
                .current_dir(&work)
                .args(["-p", &profile, "--", "/usr/bin/ruby", "--disable-gems", "-rsocket", "-e", script])
                .arg(path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?
                .success())
        };
        let connect = "UNIXSocket.new(ARGV.fetch(0)).close";
        assert!(run(connect, &inside)?);
        assert!(!run(connect, &outside)?);
        assert!(!run(connect, &escape)?);
        let bind = "UNIXServer.new(ARGV.fetch(0)).close";
        let local = work.join("created.sock");
        assert!(run(bind, &local)?);
        assert!(local.exists());
        assert!(!run(bind, &root.join("forbidden.sock"))?);
        let external = root.join("external");
        std::fs::create_dir(&external)?;
        let linked = work.join("linked");
        std::os::unix::fs::symlink(&external, &linked)?;
        assert!(!run(bind, &linked.join("forbidden.sock"))?);
        assert!(!external.join("forbidden.sock").exists());
        drop(cleanup);
        Ok(())
    }

    #[test]
    fn sandbox_refuses_network_and_writes_outside_work() -> Result<()> {
        // macOS gives TMPDIR a long path; leave room for the Unix socket names.
        let name = crate::ids::display(&crate::ids::new_id());
        let root = std::env::temp_dir().join(format!("ls-{}", &name[..16]));
        std::fs::create_dir(&root)?;
        let cleanup = crate::repo::Temp { path: root.clone() };
        let work = root.join("work");
        std::fs::create_dir(&work)?;
        let work = std::fs::canonicalize(work)?;
        let profile = macos_profile(&work, &work)?;
        let status = |args: &[&std::ffi::OsStr]| {
            let result = Command::new("/usr/bin/sandbox-exec").env_clear().args(["-p", &profile, "--"]).args(args).status();
            result
        };
        assert!(status(&["/usr/bin/touch".as_ref(), work.join("allowed").as_os_str()])?.success());
        let outside = root.join("forbidden");
        assert!(!status(&["/usr/bin/touch".as_ref(), outside.as_os_str()])?.success());
        assert!(!outside.exists());
        let secret = root.join("outside-content");
        std::fs::write(&secret, b"outside the check")?;
        assert!(!status(&["/bin/cat".as_ref(), secret.as_os_str()])?.success());
        let server = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = server.local_addr()?.port().to_string();
        // The same open local port succeeds without the sandbox, ruling out connection refusal.
        assert!(Command::new("/usr/bin/nc").args(["-z", "-w", "1", "127.0.0.1", &port]).status()?.success());
        assert!(!status(&[
            "/usr/bin/nc".as_ref(),
            "-z".as_ref(),
            "-w".as_ref(),
            "1".as_ref(),
            "127.0.0.1".as_ref(),
            port.as_ref()
        ])?
        .success());
        // Checks may use a socket in their private work tree, but not reach a
        // socket outside it through a symlink.
        let local = work.join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&local)?;
        let worker = std::thread::spawn(move || -> std::io::Result<()> {
            let (stream, _) = listener.accept()?;
            drop(stream);
            Ok(())
        });
        assert!(status(&["/usr/bin/nc".as_ref(), "-U".as_ref(), local.as_os_str()])?.success());
        worker.join().unwrap()?;
        let external = root.join("external.sock");
        let _external = std::os::unix::net::UnixListener::bind(&external)?;
        let link = work.join("external-link.sock");
        std::os::unix::fs::symlink(&external, &link)?;
        assert!(!status(&["/usr/bin/nc".as_ref(), "-U".as_ref(), link.as_os_str()])?.success());
        drop(cleanup);
        Ok(())
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn required_definitions_include_commands_and_platforms() {
        let required = vec!["unit".to_string(), "apfs".to_string()];
        let commands =
            BTreeMap::from([("unit".to_string(), "cargo test".to_string()), ("apfs".to_string(), "apfs-test".to_string())]);
        let definitions = required_definitions(&required, &commands, &json!({"apfs": "macos-aarch64"}));
        assert_eq!(definitions["unit"], json!({"command": "cargo test", "platforms": null}));
        assert_eq!(definitions["apfs"], json!({"command": "apfs-test", "platforms": "macos-aarch64"}));
    }

    #[test]
    fn required_checks_need_a_state_passing_and_valid_names() {
        assert_eq!(required_checks(Some("unit,apfs"), Some("state"), true).unwrap(), vec!["unit", "apfs"]);
        assert!(required_checks(Some("unit"), None, true).is_err());
        assert!(required_checks(Some("unit"), Some("state"), false).is_err());
        assert!(required_checks(Some(""), Some("state"), true).is_err());
        assert!(required_checks(Some("unit,,apfs"), Some("state"), true).is_err());
    }

    #[test]
    fn passing_checks_require_a_matching_successful_executor() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "test".to_string()];
        let mut check = json!({"executor": true, "argv": argv, "privileged": false,
                               "exit": 1, "platform": "macos-aarch64"});
        assert!(matches_check(&check, &argv, false, Some("macos-aarch64"), false));
        assert!(!matches_check(&check, &argv, false, Some("macos-aarch64"), true));
        check["exit"] = json!(0);
        assert!(matches_check(&check, &argv, false, Some("macos-aarch64"), true));
        assert!(!matches_check(&check, &argv, false, Some("linux-x86_64"), true));
        assert!(!matches_check(&check, &argv, true, Some("macos-aarch64"), true));
        check["executor"] = json!(false);
        assert!(!matches_check(&check, &argv, false, Some("macos-aarch64"), true));
    }

    #[test]
    fn newest_state_checks_run_first() {
        let jobs = BTreeSet::from([("001-old".to_string(), "unit".to_string()), ("002-new".to_string(), "unit".to_string())]);
        let planned = order_jobs(jobs, &json!({"unit": ["linux-x86_64", "linux-aarch64", "linux-x86_64"]}));
        assert_eq!(
            planned.iter().map(|job| job.0.as_str()).collect::<Vec<_>>(),
            vec!["002-new", "002-new", "001-old", "001-old"]
        );
    }

    fn record(machine: &str, op: &str, data: Value) -> crate::model::Record {
        let mut record = crate::model::Record::new(op);
        record.id = crate::ids::new_id();
        record.project = "project".into();
        record.machine = machine.into();
        record.seq = 1;
        record.data = data;
        record
    }

    #[test]
    fn creator_commands_ignore_runner_overrides_and_require_creator() -> Result<()> {
        let creator = record(
            "owner",
            "project",
            json!({"id":"project","config":{
                "check.unit":"echo creator", "checks.changes":["unit"], "protect":{"main":{}}
            }}),
        );
        let runner = record(
            "runner",
            "config",
            json!({"config":{
                "check.unit":"echo local", "checks.changes":["local"], "protect":{}
            }}),
        );
        let (policy, _) = creator_policy("project", vec![creator.clone(), runner.clone()])?;
        assert_eq!(policy.setting("check.unit"), Some(&json!("echo creator")));
        assert_eq!(policy.setting("checks.changes"), Some(&json!(["unit"])));
        assert!(policy.access_policy(0, false).rule_for("main").is_some());
        assert!(creator_policy("project", vec![runner]).is_err());
        let conflicting = record("other", "project", json!({"id":"project","config":{}}));
        assert!(creator_policy("project", vec![creator, conflicting]).is_err());
        Ok(())
    }
}
