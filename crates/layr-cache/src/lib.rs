//! The cache adapters: the cargo and pnpm adapters that prune a cache folder of a line to its live
//! set. They implement the interface of `layr-adapter`. The binary registers `adapters()` at startup.
//! An adapter works offline and compiles nothing. A stale or failing folder is skipped and left unchanged.
//! See docs/cache-overlays.md, section 19.

pub mod cargo;
pub mod pnpm;
pub mod sweep;

use anyhow::{anyhow, bail, Context, Result};
use layr_adapter::{CacheAdapter, Host};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use layr_adapter::Outcome;

/// The adapters of this crate, for the binary to register.
pub fn adapters() -> Vec<Box<dyn CacheAdapter>> {
    vec![Box::new(cargo::Cargo), Box::new(pnpm::Pnpm)]
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

/// The stdout of a command that stops at `until`: its process group is killed then, and the run is an error.
/// A failed command is an error with its stderr. The command reads no input.
pub fn run(cmd: &mut Command, until: Option<Instant>) -> Result<Vec<u8>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().with_context(|| format!("cannot start {program}"))?;
    let pid = child.id();
    let late = Arc::new(AtomicBool::new(false));
    let (done, waiting) = mpsc::channel::<()>();
    let timer = until.map(|t| {
        let late = late.clone();
        std::thread::spawn(move || {
            if let Err(RecvTimeoutError::Timeout) = waiting.recv_timeout(t.saturating_duration_since(Instant::now())) {
                late.store(true, Ordering::SeqCst);
                kill_group(pid);
            }
        })
    });
    let out = child.wait_with_output();
    drop(done);
    if let Some(timer) = timer {
        let _ = timer.join();
    }
    let out = out.with_context(|| format!("cannot wait for {program}"))?;
    if late.load(Ordering::SeqCst) {
        bail!("{program} reached its time limit");
    }
    if !out.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
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
