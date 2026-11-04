//! The cargo adapter. The `sweep` list of `[workspace.metadata.layr-retest]` names the standard build
//! commands. The listing of their units compiles nothing, and it is the live set that `sweep` keeps.
//! `prune` applies that to a `target` folder next to `Cargo.toml`. See docs/cache-overlays.md.

use crate::{kill_group, run, skip, sweep};
use anyhow::{anyhow, bail, Context, Result};
use layr_adapter::{CacheAdapter, Host, Outcome};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};

/// The scratch folder of the raw profiles of the instrumented build in `target`. It is deleted after
/// each run and after each sweep. The one owner of its path.
pub fn profile_scratch(target: &Path) -> PathBuf {
    target.join("profraw")
}

/// `LLVM_PROFILE_FILE` of every cargo call that retest makes with the instrumented flags. Cargo runs
/// build scripts in these calls (the listings of the sweep too), and a build script without this
/// variable writes `default_*.profraw` into its package folder, which is the project.
pub fn cargo_profile_file(target: &Path) -> PathBuf {
    profile_scratch(target).join("other/%p.profraw")
}

/// The one owner of the cargo command of the layr tools (layr-retest calls it too). Cargo's color comes from the
/// user's env (`CARGO_TERM_COLOR=always` puts escape codes in front of its status lines), and the parsers match
/// those lines at their start. So every cargo command sets the color to `never`. The listing sets it again after
/// the project's env (see `list_units`), so no project setting can change it.
fn with_color(mut c: Command) -> Command {
    c.env("CARGO_TERM_COLOR", "never");
    c
}

/// The cargo command of the layr tools on this machine.
pub fn cargo_cmd() -> Command {
    with_color(Command::new("cargo"))
}

/// The cargo command of an adapter: the host's command for cargo, with the same color.
pub(crate) fn host_cargo(host: &dyn Host) -> Result<Command> {
    Ok(with_color(host.command("cargo").map_err(|e| anyhow!("cannot run cargo: {e}"))?))
}

/// Settings of `[workspace.metadata.layr-retest]` in the root `Cargo.toml`: `sweep` lists the standard
/// build commands (cargo arguments without `cargo`), `retest` lists the commands of the retest build
/// (`target/retest`), and `env` is the build env of every cargo call of retest and of the gate.
/// `env` is the one owner of the project's build env: `run`, `sweep` and `gate` apply it.
pub struct Settings {
    pub env: Vec<(String, String)>,
    /// The `sweep` list is set. Without it the sweep does nothing.
    pub listed: bool,
    pub commands: Vec<Vec<String>>,
    pub retest: Vec<Vec<String>>,
}

/// The cargo commands under `key` (`sweep` or `retest`): an array of arrays of strings.
fn command_list(cfg: &Value, key: &str) -> Result<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    for c in cfg[key].as_array().into_iter().flatten() {
        let args = c.as_array().with_context(|| format!("{key}: each command is an array of strings"))?;
        let strings =
            args.iter().map(|a| a.as_str().map(String::from).with_context(|| format!("{key}: each argument is a string")));
        commands.push(strings.collect::<Result<Vec<_>>>()?);
    }
    Ok(commands)
}

/// Reads the retest settings of the workspace at `root`. Missing settings are empty.
pub fn settings(host: &dyn Host, root: &Path) -> Result<Settings> {
    let mut cmd = host_cargo(host)?;
    cmd.args(["metadata", "--no-deps", "--format-version", "1"]).current_dir(root);
    let meta: Value = serde_json::from_slice(&run(&mut cmd, Some(host.deadline())).context("cargo metadata")?)?;
    let cfg = &meta["metadata"]["layr-retest"];
    let mut env = Vec::new();
    for (k, v) in cfg["env"].as_object().into_iter().flatten() {
        env.push((k.clone(), v.as_str().context("env: each value is a string")?.to_string()));
    }
    Ok(Settings {
        env,
        listed: cfg["sweep"].as_array().is_some(),
        commands: command_list(cfg, "sweep")?,
        retest: command_list(cfg, "retest")?,
    })
}

