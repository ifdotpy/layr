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
struct CompileGuard {
    dir: PathBuf,
    path: PathBuf,
}

impl Drop for CompileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn no_compile_wrapper() -> Result<CompileGuard> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    const SCRIPT: &str =
        "#!/bin/sh\ncase \" $* \" in *\" --emit\"*) echo LAYR_PRUNE_COMPILE_REFUSED >&2; exit 1 ;; esac\nexec \"$@\"\n";
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("layr-nocompile-{}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("cannot create private Cargo wrapper folder"),
        }
    };
    let guard = CompileGuard { path: dir.join("rustc-wrapper"), dir };
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&guard.path)?;
    file.write_all(SCRIPT.as_bytes())?;
    Ok(guard)
}

/// The cargo arguments of a listing: `-v` names each unit that is not fresh before cargo compiles or runs
/// it, and `--message-format json` names the units. Both go before a `--` that ends the cargo arguments.
fn listing(args: &[String]) -> Result<Vec<String>> {
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut out = args.to_vec();
    let command = args.first().context("empty Cargo sweep command")?;
    let before = &args[..at];
    let no_run = match command.as_str() {
        "build" | "check" | "clippy" => false,
        "test" | "bench" => {
            anyhow::ensure!(!before.iter().any(|a| a == "--doc"), "Cargo doc tests cannot be listed without running them");
            !before.iter().any(|a| a == "--no-run")
        }
        _ => bail!("Cargo command {command} cannot be safely listed"),
    };
    let mut options = vec!["-v".to_string(), "--message-format".to_string(), "json".to_string()];
    if no_run {
        options.push("--no-run".into());
    }
    out.splice(at..at, options);
    Ok(out)
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
/// cargo, which also has the file open. Native process tables are tried before `lsof`.
/// Diagnostics have a two-second budget. The text says `unknown` when neither finds a process.
pub fn lock_holder(lock: &Path, except: u32) -> String {
    let mut pids: std::collections::BTreeSet<u32> = Default::default();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    pids.extend(proc_locks(lock));
    #[cfg(target_os = "macos")]
    pids.extend(macos_openers(lock, until));
    pids.remove(&except);
    if pids.is_empty() && std::time::Instant::now() < until {
        let mut command = Command::new("lsof");
        command.args(["-n", "-P", "-t"]).arg(lock).process_group(0);
        if let Ok(out) = run(&mut command, Some(until)) {
            pids.extend(String::from_utf8_lossy(&out).lines().filter_map(|l| l.trim().parse::<u32>().ok()));
        }
        pids.remove(&except);
    }
    if pids.is_empty() {
        return format!("unknown (process tables and lsof name no process for {})", lock.display());
    }
    let named: Vec<String> = pids.iter().map(|p| format!("pid {p} ({})", command_name(*p, until))).collect();
    named.join(", ")
}

/// macOS file references, without lsof's scan of unrelated file metadata.
#[cfg(target_os = "macos")]
fn macos_openers(lock: &Path, until: std::time::Instant) -> Vec<u32> {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn proc_listpidspath(kind: u32, info: u32, path: *const c_char, flags: u32, buffer: *mut c_void, size: c_int) -> c_int;
    }
    let Ok(path) = CString::new(lock.as_os_str().as_bytes()) else { return Vec::new() };
    let (tx, rx) = std::sync::mpsc::channel();
    // Keep the caller's deadline even if a kernel query takes too long.
    std::thread::spawn(move || {
        const PROC_ALL_PIDS: u32 = 1;
        // SAFETY: path is NUL-terminated; a null buffer requests its required size.
        let size = unsafe { proc_listpidspath(PROC_ALL_PIDS, 0, path.as_ptr(), 0, std::ptr::null_mut(), 0) };
        if size <= 0 {
            return;
        }
        // Leave room for processes created between the two calls.
        let mut pids = vec![0i32; size as usize / std::mem::size_of::<i32>() + 256];
        let bytes = (pids.len() * std::mem::size_of::<i32>()) as c_int;
        // SAFETY: the buffer is aligned and writable for `bytes`; path remains alive.
        let count = unsafe { proc_listpidspath(PROC_ALL_PIDS, 0, path.as_ptr(), 0, pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(count.max(0) as usize / std::mem::size_of::<i32>());
        let _ = tx.send(pids.into_iter().filter(|p| *p > 0).map(|p| p as u32).collect());
    });
    rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())).unwrap_or_default()
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
fn command_name(pid: u32, until: std::time::Instant) -> String {
    if std::time::Instant::now() >= until {
        return "?".into();
    }
    let mut command = Command::new("ps");
    command.args(["-o", "comm=", "-p", &pid.to_string()]).process_group(0);
    run(&mut command, Some(until))
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".into())
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
    Ok(list_units_with(host, root, target, env, args, false)?.0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StalePackage {
    name: String,
    source: Option<PathBuf>,
}

fn stale_package(line: &str) -> StalePackage {
    let rest = line.strip_prefix("Dirty ").or_else(|| line.strip_prefix("Compiling ")).unwrap_or("");
    let name = rest.split_whitespace().next().unwrap_or("").to_string();
    let source = rest.split_once(" (").and_then(|(_, path)| path.split_once(')').map(|(path, _)| PathBuf::from(path)));
    StalePackage { name, source }
}

fn cargo_lines(reader: impl BufRead, mut each: impl FnMut(String)) -> std::io::Result<()> {
    for line in reader.lines() {
        each(line?);
    }
    Ok(())
}

fn validate_messages(lines: &[String], success: bool) -> Result<()> {
    let mut finished = false;
    for line in lines.iter().filter(|line| !line.trim().is_empty()) {
        anyhow::ensure!(!finished, "Cargo output continues after completion");
        let message: Value = serde_json::from_str(line).context("invalid Cargo JSON output")?;
        match message["reason"].as_str() {
            Some("compiler-artifact") => {
                anyhow::ensure!(message["target"]["name"].is_string(), "Cargo artifact has no target name");
                let kinds = message["target"]["kind"].as_array().context("Cargo artifact has no target kind")?;
                anyhow::ensure!(!kinds.is_empty() && kinds.iter().all(Value::is_string), "invalid Cargo target kind");
                let files = message["filenames"].as_array().context("Cargo artifact has no filenames")?;
                anyhow::ensure!(
                    files.iter().all(Value::is_string)
                        && (!files.is_empty() || message["executable"].is_string())
                        && message["fresh"].is_boolean(),
                    "invalid Cargo artifact"
                );
            }
            Some("build-script-executed") => {
                anyhow::ensure!(message["out_dir"].is_string(), "Cargo build script has no output folder")
            }
            Some("compiler-message") => {}
            Some("build-finished") => {
                anyhow::ensure!(!finished && message["success"].as_bool() == Some(success), "invalid Cargo completion");
                finished = true;
            }
            _ => bail!("unknown Cargo JSON message"),
        }
    }
    anyhow::ensure!(finished, "Cargo listing has no completion message");
    Ok(())
}

fn list_units_with(
    host: &dyn Host,
    root: &Path,
    target: &Path,
    env: &[(String, String)],
    args: &[String],
    partial: bool,
) -> Result<(Listed, Vec<StalePackage>)> {
    // Cargo puts cross-target output under target/<triple>/<profile>, but host build scripts can
    // still use target/<profile>. Hold every matching profile lock while the copy and listing run.
    let profile = profile_of(args);
    let mut profiles = vec![target.join(&profile)];
    if target.is_dir() {
        for entry in std::fs::read_dir(target)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                profiles.push(entry.path().join(&profile));
            }
        }
    }
    let mut locks = Vec::new();
    for dir in profiles.into_iter().filter(|p| p.join(".fingerprint").is_dir()) {
        let path = dir.join(".cargo-lock");
        let Ok(lock) = std::fs::File::open(&path) else {
            return Ok((Listed::Busy("missing cargo build lock".into()), Vec::new()));
        };
        match lock.try_lock() {
            Ok(()) => locks.push(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                return Ok((Listed::Busy(lock_holder(&path, std::process::id())), Vec::new()))
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e).context("cannot lock the cargo build lock"),
        }
    }
    let result = list_snapshot(host, root, target, env, args, partial);
    unlock_all(&locks);
    result
}

/// Releases each flock before its file closes. A child that another thread forks keeps a copy of every
/// descriptor until it runs exec, so a lock released only by the close stays held for that time, and the
/// sweep that follows sees its own listing as a busy build.
pub(crate) fn unlock_all(locks: &[std::fs::File]) {
    for lock in locks {
        let _ = lock.unlock();
    }
}

/// Cargo may delete stale outputs before it reports `Dirty`. List a private copy so that a skipped
/// listing cannot change the cache. Never follow links from the copy back into the original tree.
fn list_snapshot(
    host: &dyn Host,
    root: &Path,
    target: &Path,
    env: &[(String, String)],
    args: &[String],
    partial: bool,
) -> Result<(Listed, Vec<StalePackage>)> {
    let cargo_args = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    if cargo_args
        .iter()
        .any(|a| a == "--target-dir" || a.starts_with("--target-dir=") || a == "--config" || a.starts_with("--config="))
    {
        bail!("the listing cannot use --target-dir or --config; use a sweep target_dir entry instead");
    }
    fn check_tree(dir: &Path) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() || !(kind.is_dir() || kind.is_file()) {
                bail!("cannot safely list a target with links or special files: {}", entry.path().display());
            }
            if kind.is_dir() {
                check_tree(&entry.path())?;
            }
        }
        Ok(())
    }
    // Cargo can run an already compiled build script after reporting it stale. Disable those
    // executables in the private copy before starting Cargo; the original target stays unchanged.
    fn block_build_scripts(dir: &Path, in_build: bool) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                block_build_scripts(&entry.path(), in_build || entry.file_name() == "build")?;
            } else if in_build
                && kind.is_file()
                && (entry.file_name().to_string_lossy().starts_with("build-script-")
                    || entry.file_name().to_string_lossy().starts_with("build_script_"))
            {
                let mut permissions = entry.metadata()?.permissions();
                permissions.set_mode(permissions.mode() & !0o111);
                std::fs::set_permissions(entry.path(), permissions)?;
            }
        }
        Ok(())
    }
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = target.parent().context("target has no parent")?;
    let scratch = loop {
        use std::os::unix::fs::DirBuilderExt;
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = parent.join(format!(".layr-cargo-list-{}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => break Scratch(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("cannot create cargo listing folder"),
        }
    };
    let copy = scratch.0.join("target");
    if target.exists() {
        if target.symlink_metadata()?.file_type().is_symlink() {
            bail!("cannot safely list a target that is a symbolic link");
        }
        check_tree(target)?;
        // macOS: the system cp by path. A PATH with GNU coreutils first (a check sandbox) has a cp without `-c`.
        let mut cp = host.command(if cfg!(target_os = "macos") { "/bin/cp" } else { "cp" }).context("cannot run cp")?;
        #[cfg(target_os = "macos")]
        cp.args(["-cRp"]);
        #[cfg(not(target_os = "macos"))]
        cp.args(["-a", "--reflink=auto"]);
        cp.arg(target).arg(&copy);
        run(&mut cp, Some(host.deadline())).context("cannot copy cargo target for listing")?;
        check_tree(&copy)?;
    } else {
        std::fs::create_dir(&copy)?;
    }
    block_build_scripts(&copy, false)?;
    match list_units_in(host, root, &copy, env, args, partial)? {
        (Listed::Fresh(plans), dirty) => {
            let mut mapped = sweep::Plans::new();
            for (path, live) in plans {
                mapped.insert(target.join(path.strip_prefix(&copy)?), live);
            }
            Ok((Listed::Fresh(mapped), dirty))
        }
        other => Ok(other),
    }
}

