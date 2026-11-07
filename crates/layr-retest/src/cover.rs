//! The cargo calls of a gate, and whether the `sweep` list keeps their units warm. During `layr retest gate` a
//! `cargo` script first on PATH records each top-level cargo call (its folder, its target folder, the env that
//! changes units, and its arguments), then runs the real cargo. After a passing command, each recorded call is
//! listed the way the sweep lists its commands, without a build. A unit that a call uses and no `sweep` command
//! builds is not in the cache of main, so every new line builds it cold. The gate names such a call and the `sweep`
//! entry that covers it. See docs/retest.md, "Gate coverage".

use anyhow::{Context, Result};
use layr_cache::cargo::{self, Listed, Settings};
use layr_cache::sweep::Plans;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The file that the `cargo` script appends the calls to. Set only for the gate command.
pub const LOG_VAR: &str = "LAYR_RETEST_CARGO_LOG";
/// The cargo that the script runs: the first `cargo` on PATH before the gate put the script there.
const REAL_VAR: &str = "LAYR_RETEST_REAL_CARGO";
/// The layr binary that records a call.
const LAYR_VAR: &str = "LAYR_RETEST_LAYR";
/// Variables beyond `cargo::KEY_ENV` that change the units of a command: clippy reads its config folder.
const EXTRA_ENV: &[&str] = &["CLIPPY_CONF_DIR"];
/// The result of the last gate in the records folder, for `layr retest doctor`.
pub const REPORT_FILE: &str = "gate-coverage.json";
/// The most distinct calls that one gate lists. A listing takes seconds; a gate with more calls checks the first ones.
const MAX_CALLS: usize = 40;
/// The most unit names that one finding prints.
const NAMES: usize = 5;

/// The script. Cargo sets `CARGO` for the processes that it starts (build scripts, `cargo-nextest`, `cargo-clippy`),
/// so a call from inside a cargo call is not a call of the gate: it runs the real cargo unrecorded.
const SCRIPT: &str = "#!/bin/sh\n\
# layr retest gate: records a top-level cargo call, then runs the real cargo.\n\
if [ -z \"${CARGO-}\" ] && [ -n \"${LAYR_RETEST_CARGO_LOG-}\" ]; then\n\
  exec \"$LAYR_RETEST_LAYR\" retest record-cargo \"$@\"\n\
fi\n\
exec \"$LAYR_RETEST_REAL_CARGO\" \"$@\"\n";

/// The folder of the `cargo` script of one gate, and its log. Removed on drop.
pub struct Shim {
    dir: PathBuf,
    pub log: PathBuf,
    real: PathBuf,
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The script folder for a gate. `None` without a `cargo` on PATH: then nothing is recorded.
pub fn shim() -> Result<Option<Shim>> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(real) = std::env::split_paths(&path).map(|d| d.join("cargo")).find(|p| executable(p)) else { return Ok(None) };
    let dir = std::env::temp_dir().join(format!("layr-retest-cargo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let script = dir.join("cargo");
    std::fs::write(&script, SCRIPT)?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    Ok(Some(Shim { log: dir.join("calls.jsonl"), dir, real }))
}

impl Shim {
    /// The env that puts the script first on PATH for the gate command.
    pub fn env(&self) -> Result<Vec<(String, OsString)>> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(self.dir.clone()).chain(std::env::split_paths(&path));
        Ok(vec![
            ("PATH".into(), std::env::join_paths(dirs).context("PATH")?),
            (LOG_VAR.into(), self.log.clone().into()),
            (REAL_VAR.into(), self.real.clone().into()),
            (LAYR_VAR.into(), std::env::current_exe()?.into()),
        ])
    }

    /// Applies the env of the script to the gate command. The gate command runs at the top, not inside a cargo
    /// call, so a `CARGO` that the gate got from its own caller (`cargo test`, `cargo run`) is removed: the script
    /// would take every call for a call from inside cargo.
    pub fn apply(&self, cmd: &mut Command) -> Result<()> {
        cmd.envs(self.env()?).env_remove("CARGO");
        Ok(())
    }
}

/// `layr retest record-cargo <args>`: appends the call to the log, then runs the real cargo in this process. A
/// record that fails never stops the call.
pub fn record(args: &[String]) -> Result<i32> {
    let real = std::env::var_os(REAL_VAR).context("record-cargo runs only from the cargo script of a gate")?;
    if let Some(log) = std::env::var_os(LOG_VAR) {
        if let Err(e) = append(Path::new(&log), args) {
            eprintln!("layr-retest gate: cannot record a cargo call: {e:#}");
        }
    }
    let err = Command::new(&real).args(args).exec();
    Err(anyhow::anyhow!("cannot run {}: {err}", PathBuf::from(real).display()))
}

/// One line of JSON per call, written in one append.
fn append(log: &Path, args: &[String]) -> Result<()> {
    let env: BTreeMap<String, String> =
        cargo::KEY_ENV.iter().chain(EXTRA_ENV).filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect();
    let call = json!({
        "cwd": std::env::current_dir()?,
        "target_dir": std::env::var("CARGO_TARGET_DIR").ok(),
        "env": env,
        "args": args,
    });
    let mut text = serde_json::to_string(&call)?;
    text.push('\n');
    std::fs::OpenOptions::new().create(true).append(true).open(log)?.write_all(text.as_bytes())?;
    Ok(())
}

/// A recorded cargo call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Call {
    pub cwd: PathBuf,
    pub target_dir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
}

