//! The cache-adapter hook of core (issue #167). Core stores the overlay property `prune` and runs the
//! adapter that the binary registered under that name. Core names no adapter: `register` gets them
//! from `main`.
//!
//! An adapter runs in a worker process: the layr binary started as `__cache-hook`, as the line owner.
//! The worker is a process group of its own, and the adapter's processes (cargo, pnpm) stay in it.
//! Two limits:
//! - The soft limit (`HOOK_LIMIT`, 120 s) is the adapter's own deadline. The adapter stops its processes
//!   at it, restores or skips the folder, and reports. This is the normal path.
//! - The hard limit is the soft limit plus `GRACE` (10 s). At the hard limit the parent kills the whole
//!   process group. Then a recovery worker runs `recover` on the folder (pnpm puts its copy back). If that
//!   fails, the folder is reported damaged (`Done::folder_ok` is false), and the caller marks it so that
//!   saves skip it until a prune of it succeeds.
//!
//! Nothing here fails a command: the result is one log line.

use crate::privs;
use layr_adapter::{CacheAdapter, Host, Outcome, Toolchain};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

/// The soft limit of one hook: the adapter stops its processes at it and skips or restores the folder.
pub const HOOK_LIMIT: Duration = Duration::from_secs(120);
/// The soft limit of one build of the standard commands (`warm`): a build of a large project takes minutes.
pub const WARM_LIMIT: Duration = Duration::from_secs(30 * 60);
/// The soft limit of a prune in the background follow pass: no one waits for it, and a loaded machine (tests of other
/// lines) makes a prune of seconds take minutes. A prune cut at its limit keeps its garbage until the next pass.
pub const BACKGROUND_PRUNE_LIMIT: Duration = Duration::from_secs(10 * 60);
/// The hard limit is the soft limit plus this grace: the parent kills the worker's process group then.
const GRACE: Duration = Duration::from_secs(10);
/// The time limit of a recovery worker (it only restores a copy), and the time the parent waits for it.
const RECOVER_LIMIT: Duration = Duration::from_secs(30);
/// The time limit of a relocation worker (it reads dep-info files and removes fingerprints).
const RELOCATE_LIMIT: Duration = Duration::from_secs(60);
/// The time limit of a toolchain command (`owner_output`): a toolchain manager may install a toolchain first.
const OUTPUT_LIMIT: Duration = Duration::from_secs(60);
/// The argument that starts the worker in the layr binary (`main`).
pub const WORKER_ARG: &str = "__cache-hook";
/// The worker's environment (its parameters): the mode (`prune`, `warm`, `recover` or `relocate`), the setting
/// (`-` for none), the folder, the limit in ms, and for `relocate` the path where the folder is used.
const ENV_MODE: &str = "LAYR_CACHE_HOOK_MODE";
const ENV_SETTING: &str = "LAYR_CACHE_HOOK_SETTING";
const ENV_FOLDER: &str = "LAYR_CACHE_HOOK_FOLDER";
const ENV_LIMIT_MS: &str = "LAYR_CACHE_HOOK_LIMIT_MS";
const ENV_AT: &str = "LAYR_CACHE_HOOK_AT";
/// The line in the worker's stdout that carries its report, so the parent finds it among other output.
const REPORT_MARK: &str = "LAYR-CACHE-HOOK ";

static ADAPTERS: OnceLock<Vec<Box<dyn CacheAdapter>>> = OnceLock::new();

/// Registers the adapters of the binary. `main` calls it once, before any command runs.
pub fn register(adapters: Vec<Box<dyn CacheAdapter>>) {
    let _ = ADAPTERS.set(adapters);
}

/// The registered adapters.
fn registered() -> &'static [Box<dyn CacheAdapter>] {
    ADAPTERS.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// What a worker reports. `folder_ok` is false when the folder may be changed and was not restored. `freed`: the
/// bytes that a prune freed; for a relocation, the number of units that it made stale.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Report {
    pub adapter: String,
    pub pruned: bool,
    pub freed: u64,
    pub restored: bool,
    pub folder_ok: bool,
    pub reason: String,
}

/// The result of one hook run: the log line, and whether the folder is in a consistent state. A folder
/// that is not consistent is marked damaged by the caller.
pub struct Done {
    pub log: String,
    pub folder_ok: bool,
    /// A build (`warm`) ran to its end. False when it stopped at its time limit or failed; true for the other modes.
    pub built: bool,
}

