//! The cargo units that name the place where they were built. A cache folder that a new line takes, or that a
//! merge brings, was built at another path. Cargo sees most of such a move itself: its fingerprints name the
//! sources relative to the package and to the target folder. It does not see a path that rustc compiled in: the
//! value of `env!("CARGO_BIN_EXE_<bin>")` or `env!("CARGO_MANIFEST_DIR")`, or a source out of the target folder
//! that the dep-info names by its absolute path. Such a unit stays fresh at the new place and names the old one:
//! a test runs the binary of the other line, or fails when that line is gone.
//! `relocate` removes the fingerprints of these units, so the next build makes them again. The other units stay.
//! A unit whose only old paths are sources out of its package (a workspace `Cargo.toml`, a `clippy.toml`) that the
//! new place has with the same bytes is moved instead: its dep-info names the new place, and it stays fresh.

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
            let text = String::from_utf8_lossy(&bytes);
            let encoded: Vec<Vec<u8>> = unit_dep_files(&profile, stem)?.iter().filter_map(|f| fs::read(f).ok()).collect();
            match place(&text, &encoded, stem, rel, &at_profile) {
                Place::Here => {}
                Place::Stale => stale += remove_fingerprint(&profile, stem)?,
                Place::Moved { old, new } => move_unit(&profile, stem, &entry.path(), &text, &old, &new)?,
            }
        }
    }
    Ok(stale)
}

