//! `layr retest doctor`: read-only checks of a project, with advice and numbers.
//! Exit code 0 when all is fine, 1 when there is a warning.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A record older than this many days is a warning: a run after it selects many tests.
const OLD_BASELINE_DAYS: u64 = 14;
/// `target/retest` above this many bytes is a warning.
const BIG_RETEST_BYTES: u64 = 20 << 30;

#[derive(Debug, PartialEq)]
pub struct Finding {
    pub warn: bool,
    pub text: String,
}

fn ok(text: impl Into<String>) -> Finding {
    Finding { warn: false, text: text.into() }
}
fn warn(text: impl Into<String>) -> Finding {
    Finding { warn: true, text: text.into() }
}

pub fn fmt_size(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{} KB", b >> 10),
    }
}

pub fn fmt_age(secs: u64) -> String {
    match secs {
        s if s >= 86400 => format!("{} days", s / 86400),
        s if s >= 3600 => format!("{} hours", s / 3600),
        s => format!("{} minutes", s / 60),
    }
}

/// Major version in `rustc -vV` ("LLVM version: 20.1.5") or `llvm-profdata --version` ("LLVM version 20.1.5").
pub fn llvm_major(text: &str) -> Option<u32> {
    let rest = text.lines().find_map(|l| l.trim().strip_prefix("LLVM version"))?;
    rest.trim_start_matches(':').trim().split('.').next()?.parse().ok()
}

/// Lines of a test source that start a workspace bin by a path or by an env variable that is not
/// `env!("CARGO_BIN_EXE_<name>")`. Cargo builds such a bin without coverage, so a change of its code
/// selects nothing and forces a full run. Returns (line number, line).
pub fn bin_path_hits(src: &str, bins: &[String]) -> Vec<(usize, String)> {
    let names: Vec<String> = bins.iter().map(|b| b.to_uppercase().replace('-', "_")).collect();
    let mut out = Vec::new();
    for (i, l) in src.lines().enumerate() {
        let t = l.trim();
        if t.starts_with("//") || t.contains("CARGO_BIN_EXE_") {
            continue;
        }
        let hard = t.contains("target/debug") || t.contains("target/release");
        let env_bin = ["env::var(\"", "env::var_os(\"", "option_env!(\""].iter().any(|p| {
            t.split(p).skip(1).any(|rest| {
                let name = rest.split('"').next().unwrap_or("");
                let word = |w: &str| matches!(w, "BIN" | "BINARY" | "EXE" | "EXECUTABLE");
                !name.starts_with("CARGO_") && (name.split('_').any(word) || names.iter().any(|n| name == n))
            })
        });
        if hard || env_bin {
            out.push((i + 1, t.to_string()));
        }
    }
    out
}

/// The part of a source that holds tests: all of a file under `tests/`, else from `#[cfg(test)]` on.
fn test_part<'a>(path: &Path, src: &'a str) -> &'a str {
    if path.components().any(|c| c.as_os_str() == "tests") {
        return src;
    }
    src.find("\n#[cfg(test)]").map_or("", |i| &src[i..])
}

/// One crate's test binaries: the integration tests (each is one more binary to link) and their sizes.
pub struct CrateBins {
    pub package: String,
    pub tests: Vec<(String, u64)>,
}

/// Advice for a crate with more than one integration test binary. The saving is the size of all but
/// the largest one: merged, their code links into one binary (`autotests = false` and one `tests/main.rs`).
pub fn merge_advice(c: &CrateBins) -> Option<Finding> {
    if c.tests.len() < 2 {
        return None;
    }
    let total: u64 = c.tests.iter().map(|t| t.1).sum();
    let saving = total - c.tests.iter().map(|t| t.1).max().unwrap_or(0);
    Some(warn(format!(
        "{}: {} integration test binaries, {} in all; one binary (autotests = false, tests/main.rs with mod lines) saves {} of disk and {} links",
        c.package,
        c.tests.len(),
        fmt_size(total),
        fmt_size(saving),
        c.tests.len() - 1
    )))
}