fn list_units_in(
    host: &dyn Host,
    root: &Path,
    target: &Path,
    env: &[(String, String)],
    args: &[String],
    partial: bool,
) -> Result<(Listed, Vec<StalePackage>)> {
    let mut c = host_cargo(host)?;
    let mut list_args = listing(args)?;
    if partial {
        let at = list_args.iter().position(|a| a == "--").unwrap_or(list_args.len());
        list_args.insert(at, "--keep-going".into());
    }
    c.args(list_args).current_dir(root).env("CARGO_TARGET_DIR", target).env("CARGO_BUILD_JOBS", "1");
    c.envs(env.iter().map(|(k, v)| (k, v))).env("CARGO_TARGET_DIR", target).env("LLVM_PROFILE_FILE", cargo_profile_file(target));
    // Set after the project's env: the listing must not compile, whatever the project's wrapper is.
    let wrapper = no_compile_wrapper()?;
    c.env("RUSTC_WRAPPER", &wrapper.path);
    // Set last: the project's env cannot turn the color back on (see `cargo_cmd`).
    let mut c = with_color(c);
    c.process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().with_context(|| format!("cannot start cargo {}", args.join(" ")))?;
    let pid = child.id();
    let (out, err) = (child.stdout.take().context("cargo stdout")?, child.stderr.take().context("cargo stderr")?);
    let stop: Arc<Mutex<Option<Listed>>> = Arc::default();
    let waited: Arc<Mutex<Option<(String, Option<PathBuf>)>>> = Arc::default();
    let dirty: Arc<Mutex<Vec<StalePackage>>> = Arc::default();
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let refused: Arc<Mutex<usize>> = Arc::default();
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let (errs_tx, errs_rx) = std::sync::mpsc::channel::<()>();
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut lines = Vec::new();
            if let Err(e) = cargo_lines(BufReader::new(out), |line| {
                if let Some(name) = sweep::not_fresh(&line) {
                    halt(&stop, pid, Listed::NotFresh(format!("{name} is not fresh")));
                }
                lines.push(line);
            }) {
                halt(&stop, pid, Listed::NotFresh(format!("cannot read Cargo output: {e}")));
            }
            let _ = lines_tx.send(lines);
        });
    }
    {
        let (stop, waited, dirty, errors, refused) =
            (stop.clone(), waited.clone(), dirty.clone(), errors.clone(), refused.clone());
        let lock_dir = target.join(profile_of(args));
        let home = cargo_home(env);
        let wrapper_line = format!("Running `{} ", wrapper.path.display());
        std::thread::spawn(move || {
            if let Err(e) = cargo_lines(BufReader::new(err), |line| {
                let t = line.trim_start();
                if t.starts_with("Blocking ") && t.contains("directory") {
                    // Stop cargo before doing potentially slow process diagnostics.
                    halt(&stop, pid, Listed::Busy(String::new()));
                } else if t.starts_with("Blocking ") {
                    let holder = if t.contains("package cache") {
                        lock_holder(&home.join(".package-cache"), pid)
                    } else {
                        "unknown (layr does not know this lock)".to_string()
                    };
                    eprintln!("layr-retest: cargo waits: {t}; holder: {holder}");
                    *waited.lock().unwrap() =
                        Some((t.to_string(), t.contains("package cache").then(|| home.join(".package-cache"))));
                } else if partial && (t.starts_with("Dirty ") || t.starts_with("Compiling ")) {
                    let package = stale_package(t);
                    let mut d = dirty.lock().unwrap();
                    if !d.contains(&package) {
                        d.push(package);
                    }
                } else if partial && t.starts_with(&wrapper_line) && t.contains("--crate-name build_script_") {
                    halt(&stop, pid, Listed::NotFresh("a build script needs compilation".into()));
                } else if partial && t.starts_with(&wrapper_line) {
                    // The wrapper refuses compilation before rustc can write an output.
                } else if partial && t == "LAYR_PRUNE_COMPILE_REFUSED" {
                    *refused.lock().unwrap() += 1;
                } else if partial && t.starts_with("error:") {
                    errors.lock().unwrap().push(t.to_string());
                } else if ["Dirty ", "Compiling ", "Running "].iter().any(|p| t.starts_with(p)) {
                    halt(&stop, pid, Listed::NotFresh(t.to_string()));
                }
            }) {
                halt(&stop, pid, Listed::NotFresh(format!("cannot read Cargo errors: {e}")));
            }
            let _ = errs_tx.send(());
        });
    }
    let (status, late) =
        wait_child(&mut child, Some(host.deadline())).with_context(|| format!("cannot wait for cargo {}", args.join(" ")))?;
    let _ = std::fs::remove_dir_all(profile_scratch(target));
    let describe = |stopped| match stopped {
        Listed::Busy(_) => Listed::Busy(lock_holder(&target.join(profile_of(args)).join(".cargo-lock"), pid)),
        other => other,
    };
    if let Some(stopped) = stop.lock().unwrap().take() {
        return Ok((stopped, Vec::new()));
    }
    if late {
        let why = waited
            .lock()
            .unwrap()
            .take()
            .map(|(message, lock)| {
                let holder =
                    lock.map(|p| lock_holder(&p, pid)).unwrap_or_else(|| "unknown (layr does not know this lock)".into());
                format!("{message}; holder: {holder}")
            })
            .unwrap_or_default();
        bail!("the cargo listing of {} reached its time limit while it waited: {why}", args.join(" "));
    }
    // The readers finish before the stop is final: a line that was read after the exit can still stop the listing.
    let lines = lines_rx.recv_timeout(std::time::Duration::from_secs(5)).ok();
    let errs_done = errs_rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();
    let dirty = std::mem::take(&mut *dirty.lock().unwrap());
    if let Some(stopped) = stop.lock().unwrap().take() {
        return Ok((stopped, dirty));
    }
    let (Some(lines), true) = (lines, errs_done) else {
        bail!("the cargo listing of {} kept its output open; a process outside its group holds it", args.join(" "));
    };
    let errors = errors.lock().unwrap();
    let refusals = *refused.lock().unwrap();
    let expected_errors = !errors.is_empty()
        && errors.len() == refusals
        && errors.iter().all(|line| {
            line.strip_prefix("error: could not compile `")
                .and_then(|rest| rest.split_once('`'))
                .is_some_and(|(package, _)| dirty.iter().any(|item| item.name == package))
        });
    if !status.success() && !(partial && !dirty.is_empty() && expected_errors) {
        bail!("cargo {} failed", args.join(" "));
    }
    validate_messages(&lines, status.success())?;
    Ok((Listed::Fresh(sweep::plan(target, lines.join("\n").as_bytes())?), dirty))
}

