//! `layr retest init`: make a project ready for `layr retest run` and `layr retest gate`. Safe to run twice.
//! Edits the project files that are safe to edit and saves the cache folders of the line. Never overwrites a
//! value that the project set.

use crate::record::RECORD_DIR;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

/// The standard commands of a cargo project: the default `sweep` of `[workspace.metadata.layr-retest]`. The debug
/// build, the test build and the release build. A profile folder that no command lists is never swept.
const SWEEP_DEFAULT: &[&[&str]] =
    &[&["build", "--locked"], &["test", "--locked", "--no-run"], &["build", "--release", "--locked"]];
/// Default `env` of `[workspace.metadata.layr-retest]`: incremental compilation, faster for a small change. A rustc
/// wrapper (such as sccache, set in `~/.cargo/config.toml`) refuses incremental compilation, so it is cleared.
const ENV_DEFAULT: &[(&str, &str)] = &[("CARGO_INCREMENTAL", "1"), ("RUSTC_WRAPPER", "")];

/// What the project folder uses.
#[derive(Debug, Default, PartialEq)]
pub struct Found {
    pub cargo: bool,
    pub pnpm: bool,
    pub vitest: bool,
    pub jest: bool,
    pub pytest: bool,
}

impl Found {
    /// A tool that writes records into `.layr-retest`.
    fn tests(&self) -> bool {
        self.cargo || self.vitest || self.jest || self.pytest
    }
}

pub fn detect(root: &Path) -> Found {
    let read = |n: &str| std::fs::read_to_string(root.join(n)).unwrap_or_default();
    let pkg = read("package.json");
    let py = format!("{}{}{}", read("pyproject.toml"), read("requirements.txt"), read("requirements-dev.txt"));
    Found {
        cargo: root.join("Cargo.toml").is_file(),
        pnpm: root.join("pnpm-lock.yaml").is_file(),
        vitest: pkg.contains("\"vitest\""),
        jest: pkg.contains("\"jest\""),
        pytest: root.join("pytest.ini").is_file() || py.contains("pytest"),
    }
}

/// `text` with `entry` as a last line, or `None` when a line with the same words exists.
/// For `.gitignore`, `accept` lists the other spellings of the same rule.
pub fn add_line(text: &str, entry: &str, accept: &[&str]) -> Option<String> {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.lines().any(|l| words(l) == words(entry) || accept.iter().any(|a| words(a) == words(l))) {
        return None;
    }
    let nl = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    Some(format!("{text}{nl}{entry}\n"))
}

/// Edit a file, creating it when missing. Returns whether it needs the change. `write` false (a protected line)
/// leaves the file as it is.
fn ensure(path: &Path, entry: &str, accept: &[&str], write: bool) -> Result<bool> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    match add_line(&old, entry, accept) {
        Some(new) if write => std::fs::write(path, new).map(|_| true).map_err(Into::into),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

/// The current line, from `layr line list`.
fn current_line() -> Option<String> {
    let (_, lines, _) = layr_all(&["line", "list"]).ok()?;
    lines.lines().find_map(|l| l.strip_prefix("* ")).map(|l| l.trim().to_string())
}

/// Whether `line` has a protection rule (`layr protect` lists one rule per line, the line name first).
fn protected(line: &str) -> bool {
    layr_all(&["protect"]).is_ok_and(|(_, out, _)| out.lines().any(|l| l.split('\t').next() == Some(line)))
}

/// `text` with the keys of `[workspace.metadata.layr-retest]` that are missing (`sweep`, `env`) added. A key
/// that is set keeps its value. Returns the new text and the added keys, or `None` when nothing is missing.
/// The table `[workspace]` is created when the manifest has none; a package-only manifest then becomes a
/// workspace of one member.
pub fn add_retest_config(text: &str) -> Result<Option<(String, Vec<&'static str>)>> {
    let mut doc: DocumentMut = text.parse().context("Cargo.toml is not valid TOML")?;
    let implicit = || {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    };
    let ws = doc.entry("workspace").or_insert(implicit()).as_table_mut().context("[workspace] is not a table")?;
    let meta = ws.entry("metadata").or_insert(implicit()).as_table_mut().context("[workspace.metadata] is not a table")?;
    let cfg = meta
        .entry("layr-retest")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("[workspace.metadata.layr-retest] is not a table")?;
    let mut added = Vec::new();
    if !cfg.contains_key("sweep") {
        let mut sweep = Array::new();
        for c in SWEEP_DEFAULT {
            let mut command = Array::new();
            for a in *c {
                command.push(*a);
            }
            sweep.push(Value::Array(command));
        }
        cfg.insert("sweep", Item::Value(Value::Array(sweep)));
        added.push("sweep");
    }
    if !cfg.contains_key("env") {
        let mut env = InlineTable::new();
        for (k, v) in ENV_DEFAULT {
            env.insert(*k, Value::from(*v));
        }
        cfg.insert("env", Item::Value(Value::InlineTable(env)));
        added.push("env");
    }
    Ok((!added.is_empty()).then(|| (doc.to_string(), added)))
}

/// Edits the manifest at `path` with [`add_retest_config`]. Returns the added keys, if any.
fn add_retest_config_file(path: &Path, write: bool) -> Result<Option<Vec<&'static str>>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    match add_retest_config(&text)? {
        Some((_, added)) if !write => Ok(Some(added)),
        Some((new, added)) => {
            std::fs::write(path, new).with_context(|| format!("cannot write {}", path.display()))?;
            Ok(Some(added))
        }
        None => Ok(None),
    }
}