/// The command that starts a worker: the layr binary with `__cache-hook`, or the test binary in tests.
pub struct Launch {
    pub exe: PathBuf,
    pub args: Vec<String>,
}

/// What an adapter gets from the worker: its commands run in the worker's group, and the time limit.
struct WorkerHost {
    until: Instant,
}

impl Host for WorkerHost {
    fn command(&self, program: &str) -> std::io::Result<Command> {
        // The worker already runs as the owner, with the owner's environment: the child inherits both, and
        // stays in the worker's process group, so a kill of the group stops it too.
        Ok(Command::new(program))
    }

    fn deadline(&self) -> Instant {
        self.until
    }
}

/// The search path of the owner's tools: its own bin folders, then the system folders. A process that is not
/// root runs as its user, so its own `PATH` (where that user keeps the tools) comes first. The root service
/// never passes its own `PATH` on.
fn owner_path(uid: u32) -> String {
    let home = privs::user(uid).map(|u| u.home).unwrap_or_else(|_| "/".into());
    // The rustup of Homebrew keeps cargo in its own folder (the daemon has no shell PATH).
    let defaults = format!(
        "{home}/.cargo/bin:{home}/.local/bin:/opt/homebrew/opt/rustup/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
    );
    match std::env::var("PATH") {
        Ok(own) if !rustix::process::geteuid().is_root() => format!("{own}:{defaults}"),
        _ => defaults,
    }
}

/// Runs the hook for `folder` of a line of owner `uid`, with the registered adapters. `setting` is the overlay
/// property: `None` detects the adapter, `none` turns the hook off.
pub fn run(setting: Option<&str>, folder: &Path, uid: u32, limit: Duration) -> Done {
    run_mode(Mode::Prune, setting, folder, uid, limit)
}

/// The hook's work: a prune, or a build of the standard commands (`warm`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Prune,
    Warm,
}

/// `run` for `mode`.
pub fn run_mode(mode: Mode, setting: Option<&str>, folder: &Path, uid: u32, limit: Duration) -> Done {
    let quiet = Done { log: String::new(), folder_ok: true, built: true };
    // Nothing to do, so nothing is started or logged: the hook is off, the owner is root (the hook runs as the
    // owner, and root is never pruned), or no adapter applies to the folder.
    if setting == Some("none") || uid == 0 {
        return quiet;
    }
    match setting {
        Some(name) if !registered().iter().any(|a| a.name() == name) => {
            return Done {
                log: format!("{}: not pruned: unknown prune adapter '{name}'", folder.display()),
                folder_ok: true,
                built: true,
            };
        }
        None if !registered().iter().any(|a| a.detects(folder)) => return quiet,
        _ => {}
    }
    let launch = Launch { exe: std::env::current_exe().unwrap_or_else(|_| "layr".into()), args: vec![WORKER_ARG.into()] };
    match mode {
        Mode::Prune => run_with(&launch, &[], setting, folder, uid, limit, limit + GRACE),
        Mode::Warm => warm_with(&launch, &[], setting, folder, uid, limit, limit + GRACE),
    }
}

/// The relocation hook (`CacheAdapter::relocated`) for `folder` of a line of owner `uid`: the folder came from
/// another path, and it is used at `at` from now on. It runs for every owner (root too) and with the hook off
/// (`prune=none`): it only makes stale the outputs that name the old place. The log is empty when nothing changed.
pub fn relocate(setting: Option<&str>, folder: &Path, at: &Path, uid: u32) -> Done {
    let setting = setting.filter(|s| *s != "none");
    let applies = match setting {
        Some(name) => registered().iter().any(|a| a.name() == name),
        None => registered().iter().any(|a| a.detects(folder)),
    };
    if !applies {
        return Done { log: String::new(), folder_ok: true, built: true };
    }
    let launch = Launch { exe: std::env::current_exe().unwrap_or_else(|_| "layr".into()), args: vec![WORKER_ARG.into()] };
    let mut done = relocate_with(&launch, &[], setting, folder, at, uid);
    // The note only reads the folder, so it needs no worker.
    let note = registered()
        .iter()
        .find(|a| setting.map_or_else(|| a.detects(folder), |name| a.name() == name))
        .and_then(|a| a.relocation_note(folder));
    if let Some(note) = note {
        let line = format!("{}: {note}", at.display());
        done.log = if done.log.is_empty() { line } else { format!("{}\nlayr: relocate {line}", done.log) };
    }
    done
}

