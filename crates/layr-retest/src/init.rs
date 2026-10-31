//! `layr retest init`: make a project ready for `layr retest run`. Safe to run twice.

use anyhow::Result;
use std::path::Path;
use std::process::Command;

/// Line of `.layrattributes` for a folder that a tool rebuilds and layr may drop.
const ATTR_TARGET: &str = "target regenerable";
const ATTR_RECORDS: &str = ".layr-retest regenerable";

/// What the project folder uses.
#[derive(Debug, Default, PartialEq)]
pub struct Found {
    pub cargo: bool,
    pub vitest: bool,
    pub jest: bool,
    pub pytest: bool,
}

pub fn detect(root: &Path) -> Found {
    let read = |n: &str| std::fs::read_to_string(root.join(n)).unwrap_or_default();
    let pkg = read("package.json");
    let py = format!("{}{}{}", read("pyproject.toml"), read("requirements.txt"), read("requirements-dev.txt"));
    Found {
        cargo: root.join("Cargo.toml").is_file(),
        vitest: pkg.contains("\"vitest\""),
        jest: pkg.contains("\"jest\""),
        pytest: root.join("pytest.ini").is_file() || py.contains("pytest"),
    }
}

/// `text` with `entry` as a last line, or `None` when a line with the same words exists.
/// For `.gitignore`, `accept` lists the other spellings of the same rule.
pub fn add_line(text: &str, entry: &str, accept: &[&str]) -> Option<String> {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.lines().any(|l| words(l) == words(entry) || accept.contains(&l.trim())) {
        return None;
    }
    let nl = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    Some(format!("{text}{nl}{entry}\n"))
}

/// Edit a file, creating it when missing. Returns whether it changed.
fn ensure(path: &Path, entry: &str, accept: &[&str]) -> Result<bool> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    match add_line(&old, entry, accept) {
        Some(new) => std::fs::write(path, new).map(|_| true).map_err(Into::into),
        None => Ok(false),
    }
}

/// A missing tool: what it is and the command that installs it.
pub struct Missing {
    pub what: &'static str,
    pub install: &'static str,
}

pub fn missing_tools() -> Vec<Missing> {
    let mut v = Vec::new();
    let nextest = Command::new("cargo").args(["nextest", "--version"]).output().is_ok_and(|o| o.status.success());
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
    let tools: Vec<&str> = [(f.cargo, "cargo workspace"), (f.vitest, "vitest"), (f.jest, "jest"), (f.pytest, "pytest")]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| *n)
        .collect();
    if tools.is_empty() {
        eprintln!("layr retest init: no Cargo.toml, vitest, jest or pytest here; run it in the project root");
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
            let ok = Command::new(w.next().unwrap()).args(w).status().is_ok_and(|s| s.success());
            if !ok {
                println!("install failed: {}", m.install);
                code = 1;
            }
        }
    }
    for (file, entry, accept) in [
        (".layrattributes", ATTR_TARGET, &[][..]),
        (".layrattributes", ATTR_RECORDS, &[][..]),
        (".gitignore", ".layr-retest", &["/.layr-retest", ".layr-retest/", "/.layr-retest/"][..]),
    ] {
        let changed = ensure(&root.join(file), entry, accept)?;
        println!("{file}: {}", if changed { format!("added `{entry}`") } else { format!("`{entry}` present") });
    }
    println!("gate command:");
    if f.cargo {
        println!("  layr retest run -- --workspace");
    }
    for (on, t) in [(f.vitest, "vitest"), (f.jest, "jest"), (f.pytest, "pytest")] {
        if on {
            println!("  layr retest run --tool {t}");
        }
    }
    println!("next: `layr retest doctor` checks the project");
    Ok(code)
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
        assert!(ensure(&p, ATTR_TARGET, &[]).unwrap());
        assert!(ensure(&p, ATTR_RECORDS, &[]).unwrap());
        assert!(!ensure(&p, ATTR_TARGET, &[]).unwrap());
        assert!(!ensure(&p, ATTR_RECORDS, &[]).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "target regenerable\n.layr-retest regenerable\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn detects_tools() {
        let d = std::env::temp_dir().join(format!("layr-retest-detect-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(detect(&d), Found::default());
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::write(d.join("package.json"), "{\"devDependencies\":{\"vitest\":\"1\"}}").unwrap();
        std::fs::write(d.join("pytest.ini"), "").unwrap();
        assert_eq!(detect(&d), Found { cargo: true, vitest: true, jest: false, pytest: true });
        std::fs::remove_dir_all(&d).unwrap();
    }
}
