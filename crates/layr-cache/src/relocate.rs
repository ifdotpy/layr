//! The cargo units that name the place where they were built. A cache folder that a new line takes, or that a
//! merge brings, was built at another path. Cargo sees most of such a move itself: its fingerprints name the
//! sources relative to the package and to the target folder. It does not see a path that rustc compiled in: the
//! value of `env!("CARGO_BIN_EXE_<bin>")` or `env!("CARGO_MANIFEST_DIR")`, or a source out of the target folder
//! that the dep-info names by its absolute path. Such a unit stays fresh at the new place and names the old one:
//! a test runs the binary of the other line, or fails when that line is gone.
//! `relocate` removes the fingerprints of these units, so the next build makes them again. The other units stay.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Makes stale the units of the cargo `target` folder that name the place where they were built, when the folder
/// is used at `at` from now on. Returns the number of units made stale.
pub fn relocate(target: &Path, at: &Path) -> Result<usize> {
    let mut stale = 0;
    for profile in profiles(target)? {
        let rel = profile.strip_prefix(target).context("a profile out of the target folder")?;
        let at_profile = at.join(rel);
        let deps = profile.join("deps");
        for entry in fs::read_dir(&deps).with_context(|| format!("read {}", deps.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".d")) else { continue };
            if !entry.file_type()?.is_file() {
                continue;
            }
            // A dep-info file that cannot be read names nothing that cargo uses either.
            let Ok(bytes) = fs::read(entry.path()) else { continue };
            if names_old_place(&String::from_utf8_lossy(&bytes), stem, rel, &at_profile) {
                stale += remove_fingerprint(&profile, stem)?;
            }
        }
    }
    Ok(stale)
}

/// The profile folders of `target`: the folders with a `deps` and a `.fingerprint` folder (`debug`, `release`, a
/// custom profile), also below a target triple.
fn profiles(target: &Path) -> Result<Vec<PathBuf>> {
    let is_profile = |p: &Path| p.join("deps").is_dir() && p.join(".fingerprint").is_dir();
    let mut out = Vec::new();
    for entry in fs::read_dir(target).with_context(|| format!("read {}", target.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let p = entry.path();
        if is_profile(&p) {
            out.push(p);
            continue;
        }
        for sub in fs::read_dir(&p).into_iter().flatten().flatten() {
            if sub.file_type().is_ok_and(|t| t.is_dir()) && is_profile(&sub.path()) {
                out.push(sub.path());
            }
        }
    }
    Ok(out)
}

/// Whether the dep-info `text` of the unit `stem` names the place where it was built, when its profile folder
/// (`rel` below the target folder) is used at `at_profile`.
fn names_old_place(text: &str, stem: &str, rel: &Path, at_profile: &Path) -> bool {
    // The first rule names the dep-info file itself, at the place of the build: `<profile>/deps/<stem>.d: ...`.
    let first = text.lines().next().unwrap_or("");
    let Some(built) = first.find(&format!("/deps/{stem}.d:")).map(|i| Path::new(&first[..i])) else { return false };
    if built == at_profile {
        return false;
    }
    let Some(old_target) = built.ancestors().nth(rel.components().count()) else { return false };
    // The old place without the end that both places share: `<old line>` of `<old line>/target/debug`.
    let old_root = without_common_end(built, at_profile);
    if old_root.components().all(|c| matches!(c, Component::RootDir)) {
        return false;
    }
    let old = |p: &str| Path::new(p).starts_with(&old_root);
    for line in text.lines() {
        if let Some(env) = line.strip_prefix("# env-dep:") {
            let Some((name, value)) = env.split_once('=') else { continue };
            // A binary path is in the target folder; other build outputs there (OUT_DIR) are copied with it.
            if old(value) && (name.starts_with("CARGO_BIN_EXE_") || !Path::new(value).starts_with(old_target)) {
                return true;
            }
        } else if !line.starts_with('#') {
            // `targets: sources`, or `source:`. The targets are in the target folder.
            let sources = match line.split_once(": ") {
                Some((_, s)) => s,
                None => line.strip_suffix(':').unwrap_or(""),
            };
            if paths(sources).any(|p| old(&p) && !Path::new(&p).starts_with(old_target)) {
                return true;
            }
        }
    }
    false
}

/// `a` without the trailing components that it shares with `b`.
fn without_common_end(a: &Path, b: &Path) -> PathBuf {
    let (ac, bc): (Vec<_>, Vec<_>) = (a.components().collect(), b.components().collect());
    let same = ac.iter().rev().zip(bc.iter().rev()).take_while(|(x, y)| x == y).count();
    ac[..ac.len() - same].iter().collect()
}

/// The paths of a make rule's dependency list: separated by spaces, with `\ ` for a space in a path.
fn paths(list: &str) -> impl Iterator<Item = String> + '_ {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = list.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                cur.push(' ');
                chars.next();
            }
            ' ' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter()
}