/// `relocate` with the worker started by `launch` and extra environment for it. A failure changes no file that a
/// build needs: a removed fingerprint only makes a unit build again.
pub fn relocate_with(launch: &Launch, env: &[(&str, &str)], setting: Option<&str>, folder: &Path, at: &Path, uid: u32) -> Done {
    let shown = at.display();
    let at_text = at.display().to_string();
    let mut env = env.to_vec();
    env.push((ENV_AT, &at_text));
    let log = match execute(launch, &env, "relocate", setting, folder, uid, RELOCATE_LIMIT, RELOCATE_LIMIT + GRACE) {
        Run::Report(r) if !r.reason.is_empty() => format!("{shown}: not relocated with {}: {}", r.adapter, r.reason),
        Run::Report(r) if r.freed == 0 => String::new(),
        Run::Report(r) => format!("{shown}: {} units named the old path; the next build makes them again", r.freed),
        Run::Start(e) => format!("{shown}: not relocated: cannot start the cache hook: {e}"),
        Run::Failed(e) => format!("{shown}: not relocated: {e}"),
        Run::Killed => format!("{shown}: not relocated: timed out after {} s", RELOCATE_LIMIT.as_secs()),
    };
    Done { log, folder_ok: true, built: true }
}

/// The build of the standard commands in a worker. After a stop at the hard limit the adapter's `recover` runs,
/// as after a prune (cargo has nothing to restore; pnpm puts its copy back).
#[allow(clippy::too_many_arguments)]
pub fn warm_with(launch: &Launch, env: &[(&str, &str)], setting: Option<&str>, folder: &Path, uid: u32, limit: Duration, wait: Duration) -> Done {
    let shown = folder.display();
    let mut built = false;
    let log = match execute(launch, env, "warm", setting, folder, uid, limit, wait) {
        Run::Report(r) if r.pruned => {
            built = true;
            format!("{shown}: built with {}", r.adapter)
        }
        Run::Report(r) => format!("{shown}: not built with {}: {}", r.adapter, r.reason),
        Run::Start(e) => format!("{shown}: not built: cannot start the cache hook: {e}"),
        Run::Failed(e) => format!("{shown}: not built: {e}"),
        Run::Killed => {
            // A stopped install can leave a half folder (pnpm): the adapter puts its copy back, as after a prune.
            let restore = execute(launch, env, "recover", setting, folder, uid, RECOVER_LIMIT, RECOVER_LIMIT + GRACE);
            return match restore {
                Run::Report(r) if r.folder_ok => Done {
                    log: format!("{shown}: not built: timed out after {} s, processes stopped", limit.as_secs()),
                    folder_ok: true,
                    built: false,
                },
                Run::Report(r) => Done { log: damaged(folder, &r.reason), folder_ok: false, built: false },
                Run::Start(e) | Run::Failed(e) => Done { log: damaged(folder, &e), folder_ok: false, built: false },
                Run::Killed => Done { log: damaged(folder, "the recovery timed out"), folder_ok: false, built: false },
            };
        }
    };
    Done { log, folder_ok: true, built }
}

/// What one worker run ended with.
enum Run {
    /// The worker printed its report.
    Report(Report),
    /// The worker never started (the owner cannot run it): nothing ran, the folder is as it was.
    Start(String),
    /// The worker ended without a report: it may have changed the folder.
    Failed(String),
    /// The worker was still running at the wait: its process group was killed.
    Killed,
}

