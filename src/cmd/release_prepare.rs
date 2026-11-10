//! `layr release prepare`: the next version and its CHANGELOG section from the conventional titles on the line
//! since the last release tag, in the format of release-please (`release-please-config.json`,
//! `.release-please-manifest.json`). It writes the version into Cargo.toml, Cargo.lock and the manifest and puts
//! the section on top of CHANGELOG.md; the caller saves the line and queues it. The tag `v<version>` is set when
//! the release state is on main.

use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use anyhow::{bail, Context, Result};
use std::path::Path;

/// One changelog entry: a conventional header of a commit (its title, or a header line in its body).
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    kind: String,
    scope: Option<String>,
    breaking: bool,
    description: String,
    /// The number of a GitHub pull request (`(#N)` at the end of the title).
    pr: Option<u64>,
    /// A Layr Cloud change (`Layr-Change: <id>` in the body).
    change: Option<String>,
    sha: String,
}

/// The sections of `release-please-config.json`: type, title, hidden.
type Sections = Vec<(String, String, bool)>;

const REPO: &str = "https://github.com/ifdotpy/layr";
const CHANGES: &str = "https://cloud.layr.ws/p/layr/#/changes";

/// `type(scope)!: description`, or None.
fn header(line: &str) -> Option<(String, Option<String>, bool, String)> {
    let (head, description) = line.split_once(": ")?;
    let (head, breaking) = match head.strip_suffix('!') {
        Some(h) => (h, true),
        None => (head, false),
    };
    let (kind, scope) = match head.split_once('(') {
        Some((k, rest)) => (k, Some(rest.strip_suffix(')')?.to_string())),
        None => (head, None),
    };
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_lowercase()) || description.trim().is_empty() {
        return None;
    }
    Some((kind.to_string(), scope, breaking, description.trim().to_string()))
}

/// The entries of one commit message, as release-please reads them: the header lines in the body come first,
/// then the title. A `BREAKING CHANGE:` footer makes the title breaking.
fn entries(sha: &str, message: &str) -> Vec<Entry> {
    let mut lines = message.lines();
    let title = lines.next().unwrap_or("").trim();
    let body: Vec<&str> = lines.collect();
    let change = body.iter().find_map(|l| l.trim().strip_prefix("Layr-Change: ")).map(|s| s.trim().to_string());
    let footer_breaking = body.iter().any(|l| l.starts_with("BREAKING CHANGE:") || l.starts_with("BREAKING-CHANGE:"));
    let mut out = Vec::new();
    for line in &body {
        if let Some((kind, scope, breaking, description)) = header(line.trim()) {
            if kind != "co" {
                out.push(Entry { kind, scope, breaking, description, pr: None, change: None, sha: sha.to_string() });
            }
        }
    }
    if let Some((kind, scope, breaking, description)) = header(title) {
        let (description, pr) = match description.rsplit_once(" (#") {
            Some((d, n)) if n.ends_with(')') && n[..n.len() - 1].chars().all(|c| c.is_ascii_digit()) => {
                (d.to_string(), n[..n.len() - 1].parse().ok())
            }
            _ => (description, None),
        };
        out.push(Entry { kind, scope, breaking: breaking || footer_breaking, description, pr, change, sha: sha.to_string() });
    }
    out
}

/// Read the historical release-please fixtures.
#[cfg(test)]
fn commits(log: &str) -> Vec<Entry> {
    log.split('\u{1e}')
        .filter_map(|c| c.trim_start_matches('\n').split_once('\u{1f}'))
        .filter(|(_, m)| !m.starts_with("chore(main): release "))
        .flat_map(|(sha, message)| entries(sha.trim(), message))
        .collect()
}

/// `x.y.z` after `version` for these entries (bump-minor-pre-major), or None when no entry makes a release.
fn next_version(version: &str, all: &[Entry], sections: &Sections) -> Result<Option<String>> {
    let parts: Vec<u64> = version.split('.').map(|p| p.parse()).collect::<Result<_, _>>().context("version is not x.y.z")?;
    let [major, minor, patch] = parts[..] else { bail!("version is not x.y.z") };
    let visible = |e: &Entry| sections.iter().any(|(t, _, hidden)| *t == e.kind && !hidden);
    let releasable: Vec<&Entry> = all.iter().filter(|e| e.breaking || visible(e)).collect();
    if releasable.is_empty() {
        return Ok(None);
    }
    let breaking = releasable.iter().any(|e| e.breaking);
    let feat = releasable.iter().any(|e| e.kind == "feat");
    Ok(Some(match (major, breaking, feat) {
        (0, true, _) | (0, _, true) => format!("0.{}.0", minor + 1),
        (_, true, _) => format!("{}.0.0", major + 1),
        (_, _, true) => format!("{major}.{}.0", minor + 1),
        _ => format!("{major}.{minor}.{}", patch + 1),
    }))
}