fn out(cmd: &mut Command) -> Option<String> {
    let o = cmd.output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn rust_files(dir: &Path, v: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() {
            if !matches!(name.to_str(), Some("target" | ".git" | "node_modules" | ".layr-retest")) {
                rust_files(&p, v);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            v.push(p);
        }
    }
}

/// The newest executable `<name>-<hash>` in the deps folder of the instrumented or the plain build.
fn test_binary_size(root: &Path, name: &str) -> Option<u64> {
    let stem = name.replace('-', "_");
    ["target/retest/debug/deps", "target/debug/deps"].iter().find_map(|d| {
        std::fs::read_dir(root.join(d))
            .ok()?
            .flatten()
            .filter(|e| {
                let f = e.file_name().to_string_lossy().into_owned();
                f.strip_prefix(&format!("{stem}-")).is_some_and(|h| h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()))
            })
            .filter_map(|e| e.metadata().ok().filter(|m| m.is_file()).map(|m| (m.modified().ok(), m.len())))
            .max()
            .map(|m| m.1)
    })
}

fn tool_checks(f: &mut Vec<Finding>) {
    match out(Command::new("cargo").args(["nextest", "--version"])) {
        Some(v) => f.push(ok(format!("cargo-nextest: {}", v.lines().next().unwrap_or("").trim()))),
        None => f.push(warn("cargo-nextest is missing; run: cargo install cargo-nextest --locked")),
    }
    let rustc = out(Command::new("rustc").arg("-vV")).unwrap_or_default();
    let rustc_llvm = llvm_major(&rustc);
    match crate::llvm_tool("llvm-profdata").and_then(|_| crate::llvm_tool("llvm-cov")) {
        Err(_) => f.push(warn("llvm-tools is missing for the active toolchain; run: rustup component add llvm-tools")),
        Ok(_) => {
            let prof = crate::llvm_tool("llvm-profdata").ok().and_then(|p| out(Command::new(p).arg("--version")));
            match (rustc_llvm, prof.as_deref().and_then(llvm_major)) {
                (Some(a), Some(b)) if a == b => f.push(ok(format!("llvm-tools: LLVM {b}, same as rustc"))),
                (Some(a), Some(b)) => f.push(warn(format!(
                    "llvm-tools is LLVM {b} but rustc uses LLVM {a}; run: rustup component add llvm-tools (for the active toolchain)"
                ))),
                _ => f.push(warn("cannot read the LLVM version of rustc or llvm-profdata")),
            }
        }
    }
}

/// Absolute `rerun-if-changed` paths under `root` in the build script outputs of `target_dir`
/// (`<target_dir>/build/<package>-<hash>/output`). Returns `(package, path)`, and whether any
/// build output was read. Every line has its own absolute path, so such a path makes cargo
/// rebuild the build script and all crates after it in each new line.
pub fn absolute_rerun_paths(target_dir: &Path, root: &Path) -> (Vec<(String, String)>, bool) {
    let mut hits = Vec::new();
    let mut seen = false;
    let Ok(rd) = std::fs::read_dir(target_dir.join("build")) else { return (hits, seen) };
    let mut dirs: Vec<_> = rd.flatten().collect();
    dirs.sort_by_key(|e| e.file_name());
    for e in dirs {
        let Ok(text) = std::fs::read_to_string(e.path().join("output")) else { continue };
        seen = true;
        let name = e.file_name().to_string_lossy().into_owned();
        let package = name.rsplit_once('-').map_or(name.as_str(), |(p, _)| p).to_string();
        for line in text.lines() {
            let Some(v) = line.strip_prefix("cargo:rerun-if-changed=").or_else(|| line.strip_prefix("cargo::rerun-if-changed="))
            else {
                continue;
            };
            if Path::new(v).is_absolute() && Path::new(v).starts_with(root) {
                let hit = (package.clone(), v.to_string());
                if !hits.contains(&hit) {
                    hits.push(hit);
                }
            }
        }
    }
    (hits, seen)
}

