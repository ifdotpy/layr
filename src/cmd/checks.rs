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
    if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o077 != 0 || meta.len() > 1024 {
        bail!("{} must be a root-owned private regular file", path.display());
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
    if !ctx.caller.is_root() {
        bail!("only root may change machine-local check execution");
    }
    let enabled = if a.has("--unset") {
        false
    } else {
        if a.pos.len() != 2 {
            bail!("usage: layr config --local (checks.runner|checks.privileged-runner) (true|false)");
        }
        a.pos[1].parse::<bool>().context("privileged runner must be true or false")?
    };
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

pub fn checks(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--once").flag("--json").flag("--privileged-only").value("--interval"), args)?;
    if a.pos != ["watch"] || !a.paths.is_empty() {
        return Err(exit(128, "usage: layr checks watch [--once] [--privileged-only] [--interval <seconds>] [--json]"));
    }
    if !cfg!(target_os = "linux") {
        bail!("automatic checks require Linux");
    }
    if ctx.caller.is_root() {
        bail!("checks watch needs a non-root service user through the layr daemon");
    }
    let interval: u64 = a.get("--interval").unwrap_or("5").parse().context("invalid check interval")?;
    if !(1..=3600).contains(&interval) {
        bail!("check interval must be from 1 to 3600 seconds");
    }
    let h = here(ctx)?;
    h.repo.require(ctx, "try")?;
    // Match the queue runner: one watcher per project without holding the project write lock.
    let lock =
        std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(h.repo.p.meta_dir().join("checks.lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .context("a check watcher is already active for this project")?;
    loop {
        let h = here(ctx)?;
        h.repo.require(ctx, "try")?;
        let results = scan(ctx, &h.repo, a.has("--privileged-only"))?;
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
        if a.has("--once") {
            return Ok(if results.checks.iter().any(|r| r["exit"] != 0) { 1 } else { 0 });
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn setting(repo: &Repo, key: &str, default: Value) -> Result<Value> {
    let value = repo.p.config_value(key)?.unwrap_or(default);
    validate_config(key, &value)?;
    Ok(value)
}

struct Scan {
    checks: Vec<Value>,
    pending: usize,
    pending_privileged: usize,
}

fn scan(ctx: &Ctx, repo: &Repo, privileged_only: bool) -> Result<Scan> {
    let changes = names("checks.changes", &setting(repo, "checks.changes", json!([]))?)?;
    let lines = setting(repo, "checks.lines", json!({}))?;
    let privileged: BTreeSet<_> =
        names("checks.privileged", &setting(repo, "checks.privileged", json!([]))?)?.into_iter().collect();
    let mut jobs = BTreeSet::new();
    for change in repo.p.db.changes()? {
        for state in repo.p.db.change_versions(&change)? {
            for name in &changes {
                jobs.insert((state.id.clone(), name.clone()));
            }
        }
    }
    let mut watched_lines = BTreeMap::new();
    for (line, checks) in lines.as_object().context("checks.lines must be an object")? {
        if repo.policy().rule_for(line).is_none() {
            bail!("automatic line '{line}' must be protected");
        }
        let checks = names("checks.lines", checks)?;
        let line = repo.line(line)?;
        for (_, head) in repo.p.db.line_heads(&line.id)? {
            for name in &checks {
                jobs.insert((head.clone(), name.clone()));
            }
        }
        watched_lines.insert(line.id, checks);
    }
    // Include intermediate heads when more than one version arrived between scans.
    if !watched_lines.is_empty() {
        for record in repo.p.db.records(usize::MAX >> 1)? {
            for (id, movement) in record.lines {
                if let (Some(names), Some(after)) = (watched_lines.get(&id), movement.after) {
                    for name in names {
                        jobs.insert((after.head.clone(), name.clone()));
                    }
                }
            }
        }
    }
    let mut commands = BTreeMap::new();
    for (_, name) in &jobs {
        if !commands.contains_key(name) {
            let command = repo
                .p
                .config_value(&format!("check.{name}"))?
                .and_then(|v| v.as_str().map(String::from))
                .with_context(|| format!("check '{name}' is not defined (layr config check.{name} '<command>')"))?;
            commands.insert(name.clone(), command);
        }
    }
    let limits = Limits {
        timeout: setting(repo, "checks.timeout", json!(900))?.as_u64().context("invalid checks.timeout")?,
        memory: setting(repo, "checks.memory", json!(4_u64 << 30))?.as_u64().context("invalid checks.memory")?,
        processes: setting(repo, "checks.processes", json!(256))?.as_u64().context("invalid checks.processes")?,
        log_bytes: setting(repo, "checks.log-bytes", json!(1 << 20))?.as_u64().context("invalid checks.log-bytes")? as usize,
    };
    let local = local_settings(repo)?;
    if privileged_only && !(local.runner && local.privileged) {
        bail!("privileged watcher requires local checks.runner and checks.privileged-runner true");
    }
    let mut results = Vec::new();
    let mut pending = 0;
    let mut pending_privileged = 0;
    for (state, name) in jobs {
        let command = &commands[&name];
        let argv = layr_model::protection::check_argv(command);
        let is_privileged = privileged.contains(&name);
        if privileged_only && !is_privileged {
            continue;
        }
        let subject = Subject { kind: "state".into(), id: state.clone() };
        if repo.p.db.check_exit(&state, &argv, None)?.is_some()
            && crate::probes::active_checks(repo, &subject)?.into_values().any(|(_, r)| {
                r.data["check"]["executor"] == true
                    && r.data["check"]["argv"] == json!(argv)
                    && r.data["check"]["privileged"] == is_privileged
                    && r.data["check"]["exit"].as_i64().is_some()
            })
        {
            continue;
        }
        // Collected sources have metadata but cannot be executed. Never attest them as passing.
        if !repo.p.has_state(&state) {
            eprintln!("layr checks: source unavailable for {state} ({name})");
            continue;
        }
        repo.check_view_read(ctx, &crate::overlay::View::new(state.clone(), vec![]))?;
        if !local.runner || (is_privileged && !local.privileged) {
            pending += 1;
            if is_privileged {
                pending_privileged += 1;
            }
            continue;
        }
        let result = super::admin::with_try_copy(ctx, repo, &state, &[], false, |dir| {
            let started = std::time::Instant::now();
            #[cfg(target_os = "linux")]
            let (code, log, truncated) = execute(ctx, repo, dir, &state, &argv, is_privileged, &limits)?;
            #[cfg(not(target_os = "linux"))]
            let (code, log, truncated): (i32, Vec<u8>, bool) = {
                bail!("automatic checks require Linux");
            };
            let work = repo.p.work_dir().join(crate::ids::new_id());
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
                "privileged":is_privileged,"executor":true,"log_ref":log_ref,"output_state":output.id,"truncated":truncated,
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
) -> Result<(i32, Vec<u8>, bool)> {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let private = if privileged {
        if !rustix::process::geteuid().is_root() {
            bail!("privileged checks require the root layr daemon");
        }
        let path = std::path::PathBuf::from(format!("/var/tmp/layr-check-{}", crate::ids::new_id()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
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
        home = private.path.join(format!(".check-home-{}", crate::ids::new_id()));
        std::fs::create_dir(&home)?;
        // Keep test mounts private so killing a timed-out suite releases its mounts too.
        // This is cleanup isolation, not a security boundary for code running as root.
        command = Command::new("/usr/bin/unshare");
        command
            .args(["--mount", "--propagation", "private", "--fork", "--kill-child", "--"])
            .args(argv)
            .current_dir(&private.path);
        crate::privs::command_as(
            &mut command,
            0,
            &[("HOME".into(), home.display().to_string()), ("LAYR_TRY_STATE".into(), state.to_string())],
        )?;
    } else {
        if !std::path::Path::new("/usr/bin/bwrap").is_file() {
            bail!("bubblewrap is required for automatic checks; install bubblewrap");
        }
        command = Command::new("/usr/bin/bwrap");
        command.args(["--unshare-all", "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv"]);
        for system in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
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
    drop(private);
    Ok((code, log, truncated))
}
