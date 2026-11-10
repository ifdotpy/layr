//! The cache adapters: the cargo, pnpm and npm adapters that prune a cache folder of a line to its live
//! set. They implement the interface of `layr-adapter`. The binary registers `adapters()` at startup.
//! An adapter works offline and compiles nothing. A stale or failing folder is skipped and left unchanged.
//! See docs/cache-overlays.md, section 19.

pub mod cargo;
pub mod pnpm;
pub mod relocate;
pub mod sweep;

use anyhow::{anyhow, bail, Context, Result};
use layr_adapter::{CacheAdapter, Host};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub use layr_adapter::Outcome;

/// The adapters of this crate, for the binary to register.
pub fn adapters() -> Vec<Box<dyn CacheAdapter>> {
    vec![Box::new(cargo::Cargo), Box::new(pnpm::Pnpm), Box::new(pnpm::Npm)]
}

/// A host for runs in this process: the caller's own user, and a deadline that the caller sets. The
/// `layr retest prune` command uses it.
pub struct LocalHost {
    until: Instant,
}

impl LocalHost {
    /// A host whose processes stop `limit` from now.
    pub fn new(limit: Duration) -> Self {
        LocalHost { until: Instant::now() + limit }
    }
}

impl Host for LocalHost {
    fn command(&self, program: &str) -> io::Result<Command> {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(program);
        cmd.process_group(0);
        Ok(cmd)
    }

    fn deadline(&self) -> Instant {
        self.until
    }
}

/// Prunes `folder`, by the adapter `setting` names. `None` detects the adapter from the folder. `none` skips.
/// Returns the adapter name (`none`, or `-` when no adapter applies) with the outcome. Errors only for a
/// name that no adapter has.
pub fn prune(setting: Option<&str>, folder: &Path) -> Result<(&'static str, Outcome)> {
    let host = LocalHost::new(Duration::from_secs(3600));
    let adapters = adapters();
    let found = match setting {
        Some("none") => return Ok(("none", skip("pruning is off (prune=none)"))),
        Some(name) => match adapters.iter().find(|a| a.name() == name) {
            Some(a) => a,
            None => bail!("unknown prune adapter '{name}'"),
        },
        None => match adapters.iter().find(|a| a.detects(folder)) {
            Some(a) => a,
            None => return Ok(("-", skip("no adapter matches the folder"))),
        },
    };
    Ok((found.name(), found.prune(&host, folder)))
}

/// A skipped outcome with its reason.
pub(crate) fn skip(reason: impl Into<String>) -> Outcome {
    Outcome::Skipped { reason: reason.into() }
}

/// Runs the command that `build` makes from the host's command for `program`, and returns its stdout
/// (see `run`). The deadline is the host's.
pub(crate) fn exec(host: &dyn Host, program: &str, build: impl FnOnce(&mut Command) -> &mut Command) -> Result<Vec<u8>> {
    let mut cmd = host.command(program).map_err(|e| anyhow!("cannot run {program}: {e}"))?;
    build(&mut cmd);
    run(&mut cmd, Some(host.deadline()))
}

/// The stdout of a command, with no time limit. A command that fails is an error with its stderr.
pub fn output(cmd: &mut Command) -> Result<Vec<u8>> {
    run(cmd, None)
}

/// Time the output pipes get after their command ended. A pipe that is still open then belongs to a process
/// outside the command's group, so the command fails instead of waiting for it.
const PIPE_GRACE: Duration = Duration::from_secs(5);

/// Reads `pipe` to its end on a thread. The bytes arrive on the returned channel.
pub(crate) fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// The bytes of a pipe that `drain` reads, after its command ended.
pub(crate) fn pipe_end(rx: &mpsc::Receiver<Vec<u8>>, program: &str) -> Result<Vec<u8>> {
    rx.recv_timeout(PIPE_GRACE)
        .map_err(|_| anyhow!("{program}: a pipe stayed open after the command ended; a process outside its group holds it"))
}

/// Waits for `child`. At `until` its process group is killed. Returns the status and whether the limit passed.
/// Without a limit it waits for the child.
pub(crate) fn wait_child(child: &mut Child, until: Option<Instant>) -> Result<(ExitStatus, bool)> {
    let Some(until) = until else { return Ok((child.wait()?, false)) };
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false));
        }
        if Instant::now() >= until {
            // The child may share the worker's group (WorkerHost): its own processes (rustc under cargo) are found
            // first and killed with it, so none keeps writing after the limit.
            let tree = descendants(child.id());
            kill_group(child.id());
            for pid in tree {
                let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).stderr(Stdio::null()).status();
            }
            return Ok((child.wait()?, true));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The stdout of a command that stops at `until`: its process group is killed then, and the run is an error.
/// A failed command is an error with its stderr. The command reads no input. The run does not wait for a pipe
/// that a process outside the group keeps open: after the command ended, the pipes get `PIPE_GRACE`.
pub fn run(cmd: &mut Command, until: Option<Instant>) -> Result<Vec<u8>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("cannot start {program}"))?;
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let (status, late) = wait_child(&mut child, until).with_context(|| format!("cannot wait for {program}"))?;
    if late {
        bail!("{program} reached its time limit");
    }
    let stdout = pipe_end(&out, &program)?;
    let stderr = pipe_end(&err, &program)?;
    if !status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&stderr).trim());
    }
    Ok(stdout)
}

/// The host of the retest commands: the cargo listings and metadata stop at the limit. `LAYR_RETEST_CARGO_TIMEOUT`
/// (seconds) sets it, and the default is 600.
pub fn retest_host() -> LocalHost {
    let secs = std::env::var("LAYR_RETEST_CARGO_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    LocalHost::new(Duration::from_secs(secs))
}

/// The processes below `pid` (children, their children, ...), found with `pgrep -P`.
fn descendants(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut todo = vec![pid];
    while let Some(p) = todo.pop() {
        let Ok(o) = Command::new("pgrep").args(["-P", &p.to_string()]).stderr(Stdio::null()).output() else { continue };
        for child in String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
            if !out.contains(&child) {
                out.push(child);
                todo.push(child);
            }
        }
    }
    out
}

/// Kills the process `pid` and the process group named by it (a child that is a group leader).
pub(crate) fn kill_group(pid: u32) {
    // The group kill fails when the child is not a group leader; then the process alone is killed. A kill of a
    // process that already exited prints nothing.
    let group = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
    if !group.is_ok_and(|s| s.success()) {
        let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).stderr(Stdio::null()).status();
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    /// A command that is not a group leader (the worker's children) and runs a child of its own: at the limit both go.
    #[test]
    fn the_limit_stops_the_processes_below_the_command_too() {
        let marker = format!("{}.{}", 61, std::process::id());
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &format!("sleep {marker} & wait")]);
        let err = run(&mut cmd, Some(Instant::now() + Duration::from_millis(500))).unwrap_err();
        assert!(format!("{err:#}").contains("time limit"), "{err:#}");
        std::thread::sleep(Duration::from_millis(200));
        let left = Command::new("pgrep").args(["-f", &format!("sleep {marker}")]).output().unwrap();
        assert!(String::from_utf8_lossy(&left.stdout).trim().is_empty(), "a process below the command still runs");
    }
}
