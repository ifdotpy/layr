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
use crate::cargo::layout::Layout;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Makes stale the units of the cargo `target` folder that name the place where they were built, when the folder
/// is used at `at` from now on. Returns the number of units rewritten or made stale.
pub fn relocate(target: &Path, at: &Path) -> Result<usize> {
    let mut changed = 0;
    for profile in profiles(target)? {
        let rel = profile.strip_prefix(target).context("a profile out of the target folder")?;
        let at_profile = at.join(rel);
        let units = Layout::units(&profile);
        let mut dirs = std::collections::BTreeMap::new();
        let mut fingerprints: std::collections::BTreeMap<&str, Vec<PathBuf>> = std::collections::BTreeMap::new();
        for unit in &units {
            fingerprints.entry(&unit.hash).or_default().push(unit.fingerprint.clone());
            // Legacy outputs share one directory; new-layout output names can be unhashed.
            dirs.entry(unit.outputs.clone()).or_insert_with(|| match unit.layout {
                Layout::Legacy => None,
                Layout::UnitDirectories => Some(unit.hash.clone()),
            });
        }
        for (dir, owner) in dirs {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let name = entry.file_name();
                let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".d")) else { continue };
                if !entry.file_type()?.is_file() { continue }
                let hash = owner.as_deref().or_else(|| stem.rsplit_once('-').map(|(_, hash)| hash));
                let Some(paths) = hash.and_then(|hash| fingerprints.get(hash)) else { continue };
                let Ok(bytes) = fs::read(entry.path()) else { continue };
                let text = String::from_utf8_lossy(&bytes);
                let encoded: Vec<Vec<u8>> = unit_dep_files(paths)?.iter().filter_map(|f| fs::read(f).ok()).collect();
                let artifact = entry.path();
                let artifact = artifact.strip_prefix(&profile)?;
                match place(&text, &encoded, artifact, rel, &at_profile) {
                    Place::Here => {}
                    Place::Stale => changed += remove_fingerprints(paths)?,
                    Place::Moved { old, new } => changed += usize::from(move_unit(paths, &entry.path(), &text, &old, &new)?),
                }
            }
        }
    }
    Ok(changed)
}