/// The live units of the target folder. A partial listing keeps every workspace unit in the
/// affected profiles and is accepted only when the stale units are workspace members.
pub fn live_units(
    host: &dyn Host,
    root: &Path,
    target: &Path,
    env: &[(String, String)],
    commands: &[Vec<String>],
) -> Result<Listed> {
    let mut live = sweep::Plans::new();
    let mut members: Option<Vec<WorkspaceMember>> = None;
    let mut keep = std::collections::BTreeSet::new();
    for args in commands {
        match list_units(host, root, target, env, args)? {
            Listed::Fresh(units) => sweep::merge(&mut live, units),
            Listed::NotFresh(why) => {
                if members.is_none() {
                    members = Some(workspace_members(host, root)?);
                }
                let workspace = members.as_deref().unwrap_or_default();
                match list_units_with(host, root, target, env, args, true)? {
                    (Listed::Fresh(units), dirty) => {
                        if let Some(other) = dirty.iter().find(|package| !workspace_contains(workspace, package)) {
                            return Ok(Listed::NotFresh(format!("{why}; {} is not a workspace member", other.name)));
                        }
                        sweep::merge(&mut live, units);
                        let selected = profile_folder(target, args)?;
                        let host = target.join(profile_of(args));
                        // A cross-target listing may not enumerate the host profile. Do not add
                        // that profile to the sweep unless Cargo actually listed units there.
                        if host != selected && live.contains_key(&host) {
                            keep.insert(host);
                        }
                        keep.insert(selected);
                    }
                    (Listed::NotFresh(more), _) => return Ok(Listed::NotFresh(format!("{why}; {more}"))),
                    (busy, _) => return Ok(busy),
                }
            }
            busy => return Ok(busy),
        }
    }
    if let Some(workspace) = &members {
        let names: Vec<String> = workspace.iter().map(|member| member.name.clone()).collect();
        for profile in keep {
            if !profile.try_exists()? {
                continue;
            }
            sweep::merge(&mut live, sweep::Plans::from([(profile.clone(), member_units(&profile, &names)?)]));
        }
    }
    Ok(Listed::Fresh(live))
}

