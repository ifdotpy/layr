//! The cargo adapter. The `sweep` list of `[workspace.metadata.layr-retest]` names the standard build
//! commands. The listing of their units compiles nothing, and it is the live set that `sweep` keeps.
//! `prune` applies that to a `target` folder next to `Cargo.toml`. See docs/cache-overlays.md.

use crate::{kill_group, run, skip, sweep, wait_child};
use anyhow::{anyhow, bail, Context, Result};
use layr_adapter::{CacheAdapter, Host, Outcome, Toolchain};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

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

/// `cargo metadata --no-deps` of the workspace at `root`. The host's time limit applies.
pub fn metadata(host: &dyn Host, root: &Path) -> Result<Value> {
    let mut cmd = host_cargo(host)?;
    cmd.args(["metadata", "--no-deps", "--format-version", "1"]).current_dir(root);
    Ok(serde_json::from_slice(&run(&mut cmd, Some(host.deadline())).context("cargo metadata")?)?)
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
    /// `sweep` entries that build into a folder of their own below `target` (`{"target_dir", "args", "env"}`): a test
    /// that sets CARGO_TARGET_DIR to `target/<dir>` with another env gets that folder warm and pruned too.
    pub other_dirs: Vec<OtherDir>,
}

/// A `sweep` entry with its own target folder: `{"target_dir": "<folder below target>", "args": [...], "env": {...}}`.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherDir {
    pub dir: String,
    pub args: Vec<String>,
    /// Over the project env (`env` of the table).
    pub env: Vec<(String, String)>,
}

/// The `sweep` entries that are objects (`OtherDir`).
fn other_dirs(cfg: &Value) -> Result<Vec<OtherDir>> {
    let mut out = Vec::new();
    for c in cfg["sweep"].as_array().into_iter().flatten().filter(|c| c.is_object()) {
        let dir = c["target_dir"].as_str().context("sweep: an object entry needs target_dir")?.to_string();
        if dir.is_empty() || dir.starts_with('/') || dir.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
            bail!("sweep: target_dir must be a folder below target");
        }
        let args = c["args"]
            .as_array()
            .context("sweep: an object entry needs args")?
            .iter()
            .map(|a| a.as_str().map(String::from).context("sweep: each argument is a string"))
            .collect::<Result<Vec<_>>>()?;
        let mut env = Vec::new();
        for (k, v) in c["env"].as_object().into_iter().flatten() {
            env.push((k.clone(), v.as_str().context("sweep: each env value is a string")?.to_string()));
        }
        out.push(OtherDir { dir, args, env });
    }
    Ok(out)
}

/// The cargo commands under `key` (`sweep` or `retest`): an array of arrays of strings. An object entry of `sweep`
/// is an `OtherDir`, read by `other_dirs`.
fn command_list(cfg: &Value, key: &str) -> Result<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    for c in cfg[key].as_array().into_iter().flatten().filter(|c| !(key == "sweep" && c.is_object())) {
        let args = c.as_array().with_context(|| format!("{key}: each command is an array of strings"))?;
        let strings =
            args.iter().map(|a| a.as_str().map(String::from).with_context(|| format!("{key}: each argument is a string")));
        commands.push(strings.collect::<Result<Vec<_>>>()?);
    }
    Ok(commands)
}

/// The environment variables that change cargo's unit hashes or the code that rustc and build scripts make
/// (docs/cache-overlays.md, section 6).
pub const KEY_ENV: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_INCREMENTAL",
    "CARGO_PROFILE_DEV_DEBUG",
    "CARGO_PROFILE_RELEASE_DEBUG",
    "CARGO_BUILD_TARGET",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "AR",
];

/// The names of the per-OS tables of the `env` table (`std::env::consts::OS`).
const OS_TABLES: [&str; 2] = ["macos", "linux"];

/// The `env` table of `[workspace.metadata.layr-retest]` in `<root>/Cargo.toml`, read without cargo: the env that
/// retest and the gate pin for every cargo call. Empty when the file or the table is missing or bad.
pub fn pinned_env(root: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) else { return Vec::new() };
    let Ok(doc) = text.parse::<toml::Table>() else { return Vec::new() };
    let env = doc.get("workspace").and_then(|w| w.get("metadata")).and_then(|m| m.get("layr-retest")).and_then(|r| r.get("env"));
    let Some(table) = env.and_then(|e| e.as_table()) else { return Vec::new() };
    let strings = |t: &toml::Table| t.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect::<Vec<_>>();
    let mut out = strings(table);
    // `macos = { ... }` and `linux = { ... }` apply on that OS only, over the plain values.
    if let Some(own) = table.get(std::env::consts::OS).and_then(|v| v.as_table()) {
        out.extend(strings(own));
    }
    out
}