/// Starts a worker in `mode` on `folder`, and waits `wait` for it. `env` is extra environment for it.
#[allow(clippy::too_many_arguments)]
fn execute(launch: &Launch, env: &[(&str, &str)], mode: &str, setting: Option<&str>, folder: &Path, uid: u32, limit: Duration, wait: Duration) -> Run {
    let mut vars: Vec<(String, String)> = vec![
        (ENV_MODE.into(), mode.into()),
        (ENV_SETTING.into(), setting.unwrap_or("-").into()),
        (ENV_FOLDER.into(), folder.display().to_string()),
        (ENV_LIMIT_MS.into(), limit.as_millis().to_string()),
        ("PATH".into(), owner_path(uid)),
    ];
    vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let mut cmd = Command::new(&launch.exe);
    crate::sys::inherit_background(&mut cmd);
    cmd.args(&launch.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Err(e) = privs::command_as(&mut cmd, uid, &vars) {
        return Run::Start(format!("{e:#}"));
    }
    // The worker is a group leader: its adapter's processes stay in this group (see `WorkerHost`).
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Run::Start(e.to_string()),
    };
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(wait) {
        Ok(Ok(out)) => {
            let text = String::from_utf8_lossy(&out.stdout);
            match text.lines().find_map(|l| l.strip_prefix(REPORT_MARK)).and_then(|j| serde_json::from_str::<Report>(j).ok()) {
                Some(r) => Run::Report(r),
                None => Run::Failed(format!("the cache hook gave no report ({})", out.status)),
            }
        }
        Ok(Err(e)) => Run::Failed(e.to_string()),
        Err(_) => {
            kill_group(pid);
            Run::Killed
        }
    }
}

/// `run` with the worker started by `launch`, extra environment for it, and `wait` as the hard limit: the time
/// core waits for the worker before it kills the worker's process group.
#[allow(clippy::too_many_arguments)]
pub fn run_with(launch: &Launch, env: &[(&str, &str)], setting: Option<&str>, folder: &Path, uid: u32, limit: Duration, wait: Duration) -> Done {
    let shown = folder.display();
    if setting == Some("none") {
        return Done { log: format!("{shown}: prune is off"), folder_ok: true, built: true };
    }
    match execute(launch, env, "prune", setting, folder, uid, limit, wait) {
        Run::Report(r) => Done { log: describe(folder, &r), folder_ok: r.folder_ok, built: true },
        Run::Start(e) => {
            Done { log: format!("{shown}: not pruned: cannot start the cache hook: {e}"), folder_ok: true, built: true }
        }
        Run::Failed(e) => Done { log: format!("{shown}: not pruned: {e}"), folder_ok: false, built: true },
        Run::Killed => {
            // The hard limit passed: the processes of the hook are stopped. Put the folder back, or report it damaged.
            let restore = execute(launch, env, "recover", setting, folder, uid, RECOVER_LIMIT, RECOVER_LIMIT + GRACE);
            match restore {
                Run::Report(r) if r.folder_ok => Done {
                    log: format!(
                        "{shown}: timed out after {} s, processes stopped; {}",
                        limit.as_secs(),
                        if r.restored { "restored from its copy" } else { "nothing to restore" }
                    ),
                    folder_ok: true,
                    built: true,
                },
                Run::Report(r) => Done { log: damaged(folder, &r.reason), folder_ok: false, built: true },
                Run::Start(e) | Run::Failed(e) => Done { log: damaged(folder, &e), folder_ok: false, built: true },
                Run::Killed => Done { log: damaged(folder, "the recovery timed out"), folder_ok: false, built: true },
            }
        }
    }
}

fn damaged(folder: &Path, reason: &str) -> String {
    format!("{}: stopped at the time limit and not restored ({reason}); marked damaged, saves skip it until a prune succeeds", folder.display())
}

/// The log line of a report of a prune.
fn describe(folder: &Path, r: &Report) -> String {
    let shown = folder.display();
    if r.pruned {
        format!("{shown}: pruned with {}, freed {} bytes", r.adapter, r.freed)
    } else {
        format!("{shown}: not pruned with {}: {}", r.adapter, r.reason)
    }
}

/// Kills the process `pid` and its process group (the worker is a group leader).
fn kill_group(pid: u32) {
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
    let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
}

/// The worker (`__cache-hook`): runs the mode and the adapter that its environment names, and prints the
/// report. Exit code 0 when a report was printed.
pub fn worker() -> i32 {
    worker_with(registered())
}