struct WorkspaceMember {
    name: String,
    source: PathBuf,
}

fn workspace_contains(workspace: &[WorkspaceMember], package: &StalePackage) -> bool {
    let source = package.source.as_ref().and_then(|path| path.canonicalize().ok());
    workspace.iter().any(|member| member.name == package.name && source.as_ref() == Some(&member.source))
}

fn workspace_members(host: &dyn Host, root: &Path) -> Result<Vec<WorkspaceMember>> {
    let meta = metadata(host, root)?;
    let ids = meta["workspace_members"].as_array().context("cargo metadata has no workspace members")?;
    let packages = meta["packages"].as_array().context("cargo metadata has no packages")?;
    let mut members = Vec::new();
    for id in ids {
        let id = id.as_str().context("invalid workspace member id")?;
        let package = packages.iter().find(|p| p["id"].as_str() == Some(id)).context("workspace member has no package")?;
        let name = package["name"].as_str().context("workspace member has no name")?.to_string();
        let manifest = package["manifest_path"].as_str().context("workspace member has no manifest")?;
        let source = Path::new(manifest).parent().context("workspace manifest has no folder")?.canonicalize()?;
        members.push(WorkspaceMember { name, source });
    }
    anyhow::ensure!(!members.is_empty(), "cargo metadata has no workspace members");
    Ok(members)
}