/// Reads the retest settings of the workspace at `root`. Missing settings are empty. A missing `env` is empty too:
/// the listing then runs cargo with the default env of a build. A default that the build does not use changes the
/// unit hashes (release is not incremental by default), so `init` writes the env into the manifest instead.
pub fn settings(host: &dyn Host, root: &Path) -> Result<Settings> {
    let meta = metadata(host, root)?;
    let cfg = &meta["metadata"]["layr-retest"];
    let entries = |obj: &serde_json::Map<String, serde_json::Value>| -> Result<Vec<(String, String)>> {
        obj.iter()
            .filter(|(k, v)| !(v.is_object() && OS_TABLES.contains(&k.as_str())))
            .map(|(k, v)| Ok((k.clone(), v.as_str().context("env: each value is a string")?.to_string())))
            .collect()
    };
    let mut env: Vec<(String, String)> = match cfg["env"].as_object() {
        Some(obj) => {
            let mut env = entries(obj)?;
            // `macos = { ... }` and `linux = { ... }` apply on that OS only, over the plain values.
            if let Some(own) = obj.get(std::env::consts::OS).and_then(|v| v.as_object()) {
                env.extend(entries(own)?);
            }
            env
        }
        None => Vec::new(),
    };
    Ok(Settings {
        env,
        listed: cfg["sweep"].as_array().is_some(),
        commands: command_list(cfg, "sweep")?,
        retest: command_list(cfg, "retest")?,
        other_dirs: other_dirs(cfg)?,
    })
}

/// The `RUSTC_WRAPPER` of a listing: it refuses every compile, so a listing never builds. Cargo asks the wrapper
/// for the version and the target information (they pass through), and a compile emits outputs (`--emit`), which it
/// refuses. Cargo prints `Dirty` before it compiles a unit, but a unit before the stale one can still compile before
/// the listing stops; the wrapper stops that too. The wrapper is not part of a unit's hash (a fresh unit stays fresh).
fn no_compile_wrapper() -> Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    const SCRIPT: &str = "#!/bin/sh\ncase \" $* \" in *\" --emit\"*) exit 1 ;; esac\nexec \"$@\"\n";
    let path = std::env::temp_dir().join(format!("layr-nocompile-{}.sh", rustix::process::getuid().as_raw()));
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SCRIPT) {
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, SCRIPT).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
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
    /// The text names the process that holds the lock (see `lock_holder`).
    Busy(String),
}