/// The worker with the adapters given (tests use fake adapters).
pub fn worker_with(adapters: &'static [Box<dyn CacheAdapter>]) -> i32 {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let setting = var(ENV_SETTING);
    let setting = (setting != "-").then_some(setting);
    let folder = PathBuf::from(var(ENV_FOLDER));
    let limit = Duration::from_millis(var(ENV_LIMIT_MS).parse().unwrap_or(0));
    let report = match var(ENV_MODE).as_str() {
        "recover" => recover_here(adapters, setting.as_deref(), &folder),
        "relocate" => relocate_here(adapters, setting.as_deref(), &folder, Path::new(&var(ENV_AT))),
        "warm" => warm_here(adapters, setting.as_deref(), &folder, limit),
        _ => prune_here(adapters, setting.as_deref(), &folder, limit),
    };
    println!("{REPORT_MARK}{}", serde_json::to_string(&report).unwrap_or_default());
    0
}

/// The adapter of `folder` for its cache key: the one that the overlay property `prune` names, or the one that
/// detects the folder (also when the property turns the prune off or names no known adapter): the key does not
/// depend on the prune setting.
pub fn adapter_for(setting: Option<&str>, folder: &Path) -> Option<&'static dyn CacheAdapter> {
    find(registered(), setting.filter(|s| *s != "none"), folder).or_else(|_| find(registered(), None, folder)).ok()
}

/// Outputs of `toolchain_output`, by command, selecting files and user: one run per command.
static OUTPUTS: std::sync::Mutex<Vec<((String, Vec<(String, Vec<u8>)>, u32), Option<String>)>> = std::sync::Mutex::new(Vec::new());

/// The standard output of the toolchain command `t` for the files in `dir`, run as `uid` with the owner's search
/// path, in an empty folder with copies of the files that select the toolchain (`dir` may be a folder that the
/// owner cannot enter, such as the store's work folder). None when it fails or runs longer than `OUTPUT_LIMIT`.
/// One run per command: a toolchain does not change during it.
pub fn toolchain_output(t: &Toolchain, dir: &Path, uid: u32) -> Option<String> {
    let files: Vec<(String, Vec<u8>)> =
        t.files.iter().filter_map(|f| std::fs::read(dir.join(f)).ok().map(|d| (f.to_string(), d))).collect();
    let id = (format!("{} {}", t.program, t.args.join(" ")), files.clone(), uid);
    if let Some((_, out)) = OUTPUTS.lock().ok()?.iter().find(|(k, _)| *k == id) {
        return out.clone();
    }
    let run = std::env::temp_dir().join(format!("layr-toolchain-{}", crate::ids::new_id()));
    let out = (|| {
        std::fs::create_dir(&run).ok()?;
        for (name, data) in &files {
            std::fs::write(run.join(name), data).ok()?;
        }
        if rustix::process::geteuid().is_root() {
            let gid = privs::user(uid).ok()?.gid;
            for p in std::iter::once(run.clone()).chain(files.iter().map(|(n, _)| run.join(n))) {
                std::os::unix::fs::chown(&p, Some(uid), Some(gid)).ok()?;
            }
        }
        let mut cmd = Command::new(t.program);
        cmd.args(t.args).current_dir(&run).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        privs::command_as(&mut cmd, uid, &[("PATH".into(), owner_path(uid))]).ok()?;
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        let child = cmd.spawn().ok()?;
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(OUTPUT_LIMIT) {
            Ok(Ok(o)) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
            Ok(_) => None,
            Err(_) => {
                kill_group(pid);
                None
            }
        }
    })();
    let _ = std::fs::remove_dir_all(&run);
    if let Ok(mut outputs) = OUTPUTS.lock() {
        outputs.push((id, out.clone()));
    }
    out
}

/// The adapter for a folder: the one named by the setting, or the one that detects the folder.
fn find<'a>(adapters: &'a [Box<dyn CacheAdapter>], setting: Option<&str>, folder: &Path) -> Result<&'a dyn CacheAdapter, String> {
    match setting {
        Some(name) => adapters.iter().map(|a| a.as_ref()).find(|a| a.name() == name).ok_or_else(|| format!("unknown prune adapter '{name}'")),
        None => adapters.iter().map(|a| a.as_ref()).find(|a| a.detects(folder)).ok_or_else(|| "no adapter matches the folder".to_string()),
    }
}