/// The cargo arguments of a listing: `-v` names each unit that is not fresh before cargo compiles or runs
/// it, and `--message-format json` names the units. Both go before a `--` that ends the cargo arguments.
fn listing(args: &[String]) -> Vec<String> {
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut out = args.to_vec();
    out.splice(at..at, ["-v", "--message-format", "json"].map(String::from));
    out
}

/// What the listing of the units of one folder found.
pub enum Listed {
    /// Every unit is fresh: the live set of each profile folder.
    Fresh(sweep::Plans),
    /// A unit is not fresh, named by cargo's status line or by its target. Nothing was built past it.
    NotFresh(String),
    /// Another cargo holds the build lock of the folder: the listing stopped before it waited for that cargo.
    Busy,
}

/// Lists the units of `args` in the target folder `target` without building them. `CARGO_BUILD_JOBS=1`, and
/// the listing stops at the first unit that is not fresh. Cargo's `-v` prints `Dirty` for such a unit
/// before it compiles or runs anything, so the stop comes before a build step. The JSON `fresh: false` is
/// a second stop, for a cargo whose status text differs (cargo then builds that one unit). A `Blocking ...
/// directory` line means that another cargo holds a lock of the folder: the listing stops instead of waiting
/// for it. The listing runs in its own process group, and a stop kills the group. Any other failure is an error.
pub fn list_units(host: &dyn Host, root: &Path, target: &Path, env: &[(String, String)], args: &[String]) -> Result<Listed> {
    let mut c = host_cargo(host)?;
    c.args(listing(args)).current_dir(root).env("CARGO_TARGET_DIR", target).env("CARGO_BUILD_JOBS", "1");
    c.envs(env.iter().map(|(k, v)| (k, v))).env("LLVM_PROFILE_FILE", cargo_profile_file(target));
    // Set last: the project's env cannot turn the color back on (see `cargo_cmd`).
    let mut c = with_color(c);
    c.process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().with_context(|| format!("cannot start cargo {}", args.join(" ")))?;
    let (out, err) = (child.stdout.take().context("cargo stdout")?, child.stderr.take().context("cargo stderr")?);
    let pid = child.id();
    let stop: Mutex<Option<Listed>> = Mutex::new(None);
    let halt = |why: Listed| {
        let mut first = stop.lock().unwrap();
        if first.is_none() {
            *first = Some(why);
            kill_group(pid);
        }
    };
    // The time limit of the host: the group is killed at the deadline, and the listing is an error.
    let until = host.deadline();
    let late = std::sync::Arc::new(AtomicBool::new(false));
    let (done, waiting) = mpsc::channel::<()>();
    let timer_late = late.clone();
    let (lines, status) = std::thread::scope(|s| {
        s.spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = waiting.recv_timeout(until.saturating_duration_since(std::time::Instant::now())) {
                timer_late.store(true, Ordering::SeqCst);
                kill_group(pid);
            }
        });
        s.spawn(|| {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let t = line.trim_start();
                // A wait for a folder's lock stops the listing. A wait for the shared package cache is short: it waits.
                if t.starts_with("Blocking ") && t.contains("directory") {
                    halt(Listed::Busy);
                } else if ["Dirty ", "Compiling ", "Running "].iter().any(|p| t.starts_with(p)) {
                    halt(Listed::NotFresh(t.to_string()));
                }
            }
        });
        let lines = s.spawn(|| {
            let mut lines = Vec::new();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(name) = sweep::not_fresh(&line) {
                    halt(Listed::NotFresh(format!("{name} is not fresh")));
                }
                lines.push(line);
            }
            lines
        });
        let status = child.wait();
        drop(done);
        (lines.join().unwrap_or_default(), status)
    });
    let _ = std::fs::remove_dir_all(profile_scratch(target));
    if let Some(stopped) = stop.into_inner().unwrap_or(None) {
        return Ok(stopped);
    }
    if late.load(Ordering::SeqCst) {
        bail!("the cargo listing reached its time limit");
    }
    if !status.with_context(|| format!("cannot wait for cargo {}", args.join(" ")))?.success() {
        bail!("cargo {} failed", args.join(" "));
    }
    Ok(Listed::Fresh(sweep::plan(target, lines.join("\n").as_bytes())?))
}