/// The registry source folder (`<CARGO_HOME>/registry/src`) that the units of `target` were built from, when this
/// machine does not have it. Cargo keeps the absolute path of a registry source in the unit's fingerprint, so with
/// another CARGO_HOME every registry crate builds again. Reads the dep-info files until one names a registry source.
pub fn foreign_registry(target: &Path) -> Option<PathBuf> {
    const MARK: &str = "/registry/src/";
    for profile in profiles(target).ok()? {
        for entry in Layout::output_dirs(&profile).into_iter().flat_map(|dir| fs::read_dir(dir).into_iter().flatten().flatten()) {
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
    let is_profile = |p: &Path| Layout::has_units(p);
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
fn place(text: &str, encoded: &[Vec<u8>], artifact: &Path, rel: &Path, at_profile: &Path) -> Place {
    // The first rule names the dep-info file itself, at the place of the build: `<profile>/deps/<stem>.d: ...`.
    let first = text.lines().next().unwrap_or("");
    let Some((rule, _)) = first.split_once(": ") else { return Place::Here };
    let path = Path::new(rule);
    if !path.ends_with(artifact) { return Place::Here }
    let Some(built) = path.ancestors().nth(artifact.components().count()) else { return Place::Here };
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

/// Moves the unit from the line root `old` to `new`: its text dep-info at `d` and the binary dep-info files
/// in its fingerprints name the new root. Their times stay, so
/// cargo's compare of the source times with them gives what it gave before.
fn move_unit(fingerprints: &[PathBuf], d: &Path, text: &str, old: &Path, new: &Path) -> Result<bool> {
    let escape = |p: &Path| format!("{}/", p.display()).replace(' ', "\\ ");
    let moved = text.replace(&escape(old), &escape(new));
    let mut changed = moved != text;
    if changed {
        rewrite(d, moved.into_bytes())?;
    }
    let (old, new) = (old.as_os_str().as_encoded_bytes(), new.as_os_str().as_encoded_bytes());
    for file in unit_dep_files(fingerprints)? {
        let bytes = fs::read(&file)?;
        let moved = rewrite_encoded(&bytes, old, new);
        if moved != bytes {
            rewrite(&file, moved)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Files that cargo or clippy name by an absolute path in every unit of a workspace (the workspace manifest, the
/// clippy config). A project cannot change that, and `relocate` moves such a unit when the file has the same bytes.
const TOOL_FILES: [&str; 4] = ["Cargo.toml", "Cargo.lock", "clippy.toml", ".clippy.toml"];

/// The sources below `root` that a unit of the cargo `target` folder names by an absolute path, as (unit, path
/// relative to `root`). Cargo names a source of the unit's own package relative to the package, and a source in the
/// target folder relative to that folder; any other source by its absolute path. So a unit that includes a file of
/// another folder of the workspace (`include_str!("../../other/x.json")`) names this place: a copy of the workspace
/// at another path builds it again, and a new line keeps it warm only when the file there has the same bytes.
/// Paths in `target` and the files of `TOOL_FILES` are left out. Each (unit, path) once.
pub fn outside_sources(target: &Path, root: &Path) -> Vec<(String, String)> {
    // Cargo writes the path that it was given: the root as named, or its canonical form (`/tmp` is `/private/tmp`).
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out: Vec<(String, String)> = Vec::new();
    for root in if canonical == root { vec![root.to_path_buf()] } else { vec![root.to_path_buf(), canonical] } {
        for item in sources_below(target, &root) {
            if !out.contains(&item) {
                out.push(item);
            }
        }
    }
    out.sort();
    out
}

/// `outside_sources` for one spelling of the root.
fn sources_below(target: &Path, root: &Path) -> Vec<(String, String)> {
    let root_bytes = root.as_os_str().as_encoded_bytes();
    let mut out: Vec<(String, String)> = Vec::new();
    for profile in profiles(target).unwrap_or_default() {
        for unit in Layout::units(&profile) {
            let name = format!("{}-{}", unit.name, unit.hash);
            for file in fs::read_dir(unit.fingerprint).into_iter().flatten().flatten() {
                if !file.file_name().to_str().is_some_and(|n| n.starts_with("dep-")) {
                    continue;
                }
                let Ok(bytes) = fs::read(file.path()) else { continue };
                for path in encoded_paths(&bytes, root_bytes) {
                    // rustc names an include as written: `<package>/src/../../other/x.json`.
                    let rel = lexical(Path::new(&path)).strip_prefix(root).map(Path::to_path_buf).unwrap_or_default();
                    let tool = rel.file_name().and_then(|n| n.to_str()).is_some_and(|n| TOOL_FILES.contains(&n));
                    if rel.as_os_str().is_empty() || rel.starts_with("target") || tool {
                        continue;
                    }
                    let item = (name.clone(), rel.display().to_string());
                    if !out.contains(&item) {
                        out.push(item);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// `path` without `.` parts, and with each `..` part taken back with the part before it. Reads no file.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The binary dep-info files in the unit's fingerprints.
fn unit_dep_files(fingerprints: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for fingerprint in fingerprints {
        let files = match fs::read_dir(fingerprint) {
            Ok(files) => files,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for file in files {
            let file = file?;
            if file.file_name().to_str().is_some_and(|n| n.starts_with("dep-")) && file.file_type()?.is_file() {
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

/// Removes the unit's fingerprint folders and returns the number removed.
fn remove_fingerprints(fingerprints: &[PathBuf]) -> Result<usize> {
    let mut removed = 0;
    for fingerprint in fingerprints {
        if fingerprint.is_dir() {
            fs::remove_dir_all(fingerprint).with_context(|| format!("remove {}", fingerprint.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod lexical_tests {
    #[test]
    fn parent_parts_take_back_the_part_before() {
        assert_eq!(
            super::lexical(std::path::Path::new("/w/crates/a/src/../../b/./x.json")),
            std::path::PathBuf::from("/w/crates/b/x.json")
        );
    }
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
    fn new_layout_stales_a_unit_with_an_old_compiled_path() {
        let root = scratch("new-layout");
        let target = root.join("new/target");
        let unit = target.join("debug/build/foo/0123456789abcdef");
        fs::create_dir_all(unit.join("out")).unwrap();
        fs::create_dir_all(unit.join("fingerprint")).unwrap();
        fs::write(unit.join("fingerprint/dep-lib-foo"), b"dep").unwrap();
        fs::write(unit.join("out/foo-0123456789abcdef.d"),
            "/old/target/debug/build/foo/0123456789abcdef/out/foo-0123456789abcdef.d: src/lib.rs\n# env-dep:CARGO_MANIFEST_DIR=/old\n").unwrap();
        assert_eq!(relocate(&target, &target).unwrap(), 1);
        assert!(!unit.join("fingerprint").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn profile_and_package_names_can_match_layout_components() {
        let root = scratch("profile-components");
        let target = root.join("new/target");
        for (profile, package, hash) in [("debug", "debug", "0123456789abcdef"), ("out", "foo", "fedcba9876543210"), ("build", "foo", "1111111111111111")] {
            let unit = target.join(profile).join("build").join(package).join(hash);
            fs::create_dir_all(unit.join("out")).unwrap();
            fs::create_dir_all(unit.join("fingerprint")).unwrap();
            fs::write(unit.join("fingerprint/dep-bin"), b"dep").unwrap();
            fs::write(unit.join("out/foo.d"), format!("/old/target/{profile}/build/{package}/{hash}/out/foo.d: src/main.rs\n# env-dep:CARGO_MANIFEST_DIR=/old\n")).unwrap();
        }
        assert_eq!(relocate(&target, &target).unwrap(), 3);
        for (profile, package, hash) in [("debug", "debug", "0123456789abcdef"), ("out", "foo", "fedcba9876543210"), ("build", "foo", "1111111111111111")] {
            assert!(!target.join(profile).join("build").join(package).join(hash).join("fingerprint").exists());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unhashed_binary_and_build_script_are_made_stale() {
        let root = scratch("unhashed");
        let target = root.join("new/target");
        for (stem, hash) in [("foo", "0123456789abcdef"), ("build_script_build", "fedcba9876543210")] {
            let unit = target.join("debug/build/foo").join(hash);
            fs::create_dir_all(unit.join("out")).unwrap();
            fs::create_dir_all(unit.join("fingerprint")).unwrap();
            fs::write(unit.join("fingerprint/dep-bin"), b"dep").unwrap();
            fs::write(unit.join("out").join(format!("{stem}.d")), format!("/old/target/debug/build/foo/{hash}/out/{stem}.d: src/main.rs\n# env-dep:CARGO_MANIFEST_DIR=/old\n")).unwrap();
        }
        assert_eq!(relocate(&target, &target).unwrap(), 2);
        for hash in ["0123456789abcdef", "fedcba9876543210"] {
            assert!(!target.join("debug/build/foo").join(hash).join("fingerprint").exists());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn symlinked_package_cannot_relocate_external_units() {
        use std::os::unix::fs::symlink;
        let root = scratch("external-symlink");
        let target = root.join("target");
        let external = root.join("external");
        let unit = external.join("0123456789abcdef");
        fs::create_dir_all(unit.join("out")).unwrap();
        fs::create_dir_all(unit.join("fingerprint")).unwrap();
        fs::write(unit.join("fingerprint/dep-bin"), b"dep").unwrap();
        fs::write(unit.join("out/foo.d"), "/old/target/debug/build/foo/0123456789abcdef/out/foo.d: src/main.rs\n# env-dep:CARGO_MANIFEST_DIR=/old\n").unwrap();
        fs::create_dir_all(target.join("debug/build")).unwrap();
        symlink(&external, target.join("debug/build/foo")).unwrap();
        assert_eq!(relocate(&target, &target).unwrap(), 0);
        assert!(unit.join("fingerprint/dep-bin").is_file());
        let _ = fs::remove_dir_all(root);
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
        fs::write(target.join("debug/deps/serde-eee.d"), format!("{old}/serde-eee.d: {}/idx/serde/lib.rs\n", here.display()))
            .unwrap();
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
        unit(
            &target,
            "dir-bbb",
            "my-y",
            &format!("{old}/dir-bbb.d: src/lib.rs\n\nsrc/lib.rs:\n\n# env-dep:CARGO_MANIFEST_DIR=/lines/old/y\n"),
        );
        unit(
            &target,
            "shared-ccc",
            "shared",
            &format!("{old}/shared-ccc.d: /lines/old/shared/src/lib.rs\n\n/lines/old/shared/src/lib.rs:\n"),
        );
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
        unit(
            &target,
            "it-fff",
            "my-x",
            &format!("{old}/it-fff.d: tests/it.rs\n\n# env-dep:CARGO_BIN_EXE_x=/lines/old/target/debug/x\n"),
        );
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
        assert_eq!(relocate(&target, &target).unwrap(), 4);
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
