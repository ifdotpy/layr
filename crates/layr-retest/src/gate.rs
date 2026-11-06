//! `layr retest gate [--name <n>] [--max-load <n>] -- <command...>`: runs a gate command with one gate at a
//! time on this machine, under a low load, with the project's build env. Then it sweeps stale build output.
//! The command's exit status is the gate's exit status. Replaces a project wrapper script.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: layr retest gate [--name <n>] [--max-load <n>] -- <command...>";
/// Lock of the gates of this machine: a folder with a `pid` file and an `owner` file. `/tmp` is the same for
/// every process of the user. `LAYR_RETEST_GATE_LOCK` names another path (the tests use one).
const LOCK_VAR: &str = "LAYR_RETEST_GATE_LOCK";
const DEFAULT_LOCK: &str = "/tmp/layr-retest-gate.lock";
/// A gate waits only when the machine is heavily oversubscribed.
const DEFAULT_LOAD_PER_CPU: f64 = 4.0;

#[cfg(target_os = "linux")]
fn parse_online_cpus(text: &str) -> Option<usize> {
    let mut total = 0usize;
    let mut last = None;
    for part in text.trim().split(',') {
        let (start, end) = match part.split_once('-') {
            Some((start, end)) => (start.parse::<usize>().ok()?, end.parse::<usize>().ok()?),
            None => { let cpu = part.parse::<usize>().ok()?; (cpu, cpu) }
        };
        if end < start || last.is_some_and(|previous| start <= previous) { return None; }
        total = total.checked_add(end.checked_sub(start)?.checked_add(1)?)?;
        last = Some(end);
    }
    (total > 0).then_some(total)
}

fn online_cpus() -> usize {
    #[cfg(target_os = "linux")]
    if let Some(count) = std::fs::read_to_string("/sys/devices/system/cpu/online").ok().and_then(|s| parse_online_cpus(&s)) {
        return count;
    }
    // SAFETY: sysconf has no pointer arguments and does not modify Rust memory.
    let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    usize::try_from(count).ok().filter(|&n| n > 0).unwrap_or(1)
}

fn default_max_load() -> f64 {
    DEFAULT_LOAD_PER_CPU * online_cpus() as f64
}
/// The longest wait for a low load. After it the gate starts anyway and says so.
const MAX_LOAD_WAIT: Duration = Duration::from_secs(600);
/// How often a waiting gate checks the lock and the load.
const POLL: Duration = Duration::from_secs(2);

struct Options {
    name: String,
    max_load: f64,
    command: Vec<String>,
}

/// `None` when there is no `--` or no command after it.
fn parse(args: &[String]) -> Result<Option<Options>> {
    let Some(at) = args.iter().position(|a| a == "--") else { return Ok(None) };
    let command = args[at + 1..].to_vec();
    if command.is_empty() {
        return Ok(None);
    }
    let mut name = "gate".to_string();
    let mut max_load = default_max_load();
    let mut it = args[..at].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next().context("--name needs a name")?.clone(),
            "--max-load" => {
                max_load = it.next().context("--max-load needs a number")?.parse().context("--max-load needs a number")?
            }
            bad => bail!("unknown option {bad}; {USAGE}"),
        }
    }
    Ok(Some(Options { name, max_load, command }))
}

/// A gate lock held by this process. Released on drop, when its pid file still names this process.
struct Lock {
    dir: PathBuf,
    pid: u32,
}

impl Drop for Lock {
    fn drop(&mut self) {
        if read_pid(&self.dir) == Some(self.pid) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn read_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("pid")).ok()?.trim().parse().ok()
}

/// Whether the process `pid` exists (`kill -0`).
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Takes the lock at `path`. The lock folder appears in one step: a folder with the pid and the owner is
/// renamed to `path`, and the rename fails while `path` exists. Waits while a live process holds the lock,
/// and prints who holds it. A lock of a process that no longer exists is removed.
fn acquire(path: &Path, owner: &str) -> Result<Lock> {
    let pid = std::process::id();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let staging = path.with_file_name(format!("{name}.{pid}.tmp"));
    let mut last = String::new();
    loop {
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).with_context(|| format!("cannot create {}", staging.display()))?;
        std::fs::write(staging.join("pid"), pid.to_string())?;
        std::fs::write(staging.join("owner"), owner)?;
        if std::fs::rename(&staging, path).is_ok() {
            return Ok(Lock { dir: path.to_path_buf(), pid });
        }
        let _ = std::fs::remove_dir_all(&staging);
        if !path.exists() {
            bail!("cannot take the gate lock at {}", path.display());
        }
        let holder = read_pid(path);
        let who = std::fs::read_to_string(path.join("owner")).unwrap_or_default();
        match holder {
            Some(p) if alive(p) => {
                let msg = format!("layr-retest gate: waiting for the gate lock held by pid {p} ({})", who.trim());
                if msg != last {
                    eprintln!("{msg}");
                    last = msg;
                }
                std::thread::sleep(POLL);
            }
            _ => {
                let pid_text = holder.map_or("no pid".to_string(), |p| format!("pid {p}"));
                eprintln!("layr-retest gate: reaped the lock of {pid_text} ({})", who.trim());
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}

/// The 1-minute load average: `/proc/loadavg`, else `sysctl -n vm.loadavg`. `None` when unknown.
fn load1() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok().or_else(|| {
        let out = Command::new("sysctl").args(["-n", "vm.loadavg"]).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    })?;
    parse_load1(&text)
}

/// The first number of a load average text: `0.52 0.58 0.59 ...` or `{ 0.52 0.58 0.59 }`.
fn parse_load1(text: &str) -> Option<f64> {
    text.split(|c: char| c.is_whitespace() || c == '{' || c == '}').find(|w| !w.is_empty())?.parse().ok()
}

/// Waits while the load is at or above `limit`, for at most `MAX_LOAD_WAIT`. An unknown load does not wait.
fn wait_for_load(limit: f64) {
    let start = Instant::now();
    let mut said = false;
    while let Some(l) = load1().filter(|l| *l >= limit) {
        if start.elapsed() >= MAX_LOAD_WAIT {
            eprintln!("layr-retest gate: load {l:.1} is still at or above {limit} after 10 minutes; starting anyway");
            return;
        }
        if !said {
            eprintln!("layr-retest gate: waiting for the load to fall below {limit} (now {l:.1})");
            said = true;
        }
        std::thread::sleep(POLL);
    }
}

/// Applies the build env of a gate command: the project's `env`, with `RUSTC_WRAPPER` cleared and
/// `CARGO_TARGET_DIR` removed, so that cargo uses the target folder of the line.
fn with_env<'a>(cmd: &'a mut Command, env: &[(String, String)]) -> &'a mut Command {
    cmd.envs(env.iter().map(|(k, v)| (k, v))).env_remove("CARGO_TARGET_DIR")
}