/// The calls of a log, each once, in the order of their first call. Bad lines are skipped.
pub fn read_calls(log: &Path) -> Vec<Call> {
    let mut out: Vec<Call> = Vec::new();
    for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let strings = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        let call = Call {
            cwd: PathBuf::from(v["cwd"].as_str().unwrap_or_default()),
            target_dir: v["target_dir"].as_str().map(PathBuf::from),
            env: v["env"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect(),
            args: strings(&v["args"]),
        };
        if !out.contains(&call) {
            out.push(call);
        }
    }
    out
}

/// Cargo options that take a value, for the commands whose arguments are rebuilt (test, bench, nextest).
const VALUE_OPTS: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "-F",
    "--features",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--profile",
    "--bin",
    "--test",
    "--example",
    "--bench",
    "--config",
    "-Z",
];
/// Cargo options without a value that select units, for the same commands.
const FLAG_OPTS: &[&str] = &[
    "--workspace",
    "--all",
    "--lib",
    "--bins",
    "--tests",
    "--examples",
    "--benches",
    "--all-targets",
    "--all-features",
    "--no-default-features",
    "--release",
    "--locked",
    "--frozen",
    "--offline",
    "--ignore-rust-version",
];
/// Options that change only the output or the job count, not the units. Dropped from every listing.
const NOISE_VALUE: &[&str] = &["-j", "--jobs", "--color", "--message-format"];
const NOISE_FLAG: &[&str] = &["-v", "-vv", "--verbose", "-q", "--quiet", "--keep-going", "--future-incompat-report", "--timings"];

/// The unit options of the cargo arguments `own` (the words before a `--`) of a test, bench or nextest call.
fn unit_options(own: &[String], nextest: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = own.iter();
    while let Some(a) = it.next() {
        let (name, inline) = a.split_once('=').map_or((a.as_str(), None), |(n, v)| (n, Some(v)));
        // nextest's `--profile` is its own; the cargo profile is `--cargo-profile`.
        let name = match (nextest, name) {
            (true, "--profile") | (true, "-P") => {
                if inline.is_none() {
                    it.next();
                }
                continue;
            }
            (true, "--cargo-profile") => "--profile",
            (_, n) => n,
        };
        if VALUE_OPTS.contains(&name) {
            let value = match inline {
                Some(v) => v.to_string(),
                None => match it.next() {
                    Some(v) => v.clone(),
                    None => break,
                },
            };
            out.push(name.to_string());
            out.push(value);
        } else if FLAG_OPTS.contains(&name) {
            out.push(name.to_string());
        }
        // Anything else (a test filter, a nextest or harness option) selects no unit.
    }
    out
}