/// The process that holds the lock file `lock`, named by pid and command. `except` is the pid of the waiting
/// cargo, which also has the file open. `lsof` finds the holder on macOS and Linux, and `/proc/locks` on Linux.
/// The text says `unknown` when neither finds a process.
pub fn lock_holder(lock: &Path, except: u32) -> String {
    let mut pids: std::collections::BTreeSet<u32> = Default::default();
    if let Ok(o) = Command::new("lsof").arg("-t").arg(lock).stderr(Stdio::null()).output() {
        pids.extend(String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    pids.extend(proc_locks(lock));
    pids.remove(&except);
    if pids.is_empty() {
        return format!("unknown (lsof and /proc/locks name no process for {})", lock.display());
    }
    let named: Vec<String> = pids.iter().map(|p| format!("pid {p} ({})", command_name(*p))).collect();
    named.join(", ")
}

/// The pids that hold a flock on the inode of `lock`, from `/proc/locks`. Empty without that file.
fn proc_locks(lock: &Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    let Ok(ino) = std::fs::metadata(lock).map(|m| m.ino()) else { return Vec::new() };
    let table = std::fs::read_to_string("/proc/locks").unwrap_or_default();
    table
        .lines()
        .filter_map(|l| {
            // `1: FLOCK  ADVISORY  WRITE 1234 08:01:131 0 EOF`: the pid, then major:minor:inode.
            let f: Vec<&str> = l.split_whitespace().collect();
            let (pid, id) = (f.get(4)?, f.get(5)?);
            let inode: u64 = id.rsplit(':').next()?.parse().ok()?;
            (inode == ino).then(|| pid.parse().ok()).flatten()
        })
        .collect()
}

/// The command name of `pid`, or `?`.
fn command_name(pid: u32) -> String {
    let out = Command::new("ps").args(["-o", "comm=", "-p", &pid.to_string()]).output().ok();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into())
}

/// The cargo home of a command: the last `CARGO_HOME` of its env, else the env of this process, else `~/.cargo`.
fn cargo_home(env: &[(String, String)]) -> PathBuf {
    let set = |v: &str| (!v.is_empty()).then(|| PathBuf::from(v));
    env.iter()
        .rev()
        .find(|(k, _)| k == "CARGO_HOME")
        .and_then(|(_, v)| set(v))
        .or_else(|| std::env::var("CARGO_HOME").ok().and_then(|v| set(&v)))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// Records `why` as the stop of a listing, unless an earlier stop came first, and kills the group.
fn halt(stop: &Mutex<Option<Listed>>, pid: u32, why: Listed) {
    let mut first = stop.lock().unwrap();
    if first.is_none() {
        *first = Some(why);
        kill_group(pid);
    }
}

/// Lists the units of `args` in the target folder `target` without building them. `CARGO_BUILD_JOBS=1`, and
/// the listing stops at the first unit that is not fresh. Cargo's `-v` prints `Dirty` for such a unit
/// before it compiles or runs anything, so the stop comes before a build step. The JSON `fresh: false` is
/// a second stop, for a cargo whose status text differs (cargo then builds that one unit). A `Blocking ...
/// directory` line means that another cargo holds a lock of the folder: the listing stops instead of waiting
/// for it, and names the holder. Other waits (the package cache) are waited for, and the time limit of the
/// host ends them: the group is killed and the listing is an error that names the holder. The listing runs in
/// its own process group. Any other failure is an error.
pub fn list_units(host: &dyn Host, root: &Path, target: &Path, env: &[(String, String)], args: &[String]) -> Result<Listed> {
    let mut c = host_cargo(host)?;
    c.args(listing(args)).current_dir(root).env("CARGO_TARGET_DIR", target).env("CARGO_BUILD_JOBS", "1");
    c.envs(env.iter().map(|(k, v)| (k, v))).env("LLVM_PROFILE_FILE", cargo_profile_file(target));
    // Set after the project's env: the listing must not compile, whatever the project's wrapper is.
    c.env("RUSTC_WRAPPER", no_compile_wrapper()?);
    // Set last: the project's env cannot turn the color back on (see `cargo_cmd`).
    let mut c = with_color(c);
    c.process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().with_context(|| format!("cannot start cargo {}", args.join(" ")))?;
    let pid = child.id();
    let (out, err) = (child.stdout.take().context("cargo stdout")?, child.stderr.take().context("cargo stderr")?);
    let stop: Arc<Mutex<Option<Listed>>> = Arc::default();
    let waited: Arc<Mutex<Option<String>>> = Arc::default();
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let (errs_tx, errs_rx) = std::sync::mpsc::channel::<()>();
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let lines: Vec<String> = BufReader::new(out)
                .lines()
                .map_while(Result::ok)
                .inspect(|line| {
                    if let Some(name) = sweep::not_fresh(line) {
                        halt(&stop, pid, Listed::NotFresh(format!("{name} is not fresh")));
                    }
                })
                .collect();
            let _ = lines_tx.send(lines);
        });
    }
    {
        let (stop, waited) = (stop.clone(), waited.clone());
        let lock_dir = target.join(profile_of(args));
        let home = cargo_home(env);
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let t = line.trim_start();
                if t.starts_with("Blocking ") && t.contains("directory") {
                    let holder = lock_holder(&lock_dir.join(".cargo-lock"), pid);
                    halt(&stop, pid, Listed::Busy(holder));
                } else if t.starts_with("Blocking ") {
                    let holder = if t.contains("package cache") {
                        lock_holder(&home.join(".package-cache"), pid)
                    } else {
                        "unknown (layr does not know this lock)".to_string()
                    };
                    eprintln!("layr-retest: cargo waits: {t}; holder: {holder}");
                    *waited.lock().unwrap() = Some(format!("{t}; holder: {holder}"));
                } else if ["Dirty ", "Compiling ", "Running "].iter().any(|p| t.starts_with(p)) {
                    halt(&stop, pid, Listed::NotFresh(t.to_string()));
                }
            }
            let _ = errs_tx.send(());
        });
    }
    let (status, late) = wait_child(&mut child, Some(host.deadline())).with_context(|| format!("cannot wait for cargo {}", args.join(" ")))?;
    let _ = std::fs::remove_dir_all(profile_scratch(target));
    if let Some(stopped) = stop.lock().unwrap().take() {
        return Ok(stopped);
    }
    if late {
        let why = waited.lock().unwrap().take().unwrap_or_default();
        bail!("the cargo listing of {} reached its time limit while it waited: {why}", args.join(" "));
    }
    // The readers finish before the stop is final: a line that was read after the exit can still stop the listing.
    let lines = lines_rx.recv_timeout(std::time::Duration::from_secs(5)).ok();
    let errs_done = errs_rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();
    if let Some(stopped) = stop.lock().unwrap().take() {
        return Ok(stopped);
    }
    let (Some(lines), true) = (lines, errs_done) else {
        bail!("the cargo listing of {} kept its output open; a process outside its group holds it", args.join(" "));
    };
    if !status.success() {
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

/// The profile folder that a cargo command builds into: `--profile <name>` (`dev` and `test` are `debug`, `bench`
/// is `release`), `--release` is `release`, and the default is `debug`. Only the cargo arguments count (before `--`).
pub fn profile_of(args: &[String]) -> String {
    let own = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    let named = own.iter().position(|a| a == "--profile").and_then(|i| own.get(i + 1)).cloned().or_else(|| {
        own.iter().find_map(|a| a.strip_prefix("--profile=")).map(String::from)
    });
    match named.as_deref() {
        Some("dev") | Some("test") => "debug".to_string(),
        Some("bench") => "release".to_string(),
        Some(p) => p.to_string(),
        None if own.iter().any(|a| a == "--release") => "release".to_string(),
        None => "debug".to_string(),
    }
}

/// The live units of each profile folder, listed on its own: the commands of one profile name only that profile's
/// units. A profile whose command is not fresh is `NotFresh` (its folder is kept), and the other profiles still
/// count, so a stale release command does not block the debug folder.
pub fn live_by_profile(host: &dyn Host, root: &Path, target: &Path, env: &[(String, String)], commands: &[Vec<String>]) -> Result<Vec<(String, Vec<Vec<String>>, Listed)>> {
    let mut groups: std::collections::BTreeMap<String, Vec<Vec<String>>> = std::collections::BTreeMap::new();
    for args in commands {
        groups.entry(profile_of(args)).or_default().push(args.clone());
    }
    let mut out = Vec::new();
    for (profile, cmds) in groups {
        let listed = live_units(host, root, target, env, &cmds)?;
        out.push((profile, cmds, listed));
    }
    Ok(out)
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

/// A top-level folder of `target` that no `sweep` command makes (another `--target` triple, a custom profile, `doc`)
/// goes when no build wrote into it for this time.
const UNLISTED_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// The top-level folders of `target` that the `sweep` commands make: their profile folders and their `--target`
/// triples. `retest` (layr-retest) and the profile scratch folder are kept too.
fn listed_dirs(commands: &[Vec<String>]) -> std::collections::BTreeSet<String> {
    let mut out: std::collections::BTreeSet<String> = ["retest".to_string()].into();
    out.extend(profile_scratch(Path::new("")).file_name().map(|n| n.to_string_lossy().into_owned()));
    for args in commands {
        out.insert(profile_of(args));
        let own = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
        for (i, a) in own.iter().enumerate() {
            if a == "--target" {
                out.extend(own.get(i + 1).cloned());
            } else if let Some(t) = a.strip_prefix("--target=") {
                out.insert(t.to_string());
            }
        }
    }
    out
}

/// The newest modification time of `dir`, its entries and their entries. A build writes there (cargo's
/// `.cargo-lock` and `.fingerprint`, the files of `doc`), so it tells when a build last used the folder.
fn newest_write(dir: &Path) -> std::time::SystemTime {
    let time = |p: &Path| std::fs::symlink_metadata(p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
    let mut newest = time(dir);
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        newest = newest.max(time(&e.path()));
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            for f in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                newest = newest.max(time(&f.path()));
            }
        }
    }
    newest
}