/// The registry source folder (`<CARGO_HOME>/registry/src`) that the units of `target` were built from, when this
/// machine does not have it. Cargo keeps the absolute path of a registry source in the unit's fingerprint, so with
/// another CARGO_HOME every registry crate builds again. Reads the dep-info files until one names a registry source.
pub fn foreign_registry(target: &Path) -> Option<PathBuf> {
    const MARK: &str = "/registry/src/";
    for profile in profiles(target).ok()? {
        for entry in fs::read_dir(profile.join("deps")).ok()?.flatten() {
            if !entry.file_name().to_str().is_some_and(|n| n.ends_with(".d")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(entry.path()) else { continue };
            let Some(line) = text.lines().next() else { continue };
            let Some(path) = line.split_once(": ").and_then(|(_, list)| paths(list).find(|p| p.contains(MARK))) else {
                continue;
            };
            let root = PathBuf::from(&path[..path.find(MARK)? + MARK.len() - 1]);
            return (!root.is_dir()).then_some(root);
        }
    }
    None
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

/// What a unit's dep-info says about the place where it was built.
#[derive(Debug, PartialEq)]
enum Place {
    /// It names no old place (or it was built here).
    Here,
    /// It names the old place in a way that only a new build fixes.
    Stale,
    /// It names only sources of the old place (`old`, a line root) that the new place (`new`) has with the same bytes.
    Moved { old: PathBuf, new: PathBuf },
}

/// What the dep-info of the unit `stem` (its `text` file and its `encoded` binary files) says about the place where it
/// was built, when its profile folder (`rel` below the target folder) is used at `at_profile`. A source out of the
/// package (`include_str!("../../other/x.json")`) is relative in the text file and absolute in the binary one, which is
/// the one cargo checks. A compiled-in path (`env!`) of the old place makes the
/// unit stale. A source of the old place out of the target folder moves with the unit when the new place has a file
/// with the same bytes at the same path below its root: cargo then checks the new file, as for a relative source.
fn place(text: &str, encoded: &[Vec<u8>], stem: &str, rel: &Path, at_profile: &Path) -> Place {
    // The first rule names the dep-info file itself, at the place of the build: `<profile>/deps/<stem>.d: ...`.
    let first = text.lines().next().unwrap_or("");
    let Some(built) = first.find(&format!("/deps/{stem}.d:")).map(|i| Path::new(&first[..i])) else { return Place::Here };
    if built == at_profile {
        return Place::Here;
    }
    let Some(old_target) = built.ancestors().nth(rel.components().count()) else { return Place::Here };
    // The old place without the end that both places share: `<old line>` of `<old line>/target/debug`.
    let old_root = without_common_end(built, at_profile);
    if old_root.components().all(|c| matches!(c, Component::RootDir)) {
        return Place::Here;
    }
    let new_root = without_common_end(at_profile, built);
    let old = |p: &str| Path::new(p).starts_with(&old_root);
    let mut sources = std::collections::BTreeSet::new();
    for line in text.lines() {
        if let Some(env) = line.strip_prefix("# env-dep:") {
            let Some((name, value)) = env.split_once('=') else { continue };
            // A binary path is in the target folder; other build outputs there (OUT_DIR) are copied with it.
            if old(value) && (name.starts_with("CARGO_BIN_EXE_") || !Path::new(value).starts_with(old_target)) {
                return Place::Stale;
            }
        } else if !line.starts_with('#') {
            // `targets: sources`, or `source:`. The targets are in the target folder.
            let list = match line.split_once(": ") {
                Some((_, s)) => s,
                None => line.strip_suffix(':').unwrap_or(""),
            };
            sources.extend(paths(list).filter(|p| old(p) && !Path::new(p).starts_with(old_target)));
        }
    }
    for bytes in encoded {
        let found = encoded_paths(bytes, old_root.as_os_str().as_encoded_bytes());
        sources.extend(found.into_iter().filter(|p| !Path::new(p).starts_with(old_target)));
    }
    if sources.is_empty() {
        return Place::Here;
    }
    let same = |p: &String| {
        let there = new_root.join(Path::new(p).strip_prefix(&old_root).unwrap_or(Path::new(p)));
        matches!((fs::read(p), fs::read(&there)), (Ok(a), Ok(b)) if a == b)
    };
    if sources.iter().all(same) {
        Place::Moved { old: old_root, new: new_root }
    } else {
        Place::Stale
    }
}

/// Moves the unit `stem` from the line root `old` to `new`: its text dep-info (`deps/<stem>.d`, at `d`) and the binary
/// dep-info files of its fingerprint (`.fingerprint/<package>-<hash>/dep-*`) name the new root. Their times stay, so
/// cargo's compare of the source times with them gives what it gave before.
fn move_unit(profile: &Path, stem: &str, d: &Path, text: &str, old: &Path, new: &Path) -> Result<()> {
    let escape = |p: &Path| format!("{}/", p.display()).replace(' ', "\\ ");
    rewrite(d, text.replace(&escape(old), &escape(new)).into_bytes())?;
    let (old, new) = (old.as_os_str().as_encoded_bytes(), new.as_os_str().as_encoded_bytes());
    for file in unit_dep_files(profile, stem)? {
        let bytes = fs::read(&file)?;
        let moved = rewrite_encoded(&bytes, old, new);
        if moved != bytes {
            rewrite(&file, moved)?;
        }
    }
    Ok(())
}

/// The binary dep-info files of the unit `stem` (`<name>-<hash>`): `.fingerprint/<package>-<hash>/dep-*`.
fn unit_dep_files(profile: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let Some((_, hash)) = stem.rsplit_once('-') else { return Ok(Vec::new()) };
    let suffix = format!("-{hash}");
    let mut out = Vec::new();
    for unit in fs::read_dir(profile.join(".fingerprint"))?.flatten() {
        if !unit.file_name().to_str().is_some_and(|n| n.ends_with(&suffix)) || !unit.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(unit.path())?.flatten() {
            if file.file_name().to_str().is_some_and(|n| n.starts_with("dep-")) {
                out.push(file.path());
            }
        }
    }
    Ok(out)
}

/// The paths below `old` in a binary dep-info of cargo (see `rewrite_encoded`).
fn encoded_paths(bytes: &[u8], old: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = bytes.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize) {
            let path = bytes.get(i + 4..i + 4 + len).unwrap_or_default();
            if path.len() > old.len() && path.starts_with(old) && path[old.len()] == b'/' {
                out.push(String::from_utf8_lossy(path).into_owned());
                i += 4 + len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Writes `bytes` to `path` and gives the file its old modification time back.
fn rewrite(path: &Path, bytes: Vec<u8>) -> Result<()> {
    let time = fs::metadata(path)?.modified()?;
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    fs::File::options().write(true).open(path)?.set_modified(time)?;
    Ok(())
}

/// A binary dep-info of cargo with each path below `old` moved below `new`. A path there is its length (u32, little
/// end) and its bytes; an environment value is stored the same way, but a unit with an old one is stale (`place`).
fn rewrite_encoded(bytes: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = bytes.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize) {
            let start = i + 4;
            let path = bytes.get(start..start + len).unwrap_or_default();
            if path.len() > old.len() && path.starts_with(old) && path[old.len()] == b'/' {
                let rest = &path[old.len()..];
                out.extend(((new.len() + rest.len()) as u32).to_le_bytes());
                out.extend(new);
                out.extend(rest);
                i = start + len;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
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
    fn a_registry_that_this_machine_does_not_have_is_named() {
        let root = scratch("registry");
        let target = root.join("target");
        let old = "/lines/old/target/debug/deps";
        unit(&target, "own-aaa", "own", &format!("{old}/own-aaa.d: src/lib.rs\n"));
        assert_eq!(foreign_registry(&target), None, "no registry crate: nothing to say");
        unit(&target, "serde-eee", "serde", &format!("{old}/serde-eee.d: /nowhere/u/.cargo/registry/src/idx/serde/lib.rs\n"));
        assert_eq!(foreign_registry(&target), Some(PathBuf::from("/nowhere/u/.cargo/registry/src")));
        let here = root.join("home/registry/src");
        fs::create_dir_all(&here).unwrap();
        fs::write(target.join("debug/deps/serde-eee.d"), format!("{old}/serde-eee.d: {}/idx/serde/lib.rs\n", here.display())).unwrap();
        assert_eq!(foreign_registry(&target), None, "this machine has that registry");
        fs::remove_dir_all(&root).unwrap();
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
    fn a_unit_whose_old_sources_the_new_line_has_moves_and_stays_fresh() {
        let root = scratch("moved");
        let (old_line, new_line) = (root.join("old"), root.join("new"));
        let target = new_line.join("target");
        for line in [&old_line, &new_line] {
            fs::create_dir_all(line.join("tools")).unwrap();
            fs::write(line.join("Cargo.toml"), "[workspace]\n").unwrap();
            fs::write(line.join("tools/clippy.toml"), "msrv = \"1.80\"\n").unwrap();
        }
        fs::write(new_line.join("tools/other.toml"), "a\n").unwrap();
        let (o, d) = (old_line.display(), format!("{}/target/debug/deps", old_line.display()));
        let text = |stem: &str, extra: &str| {
            format!("{d}/{stem}.d: src/lib.rs {o}/Cargo.toml {o}/tools/{extra}\n\nsrc/lib.rs:\n{o}/Cargo.toml:\n")
        };
        // The binary dep-info: a header, then per file its kind, length, path and a checksum flag.
        let encoded = |paths: &[String]| {
            let mut b = vec![1, 0, 0, 0, 0xff, 1];
            b.extend((paths.len() as u32).to_le_bytes());
            for p in paths {
                b.push(1);
                b.extend((p.len() as u32).to_le_bytes());
                b.extend(p.as_bytes());
                b.push(0);
            }
            b.extend(0u32.to_le_bytes());
            b
        };
        // A source out of the package that only the binary dep-info names by its absolute path, with `..` parts.
        fs::create_dir_all(old_line.join("crates/a/src")).unwrap();
        fs::create_dir_all(new_line.join("crates/a/src")).unwrap();
        fs::create_dir_all(old_line.join("crates/http")).unwrap();
        fs::create_dir_all(new_line.join("crates/http")).unwrap();
        fs::write(old_line.join("crates/http/openapi.json"), "{}").unwrap();
        fs::write(new_line.join("crates/http/openapi.json"), "{}").unwrap();
        unit(
            &target,
            "store-ddd",
            "store",
            &format!("{d}/store-ddd.d: crates/a/src/lib.rs crates/a/src/../../http/openapi.json\n"),
        );
        let store_dep = target.join("debug/.fingerprint/store-ddd/dep-test-lib-store");
        let include = |root: &dyn std::fmt::Display| format!("{root}/crates/a/src/../../http/openapi.json");
        fs::write(&store_dep, encoded(&[include(&o), "src/lib.rs".into()])).unwrap();
        unit(&target, "json-aaa", "json", &text("json-aaa", "clippy.toml"));
        let dep = target.join("debug/.fingerprint/json-aaa/dep-lib-json");
        fs::write(&dep, encoded(&[format!("{o}/tools/clippy.toml"), format!("{o}/Cargo.toml"), "src/lib.rs".into()])).unwrap();
        // A source that the new line has with other bytes, or does not have: the unit goes stale.
        fs::write(old_line.join("tools/other.toml"), "b\n").unwrap();
        unit(&target, "lint-bbb", "lint", &text("lint-bbb", "other.toml"));
        unit(&target, "gone-ccc", "gone", &text("gone-ccc", "missing.toml"));
        let time = fs::metadata(&dep).unwrap().modified().unwrap();
        assert_eq!(relocate(&target, &target).unwrap(), 2);
        assert!(fresh(&target, "json", "aaa") && !fresh(&target, "lint", "bbb") && !fresh(&target, "gone", "ccc"));
        let n = new_line.display();
        assert_eq!(
            fs::read(&dep).unwrap(),
            encoded(&[format!("{n}/tools/clippy.toml"), format!("{n}/Cargo.toml"), "src/lib.rs".into()])
        );
        assert_eq!(fs::metadata(&dep).unwrap().modified().unwrap(), time, "the time stays");
        assert!(fresh(&target, "store", "ddd"), "a unit whose old path is only in the binary dep-info moves");
        assert_eq!(fs::read(&store_dep).unwrap(), encoded(&[include(&n), "src/lib.rs".into()]));
        let moved = fs::read_to_string(target.join("debug/deps/json-aaa.d")).unwrap();
        assert!(moved.contains(&format!("{n}/Cargo.toml")) && !moved.contains(&o.to_string()), "{moved}");
        // At the new place the unit names no old place.
        assert_eq!(relocate(&target, &target).unwrap(), 0);
        assert!(fresh(&target, "json", "aaa"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_path_with_a_space_is_one_path() {
        assert_eq!(paths(r"a/b\ c.rs d.rs").collect::<Vec<_>>(), vec!["a/b c.rs", "d.rs"]);
    }
}