fn escape(s: &str) -> String {
    s.replace('<', "&lt;").replace('>', "&gt;")
}

/// One changelog line of an entry; `with_commit` adds the commit link (not in the breaking list).
fn line(e: &Entry, with_commit: bool) -> String {
    let mut s = String::from("* ");
    if let Some(scope) = &e.scope {
        s.push_str(&format!("**{scope}:** "));
    }
    s.push_str(&escape(&e.description));
    if let Some(n) = e.pr {
        s.push_str(&format!(" ([#{n}]({REPO}/issues/{n}))"));
    } else if let Some(id) = &e.change {
        s.push_str(&format!(" ([{id}]({CHANGES}/{id}))"));
    }
    if with_commit && !e.sha.is_empty() {
        s.push_str(&format!(" ([{}]({REPO}/commit/{}))", &e.sha[..7.min(e.sha.len())], e.sha));
    }
    s
}

/// The CHANGELOG section of `next`, after `previous`, on `date`. As release-please: one blank line after the
/// breaking list, two between the other sections.
fn section(previous: &str, next: &str, date: &str, all: &[Entry], sections: &Sections) -> String {
    fn by_scope(mut v: Vec<&Entry>) -> Vec<&Entry> {
        v.sort_by(|a, b| a.scope.as_deref().unwrap_or("").cmp(b.scope.as_deref().unwrap_or("")));
        v
    }
    let mut blocks: Vec<(String, bool, Vec<String>)> = Vec::new();
    let breaking = by_scope(all.iter().filter(|e| e.breaking).collect());
    if !breaking.is_empty() {
        blocks.push(("⚠ BREAKING CHANGES".into(), true, breaking.iter().map(|e| line(e, false)).collect()));
    }
    for (kind, title, hidden) in sections {
        let list = by_scope(all.iter().filter(|e| e.kind == *kind && (!hidden || e.breaking)).collect());
        if !list.is_empty() {
            blocks.push((title.clone(), false, list.iter().map(|e| line(e, true)).collect()));
        }
    }
    let mut out = format!("## [{next}]({REPO}/compare/v{previous}...v{next}) ({date})\n\n");
    let mut after_breaking = None;
    for (title, breaking, lines) in blocks {
        out.push_str(match after_breaking {
            None => "\n",
            Some(true) => "\n",
            Some(false) => "\n\n",
        });
        out.push_str(&format!("### {title}\n\n"));
        for l in lines {
            out.push_str(&l);
            out.push('\n');
        }
        after_breaking = Some(breaking);
    }
    out.push('\n');
    out
}

fn sections(root: &Path) -> Result<Sections> {
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("release-please-config.json"))?)?;
    Ok(cfg["changelog-sections"]
        .as_array()
        .context("changelog-sections")?
        .iter()
        .map(|s| {
            (
                s["type"].as_str().unwrap_or("").to_string(),
                s["section"].as_str().unwrap_or("").to_string(),
                s["hidden"].as_bool().unwrap_or(false),
            )
        })
        .collect())
}

/// Replaces the first `version = "<old>"` after `marker` in `text`.
fn set_version(text: &str, marker: &str, old: &str, new: &str) -> Result<String> {
    let at = text.find(marker).with_context(|| format!("{marker} not found"))?;
    let from = format!("version = \"{old}\"");
    let rel = text[at..].find(&from).with_context(|| format!("{from} not found after {marker}"))?;
    Ok(format!("{}version = \"{new}\"{}", &text[..at + rel], &text[at + rel + from.len()..]))
}