/// The top-level folders of `target` that no listed command makes and that no build wrote into for
/// `UNLISTED_AGE` before `now`. Files and symbolic links at the top level stay.
pub fn stale_unlisted(target: &Path, commands: &[Vec<String>], now: std::time::SystemTime) -> Vec<PathBuf> {
    stale_unlisted_but(target, commands, &[], now)
}

/// `stale_unlisted`, keeping the top-level folders of `other` (the `target_dir` of `OtherDir` entries) too.
pub fn stale_unlisted_but(target: &Path, commands: &[Vec<String>], other: &[OtherDir], now: std::time::SystemTime) -> Vec<PathBuf> {
    let mut listed = listed_dirs(commands);
    listed.extend(other.iter().filter_map(|o| o.dir.split('/').next().map(String::from)));
    let mut out = Vec::new();
    for e in std::fs::read_dir(target).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if listed.contains(&name) || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if now.duration_since(newest_write(&e.path())).is_ok_and(|age| age >= UNLISTED_AGE) {
            out.push(e.path());
        }
    }
    out.sort();
    out
}

/// The bytes of the files under `dir` (symbolic links are not followed).
fn size_of(dir: &Path) -> u64 {
    let mut n = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => n += size_of(&e.path()),
            Ok(t) if t.is_file() => n += e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => {}
        }
    }
    n
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

    fn warm(&self, host: &dyn Host, folder: &Path) -> Result<(), String> {
        warm(host, folder).map_err(|e| format!("{e:#}"))
    }

    fn key_env(&self) -> &'static [&'static str] {
        KEY_ENV
    }

    fn pinned_env(&self, folder: &Path) -> Vec<(String, String)> {
        folder.parent().map(pinned_env).unwrap_or_default()
    }

    fn toolchain_command(&self) -> Option<Toolchain> {
        // rustup takes the toolchain from these files (or its default); `rustc -vV` names the exact build.
        Some(Toolchain { program: "rustc", args: &["-vV"], files: &["rust-toolchain.toml", "rust-toolchain"] })
    }

    fn unit_of(&self, path: &str) -> Option<String> {
        unit_of(path)
    }

    fn relocated(&self, folder: &Path, at: &Path) -> Result<usize, String> {
        crate::relocate::relocate(folder, at).map_err(|e| format!("{e:#}"))
    }

    fn relocation_note(&self, folder: &Path) -> Option<String> {
        let old = crate::relocate::foreign_registry(folder)?;
        Some(format!(
            "built with the cargo registry at {}, which this machine does not have: cargo builds every registry crate \
             again. Use one CARGO_HOME path on every machine (docs/cache-overlays.md)",
            old.display()
        ))
    }
}