/// The cargo arguments that list the units of a recorded call, or `None` for a call that builds nothing (`fetch`,
/// `metadata`, `fmt`, a help or version call). `test`, `bench` and `nextest` become a `--no-run` test build with the
/// same unit options; `run` becomes a build; `test --doc` becomes a library build. Output and job options go.
pub fn listing_args(args: &[String]) -> Option<Vec<String>> {
    let (toolchain, rest) = match args.first() {
        Some(t) if t.starts_with('+') => (vec![t.clone()], &args[1..]),
        _ => (Vec::new(), args),
    };
    let at = rest.iter().position(|a| !a.starts_with('-'))?;
    let (global, sub, after) = (&rest[..at], rest[at].as_str(), &rest[at + 1..]);
    if global.iter().any(|g| matches!(g.as_str(), "-V" | "--version" | "-h" | "--help" | "--list")) {
        return None;
    }
    let split = |v: &[String]| {
        let i = v.iter().position(|a| a == "--").unwrap_or(v.len());
        (v[..i].to_vec(), v[i..].to_vec())
    };
    let mut out = toolchain;
    out.extend(global.iter().cloned());
    match sub {
        "build" | "b" | "check" | "c" | "clippy" | "doc" | "d" | "rustc" | "rustdoc" => {
            let (own, tail) = split(after);
            if own.iter().any(|a| a == "-h" || a == "--help") {
                return None;
            }
            out.push(sub.to_string());
            out.extend(drop_noise(&own));
            // The arguments after `--` reach rustc or clippy and change their units.
            out.extend(tail);
        }
        "run" | "r" => {
            out.push("build".into());
            out.extend(drop_noise(&split(after).0));
        }
        "test" | "t" | "bench" => {
            let own = split(after).0;
            if own.iter().any(|a| a == "--doc") {
                out.push("build".into());
                out.push("--lib".into());
                out.extend(unit_options(&own, false).into_iter().filter(|a| !matches!(a.as_str(), "--tests" | "--all-targets")));
            } else {
                out.push(if sub == "bench" { "bench".into() } else { "test".into() });
                out.extend(unit_options(&own, false));
                out.push("--no-run".into());
            }
        }
        "nextest" => {
            let mode = after.first()?.as_str();
            if !matches!(mode, "run" | "r" | "list" | "archive") {
                return None;
            }
            out.push("test".into());
            out.extend(unit_options(&split(&after[1..]).0, true));
            out.push("--no-run".into());
        }
        _ => return None,
    }
    Some(out)
}

fn drop_noise(own: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = own.iter();
    while let Some(a) = it.next() {
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        if NOISE_VALUE.contains(&name) {
            if !a.contains('=') && !(name == "-j" && a.len() > 2) {
                it.next();
            }
        } else if NOISE_FLAG.contains(&name) || name.starts_with("--timings") || (name.starts_with("-j") && name.len() > 2) {
        } else {
            out.push(a.clone());
        }
    }
    out
}

/// Whether a listing selects packages with `-p`: cargo then resolves the features of those packages only.
fn selects_packages(args: &[String]) -> bool {
    let own = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    own.iter().any(|a| a == "-p" || a == "--package" || a.starts_with("--package=") || (a.starts_with("-p") && a.len() > 2))
}

/// What the coverage check found for one call.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Every unit of the call is a unit of a `sweep` command.
    Covered,
    /// Units that no `sweep` command builds: their names, the `sweep` entry that covers them, and the variables that
    /// the `env` table needs for it (a call into `target` itself with another env than the project's).
    Cold { units: Vec<String>, entry: Value, env: BTreeMap<String, String> },
    /// The call builds into a folder out of the cache folder `target`: no line takes those units warm.
    Outside(PathBuf),
    /// Not checked, and why.
    Unchecked(String),
}