/// Removes the fingerprint folder of the unit `stem` (`<name>-<hash>`): `.fingerprint/<package>-<hash>`. Returns 1
/// when it removed one.
fn remove_fingerprint(profile: &Path, stem: &str) -> Result<usize> {
    let Some((_, hash)) = stem.rsplit_once('-') else { return Ok(0) };
    let suffix = format!("-{hash}");
    let mut removed = 0;
    for entry in fs::read_dir(profile.join(".fingerprint"))? {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(|n| n.ends_with(&suffix)) && entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path()).with_context(|| format!("remove {}", entry.path().display()))?;
            removed = 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("layr-relocate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// A unit of `target` built at `built` (the old target folder): its dep-info `text` and its fingerprint.
    fn unit(target: &Path, stem: &str, package: &str, text: &str) {
        let profile = target.join("debug");
        fs::create_dir_all(profile.join("deps")).unwrap();
        let hash = stem.rsplit_once('-').unwrap().1;
        fs::create_dir_all(profile.join(".fingerprint").join(format!("{package}-{hash}"))).unwrap();
        fs::write(profile.join("deps").join(format!("{stem}.d")), text).unwrap();
    }

    fn fresh(target: &Path, package: &str, hash: &str) -> bool {
        target.join("debug/.fingerprint").join(format!("{package}-{hash}")).is_dir()
    }

    #[test]
    fn only_units_with_a_compiled_in_old_path_go_stale() {
        let root = scratch("units");
        let target = root.join("new/target");
        let old = "/lines/old/target/debug/deps";
        unit(
            &target,
            "it-aaa",
            "my-x",
            &format!("{old}/it-aaa.d: tests/it.rs\n\n{old}/it-aaa: tests/it.rs\n\ntests/it.rs:\n\n# env-dep:CARGO_BIN_EXE_x=/lines/old/target/debug/x\n"),
        );
        unit(&target, "dir-bbb", "my-y", &format!("{old}/dir-bbb.d: src/lib.rs\n\nsrc/lib.rs:\n\n# env-dep:CARGO_MANIFEST_DIR=/lines/old/y\n"));
        unit(&target, "shared-ccc", "shared", &format!("{old}/shared-ccc.d: /lines/old/shared/src/lib.rs\n\n/lines/old/shared/src/lib.rs:\n"));
        // OUT_DIR content was copied with the folder; a registry crate names no line path.
        unit(
            &target,
            "gen-ddd",
            "gen",
            &format!("{old}/gen-ddd.d: src/lib.rs /lines/old/target/debug/build/gen-1/out/x.rs\n\n# env-dep:OUT_DIR=/lines/old/target/debug/build/gen-1/out\n"),
        );
        unit(&target, "serde-eee", "serde", &format!("{old}/serde-eee.d: /home/u/.cargo/registry/src/serde/lib.rs\n"));
        assert_eq!(relocate(&target, Path::new("/lines/new/target")).unwrap(), 3);
        assert!(!fresh(&target, "my-x", "aaa") && !fresh(&target, "my-y", "bbb") && !fresh(&target, "shared", "ccc"));
        assert!(fresh(&target, "gen", "ddd") && fresh(&target, "serde", "eee"));
        // At the place where it was built nothing is stale.
        unit(&target, "it-fff", "my-x", &format!("{old}/it-fff.d: tests/it.rs\n\n# env-dep:CARGO_BIN_EXE_x=/lines/old/target/debug/x\n"));
        assert_eq!(relocate(&target, Path::new("/lines/old/target")).unwrap(), 0);
        assert!(fresh(&target, "my-x", "fff"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_path_with_a_space_is_one_path() {
        assert_eq!(paths(r"a/b\ c.rs d.rs").collect::<Vec<_>>(), vec!["a/b c.rs", "d.rs"]);
    }
}