/// The unit of a file in a cargo target folder: `<profile path>/<name>-<hash>` for the files of `deps/`
/// (`lib<name>-<hash>.rlib`, `.rmeta`, `.d`, test binaries), `.fingerprint/<name>-<hash>/`, `build/<name>-<hash>/`
/// and `incremental/<name>-<hash>/`. The hash is cargo's 16 hex digits; other paths have no unit.
pub fn unit_of(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let at = parts.iter().position(|p| matches!(*p, "deps" | ".fingerprint" | "build" | "incremental"))?;
    let entry = parts.get(at + 1)?;
    let stem = if parts[at] == "deps" {
        let s = entry.strip_prefix("lib").filter(|s| s.contains('-')).unwrap_or(entry);
        s.split('.').next().unwrap_or(s)
    } else {
        entry
    };
    let (name, hash) = stem.rsplit_once('-')?;
    if name.is_empty() || hash.len() != 16 || !hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        return None;
    }
    Some(format!("{}/{name}-{hash}", parts[..at].join("/")))
}

/// Builds the `sweep` commands of the project into `target`, with the project env. It may fetch crates (a merged
/// change can add a dependency). A failed build leaves `target` consistent: cargo writes each output whole. Every
/// command runs, also after one fails: a crate that does not compile leaves only the units of that command cold,
/// not the other profiles and target folders. The error names each failed command.
pub fn warm(host: &dyn Host, target: &Path) -> Result<()> {
    let named = target.file_name().is_some_and(|n| n == "target");
    let Some(root) = target.parent().filter(|r| named && r.join("Cargo.toml").is_file()) else {
        bail!("cargo needs a folder named target next to Cargo.toml");
    };
    let cfg = settings(host, root)?;
    if !cfg.listed {
        bail!("no `sweep` list under [workspace.metadata.layr-retest] in Cargo.toml");
    }
    let mut failed = Vec::new();
    for args in &cfg.commands {
        let mut c = host_cargo(host)?;
        c.args(keep_going(args)).current_dir(root).env("CARGO_TARGET_DIR", target).envs(cfg.env.iter().map(|(k, v)| (k, v)));
        // The color again after the project's env: the parsers of later steps read plain text.
        c.env("CARGO_TERM_COLOR", "never");
        if let Err(e) = run(&mut c, Some(host.deadline())) {
            failed.push(format!("cargo {}: {e:#}", args.join(" ")));
        }
    }
    for other in &cfg.other_dirs {
        let mut c = host_cargo(host)?;
        c.args(keep_going(&other.args))
            .current_dir(root)
            .env("CARGO_TARGET_DIR", target.join(&other.dir))
            .envs(cfg.env.iter().chain(other.env.iter()).map(|(k, v)| (k, v)));
        c.env("CARGO_TERM_COLOR", "never");
        if let Err(e) = run(&mut c, Some(host.deadline())) {
            failed.push(format!("cargo {} (target/{}): {e:#}", other.args.join(" "), other.dir));
        }
    }
    if !failed.is_empty() {
        bail!("{}", failed.join("\n"));
    }
    Ok(())
}