/// Folders of the line that are cache overlays, from `layr overlay list --json`. An error when the folder is
/// not a layr line.
fn cache_folders() -> Result<Vec<String>> {
    let out = crate::layr(&["overlay", "list", "--json"])?;
    let overlays: Vec<serde_json::Value> = serde_json::from_str(out.trim()).context("layr overlay list: not JSON")?;
    Ok(overlays.iter().filter_map(|o| o["folder"].as_str().map(String::from)).collect())
}

/// A missing tool: what it is and the command that installs it.
pub struct Missing {
    pub what: &'static str,
    pub install: &'static str,
}

pub fn missing_tools() -> Vec<Missing> {
    let mut v = Vec::new();
    let nextest = layr_cache::cargo::cargo_cmd().args(["nextest", "--version"]).output().is_ok_and(|o| o.status.success());
    if !nextest {
        v.push(Missing { what: "cargo-nextest", install: "cargo install cargo-nextest --locked" });
    }
    if crate::llvm_tool("llvm-profdata").is_err() || crate::llvm_tool("llvm-cov").is_err() {
        v.push(Missing { what: "llvm-tools for the active toolchain", install: "rustup component add llvm-tools" });
    }
    v
}

pub fn run(args: &[String]) -> Result<i32> {
    let yes = args.iter().any(|a| a == "--yes");
    if let Some(bad) = args.iter().find(|a| *a != "--yes") {
        anyhow::bail!("unknown option {bad}; usage: layr retest init [--yes]");
    }
    let root = std::env::current_dir()?;
    let f = detect(&root);
    let tools: Vec<&str> =
        [(f.cargo, "cargo workspace"), (f.pnpm, "pnpm"), (f.vitest, "vitest"), (f.jest, "jest"), (f.pytest, "pytest")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, n)| *n)
            .collect();
    if tools.is_empty() {
        eprintln!("layr retest init: no Cargo.toml, pnpm-lock.yaml, vitest, jest or pytest here; run it in the project root");
        return Ok(1);
    }
    println!("found: {}", tools.join(", "));
    let mut code = 0;
    if f.cargo {
        for m in missing_tools() {
            if !yes {
                println!("missing {}: run `{}` (or use --yes)", m.what, m.install);
                code = 1;
                continue;
            }
            println!("installing {}: {}", m.what, m.install);
            let mut w = m.install.split_whitespace();
            match Command::new(w.next().unwrap()).args(w).status() {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    println!("install failed: `{}` exited with {s} (its output is above)", m.install);
                    code = 1;
                }
                Err(e) => {
                    println!("install failed: cannot run `{}`: {e}", m.install);
                    code = 1;
                }
            }
        }
    }

    // Cache folders of the project: build output, packages, and the records of the tests.
    let mut folders: Vec<&str> = Vec::new();
    if f.cargo {
        folders.push("target");
    }
    if f.pnpm {
        folders.push("node_modules");
    }
    if f.tests() {
        folders.push(RECORD_DIR);
    }
    // A protected line takes files only through a merge: init writes no file there and names the files that need
    // the change.
    let line = current_line();
    let guarded = line.as_deref().is_some_and(protected);
    let write = !guarded;
    let added = if write { "added" } else { "needs" };
    // The files that init changed. A line takes them from a state, so they are saved once (printed below).
    let mut edited: Vec<&str> = Vec::new();
    for folder in &folders {
        // A rule for the root folder only (`/target`) names the same cache folder.
        let root_only = [format!("/{folder} regenerable"), format!("{folder}/ regenerable"), format!("/{folder}/ regenerable")];
        let accept: Vec<&str> = root_only.iter().map(String::as_str).collect();
        let changed = ensure(&root.join(".layrattributes"), &format!("{folder} regenerable"), &accept, write)?;
        println!(
            ".layrattributes: {}",
            if changed { format!("{added} `{folder} regenerable`") } else { format!("`{folder} regenerable` present") }
        );
        if changed && !edited.contains(&".layrattributes") {
            edited.push(".layrattributes");
        }
    }
    if f.tests() {
        let changed = ensure(&root.join(".gitignore"), RECORD_DIR, &["/.layr-retest", ".layr-retest/", "/.layr-retest/"], write)?;
        println!(".gitignore: {}", if changed { format!("{added} `{RECORD_DIR}`") } else { format!("`{RECORD_DIR}` present") });
        if changed {
            edited.push(".gitignore");
        }
    }
    if f.cargo {
        match add_retest_config_file(&root.join("Cargo.toml"), write)? {
            Some(keys) => {
                println!("Cargo.toml: {added} {} in [workspace.metadata.layr-retest]", keys.join(", "));
                edited.push("Cargo.toml");
            }
            None => println!("Cargo.toml: [workspace.metadata.layr-retest] keys present"),
        }
    }
    if !edited.is_empty() && guarded {
        let name = line.as_deref().unwrap_or("main");
        println!("line {name} is protected, so init changed no file. Run init in a line, save, and merge:");
        println!("      layr line new retest-init {name} && cd ../retest-init && layr retest init");
        println!("      layr add {} && layr save -m \"retest init\"", edited.join(" "));
        code = 1;
    } else if !edited.is_empty() {
        println!("save: layr add {} && layr save -m \"retest init\"", edited.join(" "));
        println!("      (lines take the caches only after this save; in a protected main, save in a line and merge)");
    }

    // A cache folder that exists is saved now as a cache overlay, so a new line starts warm. A folder made later
    // becomes one at the next `layr save` (the layer's own rule, no init needed).
    let present: Vec<&str> = folders.iter().copied().filter(|f| root.join(f).is_dir()).collect();
    for folder in folders.iter().filter(|f| !present.contains(*f)) {
        println!("{folder}: not there yet; the next `layr save` after a build makes it a cache");
    }
    if guarded && !present.is_empty() {
        // A protected line refuses a save; its follow pass saves the caches after each merge.
        println!("caches: line {} is protected; the follow pass saves its caches", line.as_deref().unwrap_or("main"));
    } else if !present.is_empty() {
        match cache_folders() {
            Err(e) => {
                println!("cache folders not saved, this is not a layr line: {e:#}");
                code = 1;
            }
            Ok(saved) => {
                for folder in present {
                    if saved.iter().any(|s| s == folder) {
                        println!("{folder}: already a cache overlay");
                    } else {
                        match crate::save_folder(folder) {
                            Ok(_) => println!("{folder}: saved as cache overlay"),
                            Err(e) => {
                                println!("{folder}: not saved: {e:#}");
                                code = 1;
                            }
                        }
                    }
                }
            }
        }
    }
    follow_main(&mut code)?;
    println!("next: layr retest gate -- <your gate command>");
    println!("      layr retest doctor checks the project");
    Ok(code)
}