/// The env of the gate: the project's `env` (none without `Cargo.toml`), and `RUSTC_WRAPPER` cleared.
fn gate_env(root: &Path) -> Result<Vec<(String, String)>> {
    let mut env = if root.join("Cargo.toml").is_file() {
        layr_cache::cargo::settings(&layr_cache::LocalHost::new(std::time::Duration::from_secs(3600)), root)?.env
    } else {
        Vec::new()
    };
    env.push(("RUSTC_WRAPPER".to_string(), String::new()));
    Ok(env)
}

fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// `layr retest sweep` of this project with the gate env. Its output goes to stderr. Returns `ok`, `skipped`
/// (a folder was skipped, or no `sweep` list) or `failed`. It never changes the exit status of the gate.
fn sweep(env: &[(String, String)]) -> &'static str {
    let Ok(exe) = std::env::current_exe() else { return "failed" };
    let out = match with_env(Command::new(exe).args(["retest", "sweep"]), env).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("layr-retest gate: cannot start the sweep: {e}");
            return "failed";
        }
    };
    eprint!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        "failed"
    } else if String::from_utf8_lossy(&out.stderr).contains("sweep skipped") {
        "skipped"
    } else {
        "ok"
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Entry point of `layr retest gate`. Returns the exit status of the command.
pub fn run(args: &[String]) -> Result<i32> {
    let Some(opts) = parse(args)? else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    let root = std::env::current_dir()?;
    let lock_path = std::env::var_os(LOCK_VAR).map_or_else(|| PathBuf::from(DEFAULT_LOCK), PathBuf::from);
    let line = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let owner = format!("{} line={line} since={}", opts.name, now_secs());
    let _lock = acquire(&lock_path, &owner)?;
    wait_for_load(opts.max_load);

    let cargo = root.join("Cargo.toml").is_file();
    let env = gate_env(&root)?;
    if cargo {
        match with_env(layr_cache::cargo::cargo_cmd().args(["fetch", "--locked"]), &env).status() {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("layr-retest gate: cargo fetch failed ({s}); the command may miss new crates"),
            Err(e) => eprintln!("layr-retest gate: cannot start cargo fetch: {e}; the command may miss new crates"),
        }
    }
    let (prog, rest) = opts.command.split_first().context(USAGE)?;
    let status = with_env(Command::new(prog).args(rest), &env).status().with_context(|| format!("cannot start {prog}"))?;
    let code = exit_code(status);
    let sweep = if cargo { sweep(&env) } else { "skipped" };
    eprintln!("layr-retest gate {}: exit {code}, sweep {sweep}", opts.name);
    Ok(code)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn parses_online_cpu_ranges() {
        assert_eq!(super::parse_online_cpus("0-15,17,20-23\n"), Some(21));
        assert_eq!(super::parse_online_cpus("0"), Some(1));
        for invalid in ["", "2-1", "0-2,2", "a", "1-", "1,,2"] {
            assert_eq!(super::parse_online_cpus(invalid), None);
        }
    }

    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_command_follows_the_separator() {
        let o = parse(&args(&["--name", "test-fast", "--", "just", "test-fast"])).unwrap().unwrap();
        assert_eq!((o.name.as_str(), o.max_load, o.command), ("test-fast", default_max_load(), args(&["just", "test-fast"])));
        // A later separator belongs to the command.
        assert_eq!(
            parse(&args(&["--", "cargo", "test", "--", "x"])).unwrap().unwrap().command,
            args(&["cargo", "test", "--", "x"])
        );
    }

    #[test]
    fn no_separator_or_no_command_is_usage() {
        assert!(parse(&args(&["--name", "x"])).unwrap().is_none());
        assert!(parse(&args(&["--"])).unwrap().is_none());
        assert!(parse(&args(&["--bogus", "--", "true"])).is_err());
    }

    #[test]
    fn reads_the_one_minute_load() {
        assert_eq!(parse_load1("{ 23.27 18.66 9.99 }\n"), Some(23.27));
        assert_eq!(parse_load1("0.52 0.58 0.59 1/800 123\n"), Some(0.52));
        assert_eq!(parse_load1(""), None);
    }
}