/// Runs the adapter (by name, or the detecting one) on `folder` within `limit`.
fn prune_here(adapters: &'static [Box<dyn CacheAdapter>], setting: Option<&str>, folder: &Path, limit: Duration) -> Report {
    let found = match find(adapters, setting, folder) {
        Ok(a) => a,
        Err(reason) => return Report { adapter: setting.unwrap_or("-").into(), pruned: false, freed: 0, restored: false, folder_ok: true, reason },
    };
    let host = WorkerHost { until: Instant::now() + limit };
    match found.prune(&host, folder) {
        Outcome::Pruned { freed } => Report { adapter: found.name().into(), pruned: true, freed, restored: false, folder_ok: true, reason: String::new() },
        Outcome::Skipped { reason } => Report { adapter: found.name().into(), pruned: false, freed: 0, restored: false, folder_ok: true, reason },
    }
}

/// Runs the adapter's `warm` on `folder` within `limit`. `pruned` in the report means the build ran.
fn warm_here(adapters: &'static [Box<dyn CacheAdapter>], setting: Option<&str>, folder: &Path, limit: Duration) -> Report {
    let found = match find(adapters, setting, folder) {
        Ok(a) => a,
        Err(reason) => return Report { adapter: setting.unwrap_or("-").into(), pruned: false, freed: 0, restored: false, folder_ok: true, reason },
    };
    let host = WorkerHost { until: Instant::now() + limit };
    let (pruned, reason) = match found.warm(&host, folder) {
        Ok(()) => (true, String::new()),
        Err(reason) => (false, reason),
    };
    Report { adapter: found.name().into(), pruned, freed: 0, restored: false, folder_ok: true, reason }
}

/// Runs the adapter's `recover` on `folder` (after its processes were stopped at the hard limit).
fn recover_here(adapters: &'static [Box<dyn CacheAdapter>], setting: Option<&str>, folder: &Path) -> Report {
    let found = match find(adapters, setting, folder) {
        Ok(a) => a,
        // Without an adapter nothing changed the folder under this hook.
        Err(_) => return Report { adapter: "-".into(), pruned: false, freed: 0, restored: false, folder_ok: true, reason: String::new() },
    };
    match found.recover(folder) {
        Ok(restored) => Report { adapter: found.name().into(), pruned: false, freed: 0, restored, folder_ok: true, reason: String::new() },
        Err(reason) => Report { adapter: found.name().into(), pruned: false, freed: 0, restored: false, folder_ok: false, reason },
    }
}

/// Runs the adapter's `relocated` on `folder`, used at `at`. `freed` in the report is the number of stale units.
fn relocate_here(adapters: &'static [Box<dyn CacheAdapter>], setting: Option<&str>, folder: &Path, at: &Path) -> Report {
    let found = match find(adapters, setting, folder) {
        Ok(a) => a,
        Err(reason) => return Report { adapter: setting.unwrap_or("-").into(), pruned: false, freed: 0, restored: false, folder_ok: true, reason },
    };
    let (freed, reason) = match found.relocated(folder, at) {
        Ok(n) => (n as u64, String::new()),
        Err(reason) => (0, reason),
    };
    Report { adapter: found.name().into(), pruned: false, freed, restored: false, folder_ok: true, reason }
}

#[cfg(test)]
mod tests {
    #[test]
    fn owner_path_has_the_homebrew_rustup_folder() {
        let p = super::owner_path(rustix::process::getuid().as_raw());
        assert!(p.split(':').any(|d| d == "/opt/homebrew/opt/rustup/bin"), "{p}");
    }

    use super::*;
    use std::path::PathBuf;