/// The arguments of a warm command: `build`, `check`, `clippy` and `doc` get `--keep-going`, so a crate that
/// does not compile leaves only itself and its dependents cold. The flag changes no unit hash. Other commands
/// (such as `nextest list`) take their arguments as they are.
fn keep_going(args: &[String]) -> Vec<String> {
    let mut out = args.to_vec();
    if matches!(args.first().map(String::as_str), Some("build" | "check" | "clippy" | "doc")) && !args.iter().any(|a| a == "--keep-going") {
        // Before a `--` that passes the rest to a tool (clippy lints).
        let at = out.iter().position(|a| a == "--").unwrap_or(out.len());
        out.insert(at, "--keep-going".to_string());
    }
    out
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
    // Each profile folder is pruned on its own: a stale command of one profile keeps only that folder.
    let mut freed = 0;
    let mut busy = 0;
    let mut fresh = 0;
    let mut kept = Vec::new();
    for (profile, _, listed) in live_by_profile(host, root, target, &env, &cfg.commands)? {
        match listed {
            Listed::Fresh(live) => {
                fresh += 1;
                let (n, b) = sweep_plans(&live, false)?;
                freed += n;
                busy += b;
            }
            Listed::NotFresh(why) => {
                // A registry source at another path is another CARGO_HOME: name the one that the listing used.
                let home = match why.contains("the path to the source changed") {
                    true => format!("; the listing used CARGO_HOME {}", cargo_home(&env).display()),
                    false => String::new(),
                };
                kept.push(format!("{profile}: a unit is not fresh ({why}{home})"))
            }
            Listed::Busy(holder) => kept.push(format!("{profile}: build lock busy (holder: {holder})")),
        }
    }
    // A folder of an `OtherDir` entry has the live set of its own command and env. A folder that a test never
    // built has nothing to prune.
    for other in &cfg.other_dirs {
        let dir = target.join(&other.dir);
        if !built(&dir) {
            continue;
        }
        let mut own = env.clone();
        own.extend(other.env.iter().cloned());
        for (profile, _, listed) in live_by_profile(host, root, &dir, &own, std::slice::from_ref(&other.args))? {
            match listed {
                Listed::Fresh(live) => {
                    let (n, b) = sweep_plans(&live, false)?;
                    freed += n;
                    busy += b;
                }
                Listed::NotFresh(why) => kept.push(format!("{}/{profile}: a unit is not fresh ({why})", other.dir)),
                Listed::Busy(holder) => kept.push(format!("{}/{profile}: build lock busy (holder: {holder})", other.dir)),
            }
        }
    }
    // The folders that no listed command makes have no live set: an old one goes (issue #195).
    for dir in stale_unlisted_but(target, &cfg.commands, &cfg.other_dirs, std::time::SystemTime::now()) {
        let n = size_of(&dir);
        if std::fs::remove_dir_all(&dir).is_ok() {
            freed += n;
        }
    }
    if fresh == 0 {
        return Ok(skip(format!("no profile folder is fresh: {}; run the standard build first", kept.join("; "))));
    }
    // A busy lock keeps its profile folder (the others are swept), as the module doc says.
    let _ = busy;
    Ok(Outcome::Pruned { freed })
}