fn rerun_checks(root: &Path, f: &mut Vec<Finding>) {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut seen = false;
    let mut bad = false;
    let mut reported: Vec<(String, String)> = Vec::new();
    for dir in ["target/debug", "target/retest/debug"] {
        let d = root.join(dir);
        let (mut hits, s) = absolute_rerun_paths(&d, root);
        if canon != root {
            hits.extend(absolute_rerun_paths(&d, &canon).0);
        }
        seen |= s;
        for (package, path) in hits {
            bad = true;
            if !reported.contains(&(package.clone(), path.clone())) {
                reported.push((package.clone(), path.clone()));
            } else {
                continue;
            }
            f.push(warn(format!(
                "{package}: build script prints an absolute rerun-if-changed path {path}; \
                 print a path relative to CARGO_MANIFEST_DIR (an absolute path rebuilds it in every new line)"
            )));
        }
    }
    if !seen {
        f.push(ok("build once to check build script paths"));
    } else if !bad {
        f.push(ok("no build script prints an absolute rerun-if-changed path"));
    }
}

pub fn check(root: &Path) -> Vec<Finding> {
    let mut f = Vec::new();
    if !root.join("Cargo.toml").is_file() {
        f.push(warn("no Cargo.toml here; run in the project root"));
        return f;
    }
    tool_checks(&mut f);

    let meta: Option<Value> = out(Command::new("cargo").args(["metadata", "--no-deps", "--format-version", "1"]))
        .and_then(|s| serde_json::from_str(&s).ok());
    let Some(meta) = meta else {
        f.push(warn("cargo metadata failed; fix the workspace first"));
        return f;
    };
    let mut bins = Vec::new();
    let mut crates = Vec::new();
    let mut total_bins = 0;
    for p in meta["packages"].as_array().into_iter().flatten() {
        let mut c = CrateBins { package: p["name"].as_str().unwrap_or("").into(), tests: Vec::new() };
        for t in p["targets"].as_array().into_iter().flatten() {
            let kinds: Vec<&str> = t["kind"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).collect();
            let name = t["name"].as_str().unwrap_or("");
            if kinds.contains(&"bin") {
                bins.push(name.to_string());
            }
            if kinds.contains(&"test") {
                c.tests.push((name.to_string(), test_binary_size(root, name).unwrap_or(0)));
            }
            if kinds.iter().any(|k| matches!(*k, "test" | "lib" | "bin")) {
                total_bins += 1;
            }
        }
        crates.push(c);
    }
    let sizes: u64 = crates.iter().flat_map(|c| c.tests.iter().map(|t| t.1)).sum();
    f.push(ok(format!(
        "{total_bins} test binaries (lib, bin and integration targets); integration test binaries on disk: {}",
        fmt_size(sizes)
    )));
    let advice: Vec<Finding> = crates.iter().filter_map(merge_advice).collect();
    f.extend(advice);

    let mut files = Vec::new();
    rust_files(root, &mut files);
    let mut hits = 0;
    for p in files {
        let Ok(src) = std::fs::read_to_string(&p) else { continue };
        let part = test_part(&p, &src);
        let skipped = src[..src.len() - part.len()].matches('\n').count();
        for (n, l) in bin_path_hits(part, &bins) {
            hits += 1;
            f.push(warn(format!(
                "{}:{}: starts a bin by path or env variable, not instrumented, forces full runs; use env!(\"CARGO_BIN_EXE_<name>\"): {l}",
                p.strip_prefix(root).unwrap_or(&p).display(),
                n + skipped
            )));
        }
    }
    if hits == 0 {
        f.push(ok("no test starts a workspace bin by a path or an env variable"));
    }

    let recs: Vec<_> = std::fs::read_dir(root.join(".layr-retest"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .collect();
    match recs.iter().max() {
        None => f.push(warn("no record in .layr-retest: the first `layr retest run` is a full run")),
        Some(t) => {
            let age = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            let text = format!("{} records, newest is {} old", recs.len(), fmt_age(age));
            f.push(if age / 86400 > OLD_BASELINE_DAYS {
                warn(format!("{text}; run `layr retest run --full` to renew the baseline"))
            } else {
                ok(text)
            });
        }
    }
    let rt = dir_size(&root.join("target/retest"));
    let text = format!("target/retest: {}", fmt_size(rt));
    f.push(if rt > BIG_RETEST_BYTES {
        warn(format!("{text}; delete it to free disk, the next run rebuilds it"))
    } else {
        ok(text)
    });
    rerun_checks(root, &mut f);
    f
}

pub fn run(args: &[String]) -> Result<i32> {
    if let Some(bad) = args.first() {
        anyhow::bail!("unknown option {bad}; usage: layr retest doctor");
    }
    let found = check(&std::env::current_dir()?);
    for x in &found {
        println!("{}: {}", if x.warn { "warn" } else { "ok" }, x.text);
    }
    Ok(found.iter().any(|x| x.warn) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bins() -> Vec<String> {
        vec!["layr-server".into()]
    }

    #[test]
    fn flags_hard_coded_target_path() {
        let h = bin_path_hits(concat!("let p = \"target/", "debug/layr-server\";\n"), &bins());
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, 1);
    }

    #[test]
    fn flags_env_var_that_names_a_bin() {
        assert_eq!(bin_path_hits("let b = std::env::var(\"LAYR_SERVER_BIN\").unwrap();", &bins()).len(), 1);
        assert_eq!(bin_path_hits("let b = std::env::var_os(\"LAYR_SERVER\");", &bins()).len(), 1);
        assert_eq!(bin_path_hits("let b = std::env::var(\"STUDIO_SUPERVISOR_BINARY\");", &bins()).len(), 1);
    }

    #[test]
    fn accepts_cargo_bin_exe_and_unrelated_vars() {
        assert!(bin_path_hits("let b = env!(\"CARGO_BIN_EXE_layr-server\");", &bins()).is_empty());
        assert!(bin_path_hits("let h = std::env::var(\"HOME\");", &bins()).is_empty());
        assert!(bin_path_hits("let h = std::env::var(\"LAYR_TEST_ROOT\");", &["layr".to_string()]).is_empty());
        assert!(bin_path_hits(concat!("// target/", "debug is not used\n"), &bins()).is_empty());
    }

    #[test]
    fn merge_advice_needs_two_binaries() {
        let one = CrateBins { package: "a".into(), tests: vec![("t".into(), 10 << 20)] };
        assert_eq!(merge_advice(&one), None);
        let three = CrateBins {
            package: "a".into(),
            tests: vec![("x".into(), 10 << 20), ("y".into(), 30 << 20), ("z".into(), 20 << 20)],
        };
        let f = merge_advice(&three).unwrap();
        assert!(f.warn);
        assert!(f.text.contains("3 integration test binaries"), "{}", f.text);
        assert!(f.text.contains("60.0 MB in all"), "{}", f.text);
        assert!(f.text.contains("saves 30.0 MB"), "{}", f.text);
    }

    #[test]
    fn flags_absolute_rerun_paths_in_build_output() {
        let t = std::env::temp_dir().join(format!("layr-doctor-rerun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let root = t.join("proj");
        let b = root.join("target/debug/build");
        for (dir, body) in [
            ("good-1a2b", "cargo:rerun-if-changed=schema/api.json\ncargo:rerun-if-changed=/usr/lib/x\n".to_string()),
            ("bad-3c4d", format!("cargo::rerun-if-changed={}/schema/api.json\n", root.display())),
        ] {
            std::fs::create_dir_all(b.join(dir)).unwrap();
            std::fs::write(b.join(dir).join("output"), body).unwrap();
        }
        let (hits, seen) = absolute_rerun_paths(&root.join("target/debug"), &root);
        let _ = std::fs::remove_dir_all(&t);
        assert!(seen);
        assert_eq!(hits, vec![("bad".to_string(), format!("{}/schema/api.json", root.display()))]);
        assert!(!absolute_rerun_paths(&t.join("none"), &root).1);
    }

    #[test]
    fn reads_llvm_versions() {
        assert_eq!(llvm_major("host: x\nLLVM version: 20.1.5\n"), Some(20));
        assert_eq!(llvm_major("LLVM (http://llvm.org/):\n  LLVM version 19.1.7\n"), Some(19));
        assert_eq!(llvm_major("nothing"), None);
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(fmt_size(3 << 20), "3.0 MB");
        assert_eq!(fmt_age(3 * 86400 + 5), "3 days");
        assert_eq!(fmt_age(7200), "2 hours");
    }
}