    /// Fake adapters. `ok` prunes, `fail` skips, `slow` sleeps past the limit, `slow-writes` sleeps past the
    /// limit and then writes a file. `hard-restore` damages the folder and sleeps past the limit; its `recover`
    /// puts back the copy that its prune made. `hard-stuck` does the same and cannot recover.
    struct Fake(&'static str);

    fn copy_of(folder: &Path) -> PathBuf {
        folder.with_file_name(format!("{}.hard-copy", folder.file_name().unwrap().to_string_lossy()))
    }

    impl CacheAdapter for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn detects(&self, _folder: &Path) -> bool {
            false
        }
        fn prune(&self, _host: &dyn Host, folder: &Path) -> Outcome {
            match self.0 {
                "fail" => Outcome::Skipped { reason: "stale".into() },
                "slow" => {
                    std::thread::sleep(Duration::from_secs(3));
                    Outcome::Pruned { freed: 0 }
                }
                "slow-writes" => {
                    std::thread::sleep(Duration::from_secs(3));
                    let _ = std::fs::write(folder.join("late.txt"), "written after the limit");
                    Outcome::Pruned { freed: 1 }
                }
                "hard-restore" | "hard-stuck" => {
                    if self.0 == "hard-restore" {
                        let _ = std::fs::write(copy_of(folder), "before");
                    }
                    let _ = std::fs::remove_file(folder.join("kept.txt"));
                    let _ = std::fs::write(folder.join("partial.txt"), "half");
                    std::thread::sleep(Duration::from_secs(3));
                    Outcome::Pruned { freed: 1 }
                }
                _ => Outcome::Pruned { freed: 7 },
            }
        }
        fn warm(&self, _host: &dyn Host, folder: &Path) -> Result<(), String> {
            match self.0 {
                "ok" => std::fs::write(folder.join("built.txt"), "built").map_err(|e| e.to_string()),
                "slow" => {
                    std::thread::sleep(Duration::from_secs(3));
                    Ok(())
                }
                _ => Err("no build".into()),
            }
        }
        fn recover(&self, folder: &Path) -> Result<bool, String> {
            match self.0 {
                "hard-restore" if copy_of(folder).exists() => {
                    let _ = std::fs::remove_file(folder.join("partial.txt"));
                    std::fs::write(folder.join("kept.txt"), std::fs::read(copy_of(folder)).unwrap()).map_err(|e| e.to_string())?;
                    std::fs::remove_file(copy_of(folder)).map_err(|e| e.to_string())?;
                    Ok(true)
                }
                "hard-stuck" => Err("the copy is gone".into()),
                _ => Ok(false),
            }
        }
    }