/// One finding of the coverage check.
pub struct Finding {
    pub call: Call,
    pub listing: Vec<String>,
    pub verdict: Verdict,
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The target folder of a call: `--target-dir`, else its `CARGO_TARGET_DIR`, else `target` of the project when the
/// call ran in it. Relative paths are taken from the call's folder.
fn target_of(call: &Call, root: &Path) -> Option<PathBuf> {
    let own = &call.args[..call.args.iter().position(|a| a == "--").unwrap_or(call.args.len())];
    let flag = own
        .iter()
        .position(|a| a == "--target-dir")
        .and_then(|i| own.get(i + 1))
        .cloned()
        .or_else(|| own.iter().find_map(|a| a.strip_prefix("--target-dir=")).map(String::from));
    match flag.map(PathBuf::from).or_else(|| call.target_dir.clone()) {
        Some(t) => Some(canonical(&call.cwd.join(t))),
        None if canonical(&call.cwd).starts_with(canonical(root)) => Some(canonical(root).join("target")),
        None => None,
    }
}

/// The `sweep` commands and env that build into the folder `sub` below `target` (`""` for `target` itself).
fn sweep_of(settings: &Settings, sub: &str) -> (Vec<Vec<String>>, Vec<(String, String)>) {
    if sub.is_empty() {
        return (settings.commands.clone(), settings.env.clone());
    }
    let mut env = settings.env.clone();
    let mut commands = Vec::new();
    for o in settings.other_dirs.iter().filter(|o| o.dir == sub) {
        commands.push(o.args.clone());
        env.extend(o.env.iter().cloned());
    }
    (commands, env)
}

/// The live units of the `sweep` commands of one folder, by profile folder. A profile whose commands are not fresh
/// is `Err` with the reason.
type SweepLive = BTreeMap<PathBuf, std::result::Result<layr_cache::sweep::Live, String>>;

fn sweep_live(root: &Path, target: &Path, commands: &[Vec<String>], env: &[(String, String)]) -> Result<SweepLive> {
    let mut out = SweepLive::new();
    for (profile, _, listed) in cargo::live_by_profile(&crate::retest_host(), root, target, env, commands)? {
        let dir = target.join(&profile);
        match listed {
            Listed::Fresh(plans) => {
                for (p, live) in plans {
                    out.insert(p, Ok(live));
                }
            }
            Listed::NotFresh(why) => {
                out.insert(dir, Err(format!("a sweep command of this profile is not fresh after the gate ({why})")));
            }
            Listed::Busy(holder) => {
                out.insert(dir, Err(format!("another cargo holds the build lock: {holder}")));
            }
        }
    }
    Ok(out)
}

/// The unit names (`<name>-<hash>`) of `hashes` in the profile folder `profile`, from its `.fingerprint` folder.
fn unit_names(profile: &Path, hashes: &BTreeSet<String>) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(profile.join(".fingerprint"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| layr_cache::sweep::unit_hash(n).is_some_and(|h| hashes.contains(h)))
        .collect();
    names.sort();
    names
}

/// The units of `call` that are not in `sweep`, as `<profile>/<name>-<hash>`. Empty when every unit is covered.
fn cold_units(call: &Plans, sweep: &SweepLive, target: &Path) -> std::result::Result<Vec<String>, String> {
    let mut cold = Vec::new();
    for (profile, live) in call {
        let have = match sweep.get(profile) {
            Some(Ok(l)) => l.hashes.clone(),
            Some(Err(why)) => return Err(why.clone()),
            None => BTreeSet::new(),
        };
        let missing: BTreeSet<String> = live.hashes.difference(&have).cloned().collect();
        if !missing.is_empty() {
            let rel = profile.strip_prefix(target).unwrap_or(profile);
            cold.extend(unit_names(profile, &missing).into_iter().map(|n| format!("{}/{n}", rel.display())));
        }
    }
    Ok(cold)
}

/// The `sweep` entry for a call, and the variables that the `env` table needs. A call into `target` is the listing
/// arguments; its variables that differ from the project's go to the `env` table, which every `sweep` command of
/// `target` shares. A call into a folder below `target` is an object with `target_dir`, `args` and its own `env`.
fn entry_of(
    listing: &[String],
    sub: &str,
    call_env: &[(String, String)],
    project_env: &[(String, String)],
) -> (Value, BTreeMap<String, String>) {
    let extra: BTreeMap<String, String> = call_env.iter().filter(|kv| !project_env.contains(kv)).cloned().collect();
    if sub.is_empty() {
        return (json!(listing), extra);
    }
    let mut o = json!({ "target_dir": sub, "args": listing });
    if !extra.is_empty() {
        o["env"] = json!(extra);
    }
    (o, BTreeMap::new())
}

/// Checks each call against the `sweep` list of the project at `root`. Calls that build nothing are left out.
pub fn check(root: &Path, settings: &Settings, calls: &[Call]) -> Vec<Finding> {
    let root = canonical(root);
    let cache = root.join("target");
    let mut out = Vec::new();
    let mut sweeps: BTreeMap<String, std::result::Result<SweepLive, String>> = BTreeMap::new();
    // A call is listed once per folder, target folder, env and listing arguments.
    type Key = (PathBuf, Option<PathBuf>, Vec<(String, String)>, Vec<String>);
    let mut seen: Vec<Key> = Vec::new();
    for call in calls {
        let Some(listing) = listing_args(&call.args) else { continue };
        let target = target_of(call, &root);
        let key = (canonical(&call.cwd), target.clone(), call.env.clone(), listing.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if seen.len() > MAX_CALLS {
            out.push(Finding {
                call: call.clone(),
                listing,
                verdict: Verdict::Unchecked(format!("more than {MAX_CALLS} cargo calls")),
            });
            break;
        }
        let verdict = (|| {
            let Some(target) = target else {
                return Verdict::Unchecked("it ran out of the project and names no target folder".into());
            };
            let Ok(rel) = target.strip_prefix(&cache) else { return Verdict::Outside(target) };
            if !call.cwd.is_dir() {
                return Verdict::Unchecked(format!("its folder {} is gone", call.cwd.display()));
            }
            let sub = rel.to_string_lossy().into_owned();
            let (commands, sweep_env) = sweep_of(settings, &sub);
            let sweep = sweeps
                .entry(sub.clone())
                .or_insert_with(|| sweep_live(&root, &target, &commands, &sweep_env).map_err(|e| format!("{e:#}")));
            let sweep = match sweep {
                Ok(s) => s,
                Err(why) => return Verdict::Unchecked(format!("the sweep listing failed: {why}")),
            };
            let mut env = sweep_env.clone();
            env.extend(call.env.iter().cloned());
            match cargo::list_units(&crate::retest_host(), &call.cwd, &target, &env, &listing) {
                Ok(Listed::Fresh(plans)) => match cold_units(&plans, sweep, &target) {
                    Ok(units) if units.is_empty() => Verdict::Covered,
                    Ok(units) => {
                        let (entry, env) = entry_of(&listing, &sub, &call.env, &sweep_env);
                        Verdict::Cold { units, entry, env }
                    }
                    Err(why) => Verdict::Unchecked(why),
                },
                Ok(Listed::NotFresh(why)) => Verdict::Unchecked(format!("its units are not fresh after the gate ({why})")),
                Ok(Listed::Busy(holder)) => Verdict::Unchecked(format!("another cargo holds the build lock: {holder}")),
                Err(e) => Verdict::Unchecked(format!("{e:#}")),
            }
        })();
        out.push(Finding { call: call.clone(), listing, verdict });
    }
    out
}

/// The text of a finding, and whether it is a warning.
pub fn describe(f: &Finding, root: &Path) -> (bool, String) {
    let shown = format!("cargo {}", f.call.args.join(" "));
    let place = match canonical(&f.call.cwd).strip_prefix(canonical(root)) {
        Ok(r) if r.as_os_str().is_empty() => String::new(),
        Ok(r) => format!(" (in {})", r.display()),
        Err(_) => format!(" (in {})", f.call.cwd.display()),
    };
    match &f.verdict {
        Verdict::Covered => (false, format!("{shown}{place}: sweep builds all its units")),
        Verdict::Cold { units, entry, env } => {
            let first: Vec<&str> = units.iter().take(NAMES).map(String::as_str).collect();
            let mut text = format!(
                "{shown}{place}: {} units that no sweep command builds (first: {}); every new line builds them cold. \
                 Add to sweep in [workspace.metadata.layr-retest]: {entry}",
                units.len(),
                first.join(", ")
            );
            if !env.is_empty() {
                let vars: Vec<String> = env.iter().map(|(k, v)| format!("{k} = {v:?}")).collect();
                text.push_str(&format!(", and to its env table: {}", vars.join(", ")));
            }
            if selects_packages(&f.listing) {
                text.push_str(
                    ". It selects packages with -p: cargo resolves the features of those packages only, so their \
                     dependencies get other units than a --workspace build; prefer --workspace with --exclude",
                );
            }
            (true, text)
        }
        Verdict::Outside(dir) => (
            true,
            format!(
                "{shown}{place}: builds into {}, out of the cache folder target/; no line takes these units warm",
                dir.display()
            ),
        ),
        Verdict::Unchecked(why) => (false, format!("{shown}{place}: not checked: {why}")),
    }
}

/// Prints the findings of a gate and saves them in the records folder for `layr retest doctor`.
pub fn report(root: &Path, findings: &[Finding]) {
    let mut saved = Vec::new();
    let covered = findings.iter().filter(|f| f.verdict == Verdict::Covered).count();
    for f in findings.iter().filter(|f| f.verdict != Verdict::Covered) {
        let (warn, text) = describe(f, root);
        eprintln!("layr-retest gate: {text}");
        saved.push(json!({ "warn": warn, "text": text }));
    }
    if findings.is_empty() {
        eprintln!("layr-retest gate: the command made no cargo call that builds");
    } else if covered == findings.len() {
        eprintln!("layr-retest gate: sweep covers the units of all {covered} cargo calls of this gate");
    }
    let doc = json!({
        "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        "calls": findings.len(),
        "covered": covered,
        "findings": saved,
    });
    let dir = root.join(crate::record::RECORD_DIR);
    let _ = std::fs::create_dir_all(&dir);
    if let Err(e) = std::fs::write(dir.join(REPORT_FILE), serde_json::to_string_pretty(&doc).unwrap_or_default()) {
        eprintln!("layr-retest gate: cannot save {REPORT_FILE}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn build_commands_keep_their_unit_options_and_drop_the_output_options() {
        assert_eq!(
            listing_args(&v(&["build", "--workspace", "-j", "4", "--message-format=json", "--lib", "-v"])),
            Some(v(&["build", "--workspace", "--lib"]))
        );
        // The arguments after `--` reach clippy and stay.
        assert_eq!(
            listing_args(&v(&["clippy", "--all-targets", "--keep-going", "--", "-D", "warnings"])),
            Some(v(&["clippy", "--all-targets", "--", "-D", "warnings"]))
        );
        assert_eq!(listing_args(&v(&["+nightly", "check", "-j8"])), Some(v(&["+nightly", "check"])));
        assert_eq!(listing_args(&v(&["run", "--bin", "app", "--", "--port", "1"])), Some(v(&["build", "--bin", "app"])));
    }

    #[test]
    fn test_and_nextest_become_a_test_build() {
        assert_eq!(
            listing_args(&v(&["test", "--workspace", "--exclude", "x", "some_filter", "--", "--nocapture"])),
            Some(v(&["test", "--workspace", "--exclude", "x", "--no-run"]))
        );
        // nextest's `--profile` is its own; `--cargo-profile` is cargo's.
        assert_eq!(
            listing_args(&v(&[
                "nextest",
                "run",
                "--workspace",
                "--profile",
                "ci",
                "-E",
                "package(a)",
                "--cargo-profile",
                "fast",
                "--no-fail-fast"
            ])),
            Some(v(&["test", "--workspace", "--profile", "fast", "--no-run"]))
        );
        assert_eq!(
            listing_args(&v(&["test", "--locked", "-p", "a", "--doc", "Filter"])),
            Some(v(&["build", "--lib", "--locked", "-p", "a"]))
        );
    }

    #[test]
    fn calls_that_build_nothing_are_left_out() {
        for args in [
            &["fetch", "--locked"][..],
            &["metadata", "--format-version", "1"],
            &["--version"],
            &["fmt", "--check"],
            &["nextest", "show-config"],
        ] {
            assert_eq!(listing_args(&v(args)), None, "{args:?}");
        }
    }

    #[test]
    fn package_selection_is_seen() {
        assert!(selects_packages(&v(&["build", "--lib", "-p", "a"])));
        assert!(selects_packages(&v(&["build", "--package=a"])));
        assert!(!selects_packages(&v(&["clippy", "--workspace", "--", "-p"])));
    }

    #[test]
    fn the_entry_names_the_folder_and_the_other_env() {
        let project = vec![("CARGO_INCREMENTAL".to_string(), "1".to_string())];
        assert_eq!(entry_of(&v(&["check"]), "", &project, &project), (json!(["check"]), BTreeMap::new()));
        // A call into `target` with another variable: the variable goes to the env table.
        let call_env = vec![("CARGO_INCREMENTAL".to_string(), "1".to_string()), ("CLIPPY_CONF_DIR".to_string(), "c".to_string())];
        assert_eq!(
            entry_of(&v(&["clippy"]), "", &call_env, &project),
            (json!(["clippy"]), BTreeMap::from([("CLIPPY_CONF_DIR".to_string(), "c".to_string())]))
        );
        // A call into a folder below `target`: an object with its own env.
        assert_eq!(
            entry_of(&v(&["check", "-p", "a"]), "other", &call_env, &project),
            (json!({"target_dir": "other", "args": ["check", "-p", "a"], "env": {"CLIPPY_CONF_DIR": "c"}}), BTreeMap::new())
        );
    }

    #[test]
    fn the_script_records_only_top_level_calls() {
        assert!(SCRIPT.contains("[ -z \"${CARGO-}\" ]"));
        assert!(SCRIPT.ends_with("exec \"$LAYR_RETEST_REAL_CARGO\" \"$@\"\n"));
    }

    #[test]
    fn calls_are_read_once_each() {
        let dir = std::env::temp_dir().join(format!("layr-retest-cover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.jsonl");
        let line = r#"{"cwd":"/p","target_dir":null,"env":{"RUSTFLAGS":"-x"},"args":["build"]}"#;
        std::fs::write(&log, format!("{line}\n{line}\nnot json\n")).unwrap();
        let calls = read_calls(&log);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].env, vec![("RUSTFLAGS".to_string(), "-x".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