#[cfg(test)]
mod pinned_env_tests {
    use super::{pinned_env, unit_of};

    #[test]
    fn the_files_of_one_unit_have_one_unit() {
        let u = Some("target/debug/foo-0123456789abcdef".to_string());
        assert_eq!(unit_of("target/debug/deps/libfoo-0123456789abcdef.rlib"), u);
        assert_eq!(unit_of("target/debug/deps/libfoo-0123456789abcdef.rmeta"), u);
        assert_eq!(unit_of("target/debug/deps/foo-0123456789abcdef.d"), u);
        assert_eq!(unit_of("target/debug/deps/foo-0123456789abcdef"), u);
        assert_eq!(unit_of("target/debug/.fingerprint/foo-0123456789abcdef/lib-foo.json"), u);
        assert_eq!(unit_of("target/debug/deps/foo-0123456789abcdee.d"), Some("target/debug/foo-0123456789abcdee".into()));
        assert_eq!(unit_of("node_modules/x/build/index.js"), None);
        assert_eq!(unit_of("target/debug/foo"), None);
    }

    #[test]
    fn the_table_of_this_os_applies_over_the_plain_values() {
        let root = std::env::temp_dir().join(format!("layr-pinned-env-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace.metadata.layr-retest]\nenv = { CARGO_INCREMENTAL = \"1\", CC = \"cc\", macos = { CC = \"mac-cc\", SDKROOT = \"sdk\" }, linux = { CC = \"linux-cc\" } }\n",
        )
        .unwrap();
        let env = pinned_env(&root);
        let last = |k: &str| env.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(last("CARGO_INCREMENTAL"), Some("1"));
        let own = if cfg!(target_os = "macos") { "mac-cc" } else { "linux-cc" };
        assert_eq!(last("CC"), Some(own));
        assert_eq!(last("SDKROOT"), cfg!(target_os = "macos").then_some("sdk"));
        assert!(!env.iter().any(|(n, _)| n == "macos" || n == "linux"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

#[cfg(test)]
mod other_dir_tests {
    use super::*;

    #[test]
    fn an_object_entry_of_sweep_is_a_target_folder_of_its_own() {
        let cfg = serde_json::json!({"sweep": [
            ["build", "--workspace"],
            {"target_dir": "provider-contract-compile", "args": ["check", "--offline", "-p", "studio-runtime-schema"], "env": {"RUSTFLAGS": ""}}
        ]});
        assert_eq!(command_list(&cfg, "sweep").unwrap(), vec![vec!["build".to_string(), "--workspace".to_string()]]);
        let other = other_dirs(&cfg).unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].dir, "provider-contract-compile");
        assert_eq!(other[0].env, vec![("RUSTFLAGS".to_string(), String::new())]);
        for bad in ["", "/abs", "a/../b", "."] {
            let cfg = serde_json::json!({"sweep": [{"target_dir": bad, "args": []}]});
            assert!(other_dirs(&cfg).is_err(), "{bad:?} is refused");
        }
        // The prune keeps the folder of an entry, however old it is.
        let target = std::env::temp_dir().join(format!("layr-other-dir-{}", std::process::id()));
        std::fs::create_dir_all(target.join("provider-contract-compile/debug")).unwrap();
        std::fs::create_dir_all(target.join("old-unlisted")).unwrap();
        let later = std::time::SystemTime::now() + UNLISTED_AGE * 2;
        let stale = stale_unlisted_but(&target, &[], &other, later);
        assert_eq!(stale, vec![target.join("old-unlisted")]);
        std::fs::remove_dir_all(&target).unwrap();
    }
}