fn profile_folder(target: &Path, args: &[String]) -> Result<PathBuf> {
    let own = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    let triple = own
        .iter()
        .position(|a| a == "--target")
        .and_then(|i| own.get(i + 1))
        .cloned()
        .or_else(|| own.iter().find_map(|a| a.strip_prefix("--target=")).map(String::from));
    let base = match triple {
        Some(t) => {
            anyhow::ensure!(
                !t.is_empty() && t != "." && t != ".." && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "unsupported Cargo target path"
            );
            target.join(t)
        }
        None => target.to_path_buf(),
    };
    let profile = profile_of(args);
    anyhow::ensure!(
        !profile.is_empty()
            && profile != "."
            && profile != ".."
            && profile.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "unsupported Cargo profile"
    );
    Ok(base.join(profile))
}

fn member_units(profile: &Path, members: &[String]) -> Result<sweep::Live> {
    let mut live = sweep::Live::default();
    let fingerprints = profile.join(".fingerprint");
    if profile.try_exists()? {
        for entry in std::fs::read_dir(fingerprints).context("missing Cargo fingerprints")? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some((package, hash)) = name.rsplit_once('-') else { continue };
            if members.iter().any(|m| m == package) && sweep::unit_hash(&name).is_some() {
                live.hashes.insert(hash.to_string());
            }
        }
    }
    live.crates.extend(members.iter().map(|m| m.replace('-', "_")));
    Ok(live)
}

