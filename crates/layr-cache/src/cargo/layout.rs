//! Cargo profile layouts. Old caches can coexist with units built by newer Cargo.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Legacy,
    UnitDirectories,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnitPath {
    pub profile: String,
    pub hash: String,
}

#[derive(Debug)]
pub struct Unit {
    pub name: String,
    pub hash: String,
    pub fingerprint: PathBuf,
    pub outputs: PathBuf,
    pub directory: PathBuf,
    pub layout: Layout,
}

fn safe_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.file_type().is_dir())
}

fn entries(path: &Path) -> impl Iterator<Item = std::fs::DirEntry> {
    safe_dir(path).then(|| std::fs::read_dir(path).ok()).flatten().into_iter().flatten().flatten()
}

fn hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Layout {
    /// All recognized units, including both layouts in a migrated profile.
    pub fn units(profile: &Path) -> Vec<Unit> {
        let mut units = Vec::new();
        let deps = profile.join("deps");
        let legacy_deps_safe = deps.symlink_metadata().map_or(true, |m| m.file_type().is_dir());
        for e in entries(&profile.join(".fingerprint")).filter(|_| legacy_deps_safe) {
            let Some(stem) = e.file_name().to_str().map(str::to_owned) else { continue };
            let Some((name, h)) = stem.rsplit_once('-') else { continue };
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            units.push(Unit {
                name: name.into(),
                hash: h.into(),
                fingerprint: e.path(),
                outputs: profile.join("deps"),
                directory: e.path(),
                layout: Self::Legacy,
            });
        }
        for package in entries(&profile.join("build")) {
            let Some(name) = package.file_name().to_str().map(str::to_owned) else { continue };
            if !package.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for e in entries(&package.path()) {
                let Some(h) = e.file_name().to_str().map(str::to_owned) else { continue };
                if !hash(&h) || !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let fingerprint = e.path().join("fingerprint");
                let outputs = e.path().join("out");
                if [&fingerprint, &outputs].iter().any(|p| p.symlink_metadata().is_ok_and(|m| !m.file_type().is_dir())) {
                    continue;
                }
                units.push(Unit {
                    name: name.clone(),
                    hash: h,
                    fingerprint,
                    outputs,
                    directory: e.path(),
                    layout: Self::UnitDirectories,
                });
            }
        }
        units
    }

    /// Directories containing compiler outputs, in either layout.
    pub fn output_dirs(profile: &Path) -> Vec<PathBuf> {
        let mut dirs: std::collections::BTreeSet<_> = Self::units(profile).into_iter().map(|u| u.outputs).collect();
        if safe_dir(&profile.join("deps")) {
            dirs.insert(profile.join("deps"));
        }
        dirs.into_iter().collect()
    }

    /// Saved stdout of build script runs, paired with their package names.
    pub fn build_script_outputs(profile: &Path) -> Vec<(String, PathBuf)> {
        let mut outputs = Vec::new();
        for e in entries(&profile.join("build")) {
            let Some(name) = e.file_name().to_str().map(str::to_owned) else { continue };
            let old = e.path().join("output");
            if old.symlink_metadata().is_ok_and(|m| m.file_type().is_file()) {
                outputs.push((name.rsplit_once('-').map_or(name.as_str(), |(p, _)| p).into(), old));
            }
        }
        for unit in Self::units(profile).into_iter().filter(|u| u.layout == Self::UnitDirectories) {
            let path = unit.directory.join("run/stdout");
            if safe_dir(&unit.directory.join("run")) && path.symlink_metadata().is_ok_and(|m| m.file_type().is_file()) {
                outputs.push((unit.name, path));
            }
        }
        outputs.sort();
        outputs
    }

    pub fn has_units(profile: &Path) -> bool {
        safe_dir(&profile.join(".fingerprint"))
            || entries(&profile.join("build")).filter(|p| p.file_type().is_ok_and(|t| t.is_dir())).any(|package| {
                entries(&package.path())
                    .any(|unit| unit.file_name().to_str().is_some_and(hash) && unit.file_type().is_ok_and(|t| t.is_dir()))
            })
    }

    /// Profile and hash for a path belonging to either layout.
    pub fn path_unit(path: &str) -> Option<UnitPath> {
        let parts: Vec<_> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            if *part == "build" {
                if let (Some(_), Some(h)) = (parts.get(i + 1), parts.get(i + 2)) {
                    if hash(h) {
                        return Some(UnitPath { profile: parts[..i].join("/"), hash: (*h).into() });
                    }
                }
            }
            if matches!(*part, "deps" | ".fingerprint" | "build" | "incremental") {
                if let Some(p) = parts.get(i + 1) {
                    let stem = p.split('.').next().unwrap_or(p);
                    if let Some((name, h)) = stem.rsplit_once('-') {
                        if !name.is_empty() && hash(h) {
                            return Some(UnitPath { profile: parts[..i].join("/"), hash: h.into() });
                        }
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Layout, UnitPath};

    #[test]
    fn custom_profile_named_build_keeps_unit_files_together() {
        let hash = "0123456789abcdef";
        let fingerprint = format!("build/build/foo/{hash}/fingerprint/lib-foo");
        let output = format!("build/build/foo/{hash}/out/libfoo-{hash}.rlib");
        let expected = Some(UnitPath { profile: "build".into(), hash: hash.into() });
        assert_eq!(Layout::path_unit(&fingerprint), expected);
        assert_eq!(Layout::path_unit(&output), expected);
        assert_eq!(Layout::path_unit(&format!("build/.fingerprint/foo-{hash}/lib-foo")), expected);
        assert_eq!(Layout::path_unit(&format!("build/deps/libfoo-{hash}.rlib")), expected);
    }

    #[test]
    fn legacy_unit_wins_over_nested_generated_build_directory() {
        let expected = Some(UnitPath { profile: "debug".into(), hash: "0123456789abcdef".into() });
        assert_eq!(Layout::path_unit("debug/build/foo-0123456789abcdef/out/build/generated/1111111111111111/file"), expected);
    }

    #[test]
    fn paths_group_legacy_and_unit_directory_files() {
        let hash = "0123456789abcdef";
        for path in [
            format!("debug/.fingerprint/foo-{hash}/dep-lib-foo"),
            format!("debug/deps/libfoo-{hash}.rlib"),
            format!("debug/build/foo/{hash}/fingerprint/dep-lib-foo"),
            format!("debug/build/foo/{hash}/out/libfoo-{hash}.rlib"),
        ] {
            assert_eq!(Layout::path_unit(&path), Some(UnitPath { profile: "debug".into(), hash: hash.into() }));
        }
    }
}