/// Runs layr: success, stdout and stderr. (`crate::layr` returns the stdout of a success only.)
fn layr_all(args: &[&str]) -> Result<(bool, String, String)> {
    let out = Command::new(std::env::current_exe()?).args(args).output().context("cannot run layr")?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Sets the target `origin/main` of the main line when the project has an `origin` remote, and runs its first pass.
/// Idempotent: a main line that already follows it is left alone. Other lines get no target. Sets `code` to 1 when
/// the pass fails.
fn follow_main(code: &mut i32) -> Result<()> {
    let (_, lines, _) = layr_all(&["line", "list"])?;
    let Some(current) = lines.lines().find_map(|l| l.strip_prefix("* ")).map(str::trim) else {
        println!("follow: cannot read the current line; run `layr retest init` in the main line");
        return Ok(());
    };
    if current != "main" {
        println!("follow: line {current} is not main; run `layr retest init` in main to follow origin/main");
        return Ok(());
    }
    let (_, remotes, _) = layr_all(&["git", "remote"])?;
    if !remotes.lines().any(|r| r.trim() == "origin") {
        println!("follow: no git remote origin; main follows nothing");
        return Ok(());
    }
    let (_, show, _) = layr_all(&["follow"])?;
    if show.lines().any(|l| l.trim() == "Following origin/main on line main") {
        println!("follow: main already follows origin/main");
        return Ok(());
    }
    let (ok, out, err) = layr_all(&["follow", "origin/main"])?;
    print!("{out}");
    if ok {
        println!("follow: main follows origin/main");
        println!("follow: each pass builds the sweep commands in main and saves its caches; a line's caches go back to main after `layr git push` of the line or a local `layr merge`");
    } else {
        println!("follow: origin/main not set: {}", err.trim());
        *code = 1;
    }
    if err.contains("no layr daemon runs") {
        println!("follow: no daemon runs, so passes run only by hand; install it: docs/follow.md (Install on macOS, Linux)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_a_missing_line_once() {
        let a = add_line("a\n", "target regenerable", &[]).unwrap();
        assert_eq!(a, "a\ntarget regenerable\n");
        assert_eq!(add_line(&a, "target regenerable", &[]), None);
    }

    #[test]
    fn adds_a_newline_before_the_entry() {
        assert_eq!(add_line("a", "b", &[]).unwrap(), "a\nb\n");
        assert_eq!(add_line("", "b", &[]).unwrap(), "b\n");
    }

    #[test]
    fn spacing_does_not_matter() {
        assert_eq!(add_line("target   regenerable\n", "target regenerable", &[]), None);
    }

    #[test]
    fn a_root_only_rule_counts() {
        let accept = ["/target regenerable", "target/ regenerable", "/target/ regenerable"];
        assert_eq!(add_line("/target  regenerable\n", "target regenerable", &accept), None);
        assert_eq!(add_line("/target/ regenerable\n", "target regenerable", &accept), None);
        assert!(add_line("/targets regenerable\n", "target regenerable", &accept).is_some());
    }

    #[test]
    fn other_gitignore_spellings_count() {
        let accept = ["/.layr-retest", ".layr-retest/"];
        assert_eq!(add_line("/target\n/.layr-retest\n", ".layr-retest", &accept), None);
        assert!(add_line("/target\n", ".layr-retest", &accept).is_some());
    }

    #[test]
    fn ensure_creates_and_is_idempotent() {
        let d = std::env::temp_dir().join(format!("layr-retest-init-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(".layrattributes");
        assert!(ensure(&p, "target regenerable", &[], true).unwrap());
        assert!(ensure(&p, "node_modules regenerable", &[], true).unwrap());
        assert!(!ensure(&p, "target regenerable", &[], true).unwrap());
        assert!(!ensure(&p, "node_modules regenerable", &[], true).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "target regenerable\nnode_modules regenerable\n");
        // A protected line: the change is reported, the file stays.
        assert!(ensure(&p, ".layr-retest regenerable", &[], false).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "target regenerable\nnode_modules regenerable\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn detects_tools() {
        let d = std::env::temp_dir().join(format!("layr-retest-detect-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(detect(&d), Found::default());
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::write(d.join("package.json"), "{\"devDependencies\":{\"vitest\":\"1\"}}").unwrap();
        std::fs::write(d.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(d.join("pytest.ini"), "").unwrap();
        assert_eq!(detect(&d), Found { cargo: true, pnpm: true, vitest: true, jest: false, pytest: true });
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_package_manifest_gets_the_defaults() {
        let (out, added) = add_retest_config("[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap().unwrap();
        assert_eq!(added, ["sweep", "env"]);
        assert!(out.contains("[workspace.metadata.layr-retest]"), "{out}");
        assert!(
            out.contains(
                "sweep = [[\"build\", \"--locked\"], [\"test\", \"--locked\", \"--no-run\"], [\"build\", \"--release\", \"--locked\"]]"
            ),
            "{out}"
        );
        assert!(out.contains("env = { CARGO_INCREMENTAL = \"1\", RUSTC_WRAPPER = \"\" }"), "{out}");
        assert_eq!(add_retest_config(&out).unwrap(), None, "a second run changes nothing");
    }

    #[test]
    fn the_project_values_stay() {
        let text = "[workspace]\nmembers = [\"a\"]\n\n[workspace.metadata.layr-retest]\n# keep me\nenv = { LAYR_X = \"1\" }\n";
        let (out, added) = add_retest_config(text).unwrap().unwrap();
        assert_eq!(added, ["sweep"]);
        assert!(out.contains("# keep me") && out.contains("env = { LAYR_X = \"1\" }"), "{out}");
        assert!(!out.contains("CARGO_INCREMENTAL"), "{out}");
    }

    #[test]
    fn a_complete_config_is_left_alone() {
        let text = "[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [[\"test\", \"--no-run\"]]\nenv = {}\n";
        assert_eq!(add_retest_config(text).unwrap(), None);
    }
}