/// The live units of the target folder `target`: the units that every command of `commands` uses, each
/// run with `env`. Stops at the first command whose units are not fresh.
pub fn live_units(host: &dyn Host, root: &Path, target: &Path, env: &[(String, String)], commands: &[Vec<String>]) -> Result<Listed> {
    let mut live = sweep::Plans::new();
    for args in commands {
        match list_units(host, root, target, env, args)? {
            Listed::Fresh(units) => sweep::merge(&mut live, units),
            stale => return Ok(stale),
        }
    }
    Ok(Listed::Fresh(live))
}

/// Sweeps each profile folder of `plans` under its cargo lock. Returns the freed bytes and the number of
/// profile folders skipped because their lock is busy. A dry run counts only.
pub fn sweep_plans(plans: &sweep::Plans, dry: bool) -> Result<(u64, usize)> {
    let (mut freed, mut busy) = (0, 0);
    for (profile, live) in plans {
        let n = if dry {
            Some(sweep::sweep(profile, live, true))
        } else {
            sweep::locked(profile, || sweep::sweep(profile, live, false))?
        };
        match n {
            Some(n) => freed += n,
            None => busy += 1,
        }
    }
    Ok((freed, busy))
}

/// Whether a build wrote a profile folder in `target` (`<target>/<profile>/.fingerprint`, any profile). The
/// listing names the profiles itself, so a folder with only a release or a custom profile is swept too.
pub fn built(target: &Path) -> bool {
    std::fs::read_dir(target).into_iter().flatten().flatten().any(|e| e.path().join(".fingerprint").is_dir())
}

/// The cargo adapter for `target`: removes the entries that the `sweep` list does not need. The folder must be
/// named `target` and have a `Cargo.toml` next to it. A unit that is not fresh, a busy build lock or any other
/// failure skips the folder. A busy lock skips only its profile folder: the others are already swept.
pub fn prune(host: &dyn Host, target: &Path) -> Outcome {
    let named = target.file_name().is_some_and(|n| n == "target");
    let Some(root) = target.parent().filter(|r| named && r.join("Cargo.toml").is_file()) else {
        return skip("cargo needs a folder named target next to Cargo.toml");
    };
    prune_in(host, root, target).unwrap_or_else(|e| skip(format!("{e:#}")))
}

/// The cargo adapter (`layr-adapter`): `target` next to `Cargo.toml`.
pub struct Cargo;

impl CacheAdapter for Cargo {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn detects(&self, folder: &Path) -> bool {
        folder.file_name().is_some_and(|n| n == "target") && folder.parent().is_some_and(|r| r.join("Cargo.toml").is_file())
    }

    fn prune(&self, host: &dyn Host, folder: &Path) -> Outcome {
        prune(host, folder)
    }
}

fn prune_in(host: &dyn Host, root: &Path, target: &Path) -> Result<Outcome> {
    let cfg = settings(host, root)?;
    if !cfg.listed {
        return Ok(skip("no `sweep` list under [workspace.metadata.layr-retest] in Cargo.toml"));
    }
    if !built(target) {
        return Ok(skip("target has no build output"));
    }
    // The listings must not reach the network: a unit whose crate is not in the cargo cache fails the listing.
    let mut env = cfg.env;
    env.push(("CARGO_NET_OFFLINE".to_string(), "true".to_string()));
    Ok(match live_units(host, root, target, &env, &cfg.commands)? {
        Listed::Fresh(live) => match sweep_plans(&live, false)? {
            (_, busy) if busy > 0 => skip(format!("build lock busy in {busy} profile folder(s)")),
            (freed, _) => Outcome::Pruned { freed },
        },
        Listed::NotFresh(why) => skip(format!("a unit is not fresh ({why}); run the standard build first")),
        Listed::Busy => skip("build lock busy"),
    })
}