pub fn prepare(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("-n|--dry-run").value("--date").flag("--json"), args)?;
    let root = ctx.cwd.clone();
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join(".release-please-manifest.json")).context("no .release-please-manifest.json here")?,
    )?;
    let previous = manifest["."].as_str().context("manifest has no version")?.to_string();
    let here = super::here(ctx)?;
    let head = &here.line()?.head;
    let ref_name = format!("release/v{previous}");
    let base = crate::revs::resolve(&here.repo, here.line.as_ref(), &ref_name)
        .or_else(|_| crate::revs::resolve(&here.repo, here.line.as_ref(), &format!("v{previous}")))
        .with_context(|| format!("no release ref or tag for v{previous}"))?;
    if !here.repo.is_ancestor(&base, head)? {
        bail!("v{previous} is not in the current line history");
    }
    let mut all = Vec::new();
    for id in here.repo.first_parents_since(head, &base)? {
        let state = here.repo.state(&id)?;
        if state.message.starts_with("chore(main): release ") {
            continue;
        }
        let commit = here.repo.p.db.git_commit_of(&id)?.unwrap_or_default();
        all.extend(entries(&commit, &state.message));
    }
    let sections = sections(&root)?;
    let Some(next) = next_version(&previous, &all, &sections)? else {
        if a.has("--json") {
            outln!(ctx, "{}", serde_json::json!({"status":"none","previous":previous,"base":base,"head":head}));
        } else {
            outln!(ctx, "no release: no feat, fix, perf, revert or breaking change since v{previous}");
        }
        return Ok(0);
    };
    let date = a.get("--date").map(str::to_owned).unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let text = section(&previous, &next, &date, &all, &sections);
    if a.has("-n") {
        if a.has("--json") {
            outln!(
                ctx,
                "{}",
                serde_json::json!({"status":"ready","previous":previous,"version":next,
                "tag":format!("v{next}"),"base":base,"head":head,"changelog":text})
            );
        } else {
            ctx.print(&text);
        }
        return Ok(0);
    }
    let changelog = std::fs::read_to_string(root.join("CHANGELOG.md")).unwrap_or_else(|_| "# Changelog\n\n".into());
    let rest = changelog.strip_prefix("# Changelog\n\n").unwrap_or(&changelog);
    let toml = std::fs::read_to_string(root.join("Cargo.toml"))?;
    // The workspace version is the version of layr and layr-api (`version.workspace = true`).
    let toml = set_version(&toml, "[workspace.package]", &previous, &next)?;
    let lock = std::fs::read_to_string(root.join("Cargo.lock"))?;
    let lock = set_version(&lock, "name = \"layr\"\n", &previous, &next)?;
    let lock = set_version(&lock, "name = \"layr-api\"\n", &previous, &next)?;
    std::fs::write(root.join("CHANGELOG.md"), format!("# Changelog\n\n{text}{rest}"))?;
    std::fs::write(root.join("Cargo.toml"), toml)?;
    std::fs::write(root.join("Cargo.lock"), lock)?;
    std::fs::write(root.join(".release-please-manifest.json"), format!("{{\n  \".\": \"{next}\"\n}}\n"))?;
    if a.has("--json") {
        outln!(
            ctx,
            "{}",
            serde_json::json!({"status":"written","previous":previous,"version":next,
            "tag":format!("v{next}"),"base":base,"head":head})
        );
    } else {
        outln!(ctx, "{next}: CHANGELOG.md, Cargo.toml, Cargo.lock and the manifest written; save, queue, then tag v{next}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Sections {
        sections(&std::env::current_dir().unwrap()).unwrap()
    }

    /// The section of `version` in the CHANGELOG.md of the repository.
    fn recorded(version: &str) -> String {
        let text = std::fs::read_to_string(std::env::current_dir().unwrap().join("CHANGELOG.md")).unwrap();
        let start = text.find(&format!("## [{version}]")).unwrap();
        let end = text[start + 3..].find("\n## [").map(|i| start + 3 + i + 1).unwrap();
        text[start..end].to_string()
    }

    /// Release-please made these sections from the same commits: the output is the same, byte for byte.
    #[test]
    fn the_sections_of_0_36_and_0_37_come_out_as_release_please_wrote_them() {
        let dir = std::env::current_dir().unwrap().join("tests/fixtures/release");
        for (previous, version) in [("0.35.0", "0.36.0"), ("0.36.0", "0.37.0")] {
            let all = commits(&std::fs::read_to_string(dir.join(format!("v{version}.log"))).unwrap());
            assert_eq!(next_version(previous, &all, &config()).unwrap().as_deref(), Some(version));
            assert_eq!(section(previous, version, "2026-11-06", &all, &config()), recorded(version), "section of {version}");
        }
    }

    #[test]
    fn versions_follow_bump_minor_pre_major() {
        let e = |kind: &str, breaking: bool| Entry {
            kind: kind.into(),
            scope: None,
            breaking,
            description: "x".into(),
            pr: None,
            change: None,
            sha: "0".repeat(40),
        };
        let c = config();
        assert_eq!(next_version("0.37.0", &[e("fix", false)], &c).unwrap().as_deref(), Some("0.37.1"));
        assert_eq!(next_version("0.37.1", &[e("feat", false)], &c).unwrap().as_deref(), Some("0.38.0"));
        assert_eq!(next_version("0.37.1", &[e("refactor", true)], &c).unwrap().as_deref(), Some("0.38.0"));
        assert_eq!(next_version("1.2.3", &[e("fix", true)], &c).unwrap().as_deref(), Some("2.0.0"));
        assert_eq!(next_version("0.37.0", &[e("docs", false), e("test", false)], &c).unwrap(), None);
    }

    #[test]
    fn a_layr_change_links_to_layr_cloud() {
        let all = entries("abcdef0123456789", "fix(queue): squash the entry\n\nLayr-Change: 01a1b2c3\n");
        assert_eq!(line(&all[0], false), format!("* **queue:** squash the entry ([01a1b2c3]({CHANGES}/01a1b2c3))"));
    }
}