/// The profile folder that a cargo command builds into: `--profile <name>` (`dev` and `test` are `debug`, `bench`
/// is `release`), `--release` is `release`, and the default is `debug`. Only the cargo arguments count (before `--`).
pub fn profile_of(args: &[String]) -> String {
    let own = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    let named = own
        .iter()
        .position(|a| a == "--profile")
        .and_then(|i| own.get(i + 1))
        .cloned()
        .or_else(|| own.iter().find_map(|a| a.strip_prefix("--profile=")).map(String::from));
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
pub fn live_by_profile(
    host: &dyn Host,
    root: &Path,
    target: &Path,
    env: &[(String, String)],
    commands: &[Vec<String>],
) -> Result<Vec<(String, Vec<Vec<String>>, Listed)>> {
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
pub fn stale_unlisted_but(
    target: &Path,
    commands: &[Vec<String>],
    other: &[OtherDir],
    now: std::time::SystemTime,
) -> Vec<PathBuf> {
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
    if matches!(args.first().map(String::as_str), Some("build" | "check" | "clippy" | "doc"))
        && !args.iter().any(|a| a == "--keep-going")
    {
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
    if !built(target) && !cfg.other_dirs.iter().any(|other| built(&target.join(&other.dir))) {
        return Ok(skip("target has no build output"));
    }
    // The listings must not reach the network: a unit whose crate is not in the cargo cache fails the listing.
    let mut env = cfg.env;
    env.push(("CARGO_NET_OFFLINE".to_string(), "true".to_string()));
    // Each profile folder is pruned on its own: a stale command of one profile keeps only that folder.
    let mut ready = Vec::new();
    let mut kept = Vec::new();
    for (profile, _, listed) in live_by_profile(host, root, target, &env, &cfg.commands)? {
        match listed {
            Listed::Fresh(live) => {
                ready.push((profile, live));
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
                    ready.push((format!("{}/{profile}", other.dir), live));
                }
                Listed::NotFresh(why) => kept.push(format!("{}/{profile}: a unit is not fresh ({why})", other.dir)),
                Listed::Busy(holder) => kept.push(format!("{}/{profile}: build lock busy (holder: {holder})", other.dir)),
            }
        }
    }
    // Complete every listing before removing output: a later listing error must leave the target intact.
    let mut freed = 0;
    let mut fresh = 0;
    for (profile, live) in ready {
        let (n, busy) = sweep_plans(&live, false)?;
        fresh += usize::from(!live.is_empty() && busy < live.len());
        if busy > 0 {
            kept.push(format!("{profile}: build lock busy during sweep"));
        }
        freed += n;
    }
    if fresh == 0 {
        return Ok(skip(format!("no profile folder is fresh: {}; run the standard build first", kept.join("; "))));
    }
    // The folders that no listed command makes have no live set: an old one goes (issue #195).
    for dir in stale_unlisted_but(target, &cfg.commands, &cfg.other_dirs, std::time::SystemTime::now()) {
        let n = size_of(&dir);
        if std::fs::remove_dir_all(&dir).is_ok() {
            freed += n;
        }
    }
    Ok(Outcome::Pruned { freed })
}

#[cfg(test)]
mod unlock_tests {
    use super::unlock_all;
    use std::fs::{File, TryLockError};

    /// A descriptor copy (what a child that another thread forks holds until exec) keeps a flock that only
    /// the close of the original released: the next lock of the file is refused. `unlock_all` releases it.
    #[test]
    fn a_released_build_lock_is_free_while_a_copy_of_its_descriptor_lives() {
        let dir = std::env::temp_dir().join(format!("layr-unlock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".cargo-lock");
        std::fs::write(&path, "").unwrap();

        let closed_only = File::open(&path).unwrap();
        closed_only.try_lock().unwrap();
        let copy = closed_only.try_clone().unwrap();
        drop(closed_only);
        assert!(matches!(File::open(&path).unwrap().try_lock(), Err(TryLockError::WouldBlock)), "the copy holds the lock");
        // A child that another test forks meanwhile keeps this description too: release it explicitly.
        unlock_all(std::slice::from_ref(&copy));
        drop(copy);

        let released = File::open(&path).unwrap();
        released.try_lock().unwrap();
        let copy = released.try_clone().unwrap();
        unlock_all(std::slice::from_ref(&released));
        drop(released);
        File::open(&path).unwrap().try_lock().expect("an unlocked file is free while a copy lives");
        drop(copy);
        std::fs::remove_dir_all(&dir).unwrap();
    }
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
    fn cargo_listing_wrappers_have_separate_private_folders() {
        use std::os::unix::fs::PermissionsExt;
        let first = no_compile_wrapper().unwrap();
        let second = no_compile_wrapper().unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::metadata(&first.dir).unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(std::fs::metadata(&first.path).unwrap().permissions().mode() & 0o077, 0);
        let dir = first.dir.clone();
        drop(first);
        assert!(!dir.exists());
    }

    #[test]
    fn a_partial_listing_refuses_target_and_profile_paths() {
        let target = Path::new("/tmp/example-target");
        assert!(profile_folder(target, &["build".into(), "--target=../../outside".into()]).is_err());
        assert!(profile_folder(target, &["build".into(), "--profile=../outside".into()]).is_err());
        assert_eq!(
            profile_folder(target, &["build".into(), "--target=x86_64-unknown-linux-gnu".into()]).unwrap(),
            target.join("x86_64-unknown-linux-gnu/debug")
        );
    }

    #[test]
    fn cargo_output_read_errors_are_not_treated_as_end_of_file() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken pipe"))
            }
        }
        impl std::io::BufRead for Broken {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                Err(std::io::Error::other("broken pipe"))
            }
            fn consume(&mut self, _: usize) {}
        }
        assert!(cargo_lines(Broken, |_| {}).is_err());
    }

    #[test]
    fn listing_refuses_commands_that_can_run_project_programs() {
        let args = |items: &[&str]| items.iter().map(|item| (*item).to_string()).collect::<Vec<_>>();
        let test = listing(&args(&["test", "--workspace"])).unwrap();
        assert!(test.contains(&"--no-run".to_string()));
        assert!(listing(&args(&["test", "--doc"])).is_err());
        assert!(listing(&args(&["run"])).is_err());
    }

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