    fn fakes() -> &'static [Box<dyn CacheAdapter>] {
        Box::leak(Box::new(vec![
            Box::new(Fake("ok")) as Box<dyn CacheAdapter>,
            Box::new(Fake("fail")),
            Box::new(Fake("slow")),
            Box::new(Fake("slow-writes")),
            Box::new(Fake("hard-restore")),
            Box::new(Fake("hard-stuck")),
        ]))
    }

    /// The test binary as the worker: the test `worker_entry` runs the worker when the test env is set.
    fn launch() -> Launch {
        Launch { exe: std::env::current_exe().unwrap(), args: vec!["--exact".into(), "cache_hook::tests::worker_entry".into(), "--nocapture".into()] }
    }

    const TEST_ENV: [(&str, &str); 1] = [("LAYR_CACHE_HOOK_TEST", "1")];

    /// A fresh folder with one file, under the temp dir.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-cache-hook-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kept.txt"), "before").unwrap();
        dir
    }

    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// The worker, started with the fake adapters. The test env makes it run; otherwise it is a no-op test.
    #[test]
    fn worker_entry() {
        if std::env::var_os("LAYR_CACHE_HOOK_TEST").is_none() {
            return;
        }
        std::process::exit(worker_with(fakes()));
    }

    fn hook(setting: &str, dir: &Path, uid: u32, limit_ms: u64, wait_ms: u64) -> Done {
        run_with(&launch(), &TEST_ENV, Some(setting), dir, uid, Duration::from_millis(limit_ms), Duration::from_millis(wait_ms))
    }

    #[test]
    fn warm_builds_in_the_worker_and_a_timeout_keeps_the_folder_ok() {
        let dir = folder("warm");
        let ms = Duration::from_millis;
        let done = warm_with(&launch(), &TEST_ENV, Some("ok"), &dir, me(), ms(5000), ms(20000));
        assert!(done.log.contains("built with ok") && done.built, "{}", done.log);
        assert!(dir.join("built.txt").exists());
        let done = warm_with(&launch(), &TEST_ENV, Some("fail"), &dir, me(), ms(5000), ms(20000));
        assert!(done.log.contains("not built with fail: no build") && !done.built, "{}", done.log);
        let done = warm_with(&launch(), &TEST_ENV, Some("slow"), &dir, me(), ms(200), ms(500));
        assert!(done.log.contains("timed out") && done.folder_ok && !done.built, "{}", done.log);
    }

    #[test]
    fn a_named_adapter_prunes_and_its_outcome_is_logged() {
        let dir = folder("ok");
        let done = hook("ok", &dir, me(), 5000, 20000);
        assert!(done.log.contains("pruned with ok, freed 7 bytes"), "{}", done.log);
        assert!(done.folder_ok);
    }

    #[test]
    fn none_keeps_the_folder_and_an_unknown_name_is_logged() {
        let dir = folder("none");
        assert!(hook("none", &dir, me(), 5000, 20000).log.contains("prune is off"));
        assert!(hook("maven", &dir, me(), 5000, 20000).log.contains("unknown prune adapter 'maven'"));
    }

    #[test]
    fn a_failing_adapter_only_logs_its_reason() {
        let dir = folder("fail");
        let done = hook("fail", &dir, me(), 5000, 20000);
        assert!(done.log.contains("not pruned with fail: stale"), "{}", done.log);
        assert!(done.folder_ok);
    }

    /// Checked at the production entry, and silent: the root owner is never pruned, whatever user runs the tests.
    #[test]
    fn the_root_owner_is_never_pruned() {
        let dir = folder("root");
        let done = run(Some("ok"), &dir, 0, Duration::from_secs(5));
        assert!(done.log.is_empty() && done.folder_ok);
        assert_eq!(names(&dir), vec!["kept.txt".to_string()]);
    }

    /// A folder that no adapter applies to is left alone, silently, without starting a worker.
    #[test]
    fn a_folder_without_an_adapter_is_silent() {
        let dir = folder("silent");
        let done = run(None, &dir, me(), Duration::from_secs(5));
        assert!(done.log.is_empty() && done.folder_ok);
    }

    /// A worker that cannot start leaves the folder as it was: the folder is not marked damaged.
    #[test]
    fn a_worker_that_cannot_start_does_not_damage_the_folder() {
        let dir = folder("start");
        let launch = Launch { exe: PathBuf::from("/nonexistent/layr-worker"), args: vec![] };
        let done = run_with(&launch, &[], Some("ok"), &dir, me(), Duration::from_secs(5), Duration::from_secs(20));
        assert!(done.log.contains("cannot start the cache hook"), "{}", done.log);
        assert!(done.folder_ok);
    }

    /// The adapter sleeps past the limit: the parent stops the worker at the hard limit and reports a timeout.
    #[test]
    fn a_slow_adapter_is_stopped_at_the_time_limit() {
        let dir = folder("slow");
        let started = Instant::now();
        let done = hook("slow", &dir, me(), 100, 400);
        assert!(done.log.contains("timed out"), "{}", done.log);
        assert!(started.elapsed() < Duration::from_secs(2), "the hook waited for the adapter");
    }

    /// The adapter writes into the folder after the limit. The worker is killed at the hard limit, so the write
    /// never happens: the folder is unchanged after the timeout, also after the time the write would have taken.
    #[test]
    fn a_timed_out_adapter_writes_nothing_after_the_limit() {
        let dir = folder("writes");
        let done = hook("slow-writes", &dir, me(), 200, 500);
        assert!(done.log.contains("timed out"), "{}", done.log);
        std::thread::sleep(Duration::from_secs(4));
        assert_eq!(names(&dir), vec!["kept.txt".to_string()], "the folder changed after the timeout");
        assert_eq!(std::fs::read_to_string(dir.join("kept.txt")).unwrap(), "before");
    }

    /// Hard kill with a copy: the adapter is stopped while it damages the folder, and the recovery worker puts
    /// the copy back. The folder is as it was, and the outcome says it was restored.
    #[test]
    fn a_hard_kill_restores_the_folder_from_its_copy() {
        let dir = folder("hard-restore");
        let done = hook("hard-restore", &dir, me(), 200, 500);
        assert!(done.log.contains("timed out") && done.log.contains("processes stopped; restored from its copy"), "{}", done.log);
        assert!(done.folder_ok);
        assert_eq!(names(&dir), vec!["kept.txt".to_string()]);
        assert_eq!(std::fs::read_to_string(dir.join("kept.txt")).unwrap(), "before");
        assert!(!copy_of(&dir).exists());
    }

    /// Hard kill without a way back: the folder is reported damaged, so the caller marks it.
    #[test]
    fn a_hard_kill_that_cannot_restore_reports_the_folder_damaged() {
        let dir = folder("hard-stuck");
        let done = hook("hard-stuck", &dir, me(), 200, 500);
        assert!(!done.folder_ok, "{}", done.log);
        assert!(done.log.contains("marked damaged") && done.log.contains("the copy is gone"), "{}", done.log);
    }
}
