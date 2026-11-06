//! The node adapters: pnpm (`node_modules` next to `pnpm-lock.yaml`) and npm (next to `package-lock.json`).
//! A prune makes `node_modules` match the lockfile, offline and with no lifecycle script. pnpm:
//! 1. `pnpm install --frozen-lockfile`: the lockfile must match `package.json`, and the store must hold every
//!    package that the lockfile needs. pnpm creates folders before it finds a missing package, so the
//!    folder is copied first and restored on any failure.
//! 2. `pnpm prune`: removes the packages that the lockfile no longer has, from `node_modules` and `.pnpm`.
//!
//! npm: `npm prune --offline`, the same copy and restore. A warm step (the follow pass) installs online with
//! the scripts, as the project's own install does. npm never runs `npm ci`: it deletes the whole folder.

use crate::{exec, skip, sweep};
use anyhow::{Context, Result};
use layr_adapter::{CacheAdapter, Host, Outcome};
use std::path::Path;

/// Apple's cp supports clone copies (-c); a GNU cp earlier in PATH does not.
const MACOS_CP: &str = "/bin/cp";

/// A node package tool: its program, its lockfile, the commands of a prune (offline, no scripts) and of a warm
/// step (online, with scripts).
pub struct Tool {
    pub program: &'static str,
    pub lockfile: &'static str,
    pub prune: &'static [&'static [&'static str]],
    pub warm: &'static [&'static str],
    /// The install that makes a damaged folder whole again from the local store (`repair_with`).
    pub repair: &'static [&'static str],
}

pub const PNPM: Tool = Tool {
    program: "pnpm",
    lockfile: "pnpm-lock.yaml",
    prune: &[
        &["install", "--frozen-lockfile", "--offline", "--ignore-scripts", "--reporter=silent"],
        &["prune", "--ignore-scripts", "--reporter=silent"],
    ],
    warm: &["install", "--frozen-lockfile", "--reporter=silent"],
    repair: &["install", "--frozen-lockfile", "--offline", "--force", "--ignore-scripts", "--reporter=silent"],
};

pub const NPM: Tool = Tool {
    program: "npm",
    lockfile: "package-lock.json",
    prune: &[&["prune", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error"]],
    warm: &["install", "--no-audit", "--no-fund", "--loglevel=error"],
    repair: &["ci", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error"],
};

/// The pnpm adapter (`layr-adapter`): `node_modules` next to `pnpm-lock.yaml`.
pub struct Pnpm;
/// The npm adapter: `node_modules` next to `package-lock.json` (and no `pnpm-lock.yaml`).
pub struct Npm;

fn detects_with(tool: &Tool, folder: &Path) -> bool {
    folder.file_name().is_some_and(|n| n == "node_modules") && folder.parent().is_some_and(|r| r.join(tool.lockfile).is_file())
}

fn recover_with(folder: &Path) -> Result<bool, String> {
    let Some(root) = folder.parent() else { return Err("no project folder".into()) };
    let copy_path = root.join("node_modules.layr-prune");
    // A copy that a stop cut short is no copy: only a finished copy has the final name.
    let _ = std::fs::remove_dir_all(part_of(&copy_path));
    if !copy_path.exists() {
        return Ok(false);
    }
    restore(folder, &copy_path).map(|_| true).map_err(|e| format!("{e:#}"))
}

/// Installs every package of the lockfile again from the local store, over the files that are there (a folder
/// that an interrupted step left with missing files). Offline: the store has each package of the lockfile.
pub fn repair_with(tool: &Tool, host: &dyn Host, folder: &Path) -> Result<()> {
    let named = folder.file_name().is_some_and(|n| n == "node_modules");
    let Some(root) = folder.parent().filter(|r| named && r.join(tool.lockfile).is_file()) else {
        anyhow::bail!("{} needs a folder named node_modules next to {}", tool.program, tool.lockfile);
    };
    let files = ProjectFiles::read(tool, root);
    let done = exec(host, tool.program, |c| c.args(tool.repair).current_dir(root)).map(drop);
    files.put_back()?;
    done.with_context(|| format!("{} {}", tool.program, tool.repair.join(" ")))
}

impl CacheAdapter for Pnpm {
    fn name(&self) -> &'static str {
        "pnpm"
    }

    fn detects(&self, folder: &Path) -> bool {
        detects_with(&PNPM, folder)
    }

    fn prune(&self, host: &dyn Host, folder: &Path) -> Outcome {
        prune_with(&PNPM, host, folder)
    }

    fn warm(&self, host: &dyn Host, folder: &Path) -> Result<(), String> {
        warm_with(&PNPM, host, folder).map_err(|e| format!("{e:#}"))
    }

    fn recover(&self, folder: &Path) -> Result<bool, String> {
        recover_with(folder)
    }

    fn repair(&self, host: &dyn Host, folder: &Path) -> Result<(), String> {
        repair_with(&PNPM, host, folder).map_err(|e| format!("{e:#}"))
    }
}

impl CacheAdapter for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn detects(&self, folder: &Path) -> bool {
        // A pnpm project that also has a package-lock.json is pnpm's.
        detects_with(&NPM, folder) && !detects_with(&PNPM, folder)
    }

    fn prune(&self, host: &dyn Host, folder: &Path) -> Outcome {
        prune_with(&NPM, host, folder)
    }

    fn warm(&self, host: &dyn Host, folder: &Path) -> Result<(), String> {
        warm_with(&NPM, host, folder).map_err(|e| format!("{e:#}"))
    }

    fn recover(&self, folder: &Path) -> Result<bool, String> {
        recover_with(folder)
    }

    fn repair(&self, host: &dyn Host, folder: &Path) -> Result<(), String> {
        repair_with(&NPM, host, folder).map_err(|e| format!("{e:#}"))
    }
}

/// The pnpm prune (`prune_with` for pnpm).
pub fn prune(host: &dyn Host, folder: &Path) -> Outcome {
    prune_with(&PNPM, host, folder)
}

/// The pnpm warm step (`warm_with` for pnpm).
pub fn warm(host: &dyn Host, folder: &Path) -> Result<()> {
    warm_with(&PNPM, host, folder)
}

/// Installs the lockfile into `folder`, a `node_modules` folder: online (a merged change can add a package) and
/// with the lifecycle scripts, as the project's own install runs. The folder is copied first and restored on a
/// failure, as in a prune; `recover` puts the copy back after a stop at the hard limit.
pub fn warm_with(tool: &Tool, host: &dyn Host, folder: &Path) -> Result<()> {
    let files = folder.parent().map(|root| ProjectFiles::read(tool, root));
    let warmed = warm_files_kept(tool, host, folder);
    if let Some(files) = files {
        files.put_back()?;
    }
    warmed
}

fn warm_files_kept(tool: &Tool, host: &dyn Host, folder: &Path) -> Result<()> {
    let named = folder.file_name().is_some_and(|n| n == "node_modules");
    let Some(root) = folder.parent().filter(|r| named && r.join(tool.lockfile).is_file()) else {
        anyhow::bail!("{} needs a folder named node_modules next to {}", tool.program, tool.lockfile);
    };
    let copy_path = root.join("node_modules.layr-prune");
    if copy_path.exists() {
        restore(folder, &copy_path).context("cannot restore node_modules from its copy")?;
    }
    if folder.exists() {
        copy(host, folder, &copy_path).context("cannot copy node_modules")?;
    }
    if let Err(e) = exec(host, tool.program, |c| c.args(tool.warm).current_dir(root)) {
        let what = format!("{} {}", tool.program, tool.warm[0]);
        if copy_path.exists() {
            restore(folder, &copy_path).with_context(|| format!("{what}: {e:#}; restoring node_modules failed"))?;
        }
        return Err(e.context(what));
    }
    let _ = std::fs::remove_dir_all(&copy_path);
    Ok(())
}

/// The project files next to `node_modules` that a tool may write (`npm prune` writes `package-lock.json` again,
/// with its own format and a new time): their bytes and times before a step, put back after it. A cache step
/// changes `node_modules` only; the files of the line stay as the line saved them.
struct ProjectFiles(Vec<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)>);

impl ProjectFiles {
    fn read(tool: &Tool, root: &Path) -> ProjectFiles {
        let names = [tool.lockfile, "package.json"];
        ProjectFiles(
            names
                .iter()
                .map(|name| root.join(name))
                .filter_map(|path| {
                    let bytes = std::fs::read(&path).ok()?;
                    let time = std::fs::metadata(&path).ok()?.modified().ok()?;
                    Some((path, bytes, time))
                })
                .collect(),
        )
    }

    fn put_back(&self) -> Result<()> {
        for (path, bytes, time) in &self.0 {
            if std::fs::read(path).ok().as_ref() != Some(bytes) {
                std::fs::write(path, bytes).with_context(|| format!("cannot write {} back", path.display()))?;
            }
            if std::fs::metadata(path).and_then(|m| m.modified()).ok() != Some(*time) {
                std::fs::File::options().write(true).open(path)?.set_modified(*time)?;
            }
        }
        Ok(())
    }
}

/// Prunes `folder`, a `node_modules` folder, with `tool`. Any failure restores the folder and skips it.
pub fn prune_with(tool: &Tool, host: &dyn Host, folder: &Path) -> Outcome {
    let Some(root) = folder.parent() else { return skip("no project folder".to_string()) };
    let files = ProjectFiles::read(tool, root);
    let outcome = prune_files_kept(tool, host, folder);
    match files.put_back() {
        Ok(()) => outcome,
        Err(e) => skip(format!("{e:#}")),
    }
}

fn prune_files_kept(tool: &Tool, host: &dyn Host, folder: &Path) -> Outcome {
    let named = folder.file_name().is_some_and(|n| n == "node_modules");
    let Some(root) = folder.parent().filter(|r| named && r.join(tool.lockfile).is_file()) else {
        return skip(format!("{} needs a folder named node_modules next to {}", tool.program, tool.lockfile));
    };
    let copy_path = root.join("node_modules.layr-prune");
    // A copy that an interrupted prune left: put it back first, so the folder is the state before that prune.
    if copy_path.exists() {
        if let Err(e) = restore(folder, &copy_path) {
            return skip(format!("cannot restore node_modules from its copy: {e:#}"));
        }
    }
    let before = sweep::tree_bytes(folder);
    if let Err(e) = copy(host, folder, &copy_path) {
        return skip(format!("cannot copy node_modules: {e:#}"));
    }
    for args in tool.prune {
        if let Err(e) = exec(host, tool.program, |c| c.args(*args).current_dir(root)) {
            return match restore(folder, &copy_path) {
                Ok(()) => skip(format!("{} {}: {e:#}; node_modules unchanged", tool.program, args[0])),
                Err(r) => skip(format!(
                    "{} {}: {e:#}; restoring node_modules failed, the copy is at {}: {r:#}",
                    tool.program,
                    args[0],
                    copy_path.display()
                )),
            };
        }
    }
    let freed = before.saturating_sub(sweep::tree_bytes(folder));
    // A copy that stays behind is reported by the check at the start of the next prune.
    let _ = std::fs::remove_dir_all(&copy_path);
    Outcome::Pruned { freed }
}

/// The name of a copy while it is made: only a finished copy gets its final name, so a stop in the middle of the
/// copy never leaves a partial copy that a recovery would put in place of the whole folder.
fn part_of(dst: &Path) -> std::path::PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dst.with_file_name(name)
}

/// A copy-on-write copy of `src` at `dst`, with its times and modes. A failed copy leaves nothing behind.
fn copy(host: &dyn Host, src: &Path, dst: &Path) -> Result<()> {
    let program = if cfg!(target_os = "macos") { MACOS_CP } else { "cp" };
    let part = part_of(dst);
    let _ = std::fs::remove_dir_all(&part);
    let made = exec(host, program, |c| {
        if cfg!(target_os = "macos") {
            c.args(["-cRp"]).arg(src).arg(&part)
        } else {
            c.args(["-a", "--reflink=auto"]).arg(src).arg(&part)
        }
    })
    .map(drop)
    .and_then(|()| std::fs::rename(&part, dst).context("cannot name the finished copy"));
    if made.is_err() {
        let _ = std::fs::remove_dir_all(&part);
    }
    made
}

/// Puts the copy of `folder` back in place of the changed `folder`. Two renames, then the slow delete: a stop at
/// any point leaves either the copy or the restored folder whole (the changed folder waits in `.layr-broken`).
fn restore(folder: &Path, copy_path: &Path) -> Result<()> {
    let broken = folder.with_file_name("node_modules.layr-broken");
    let _ = std::fs::remove_dir_all(&broken);
    if folder.exists() {
        std::fs::rename(folder, &broken).context("cannot move the changed folder aside")?;
    }
    std::fs::rename(copy_path, folder).context("cannot move the copy back")?;
    let _ = std::fs::remove_dir_all(&broken);
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn clone_copy_uses_the_system_cp_instead_of_path() {
        struct CheckProgram;
        impl Host for CheckProgram {
            fn command(&self, program: &str) -> std::io::Result<std::process::Command> {
                assert_eq!(program, "/bin/cp");
                Err(std::io::ErrorKind::NotFound.into())
            }

            fn deadline(&self) -> std::time::Instant {
                unreachable!("the test host never starts a process")
            }
        }

        // Stop at command selection; no filesystem copy or PATH lookup occurs.
        assert!(copy(&CheckProgram, Path::new(""), Path::new("")).is_err());
    }

    fn project(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-pnpm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "whole").unwrap();
        root
    }

    #[test]
    fn a_copy_cut_short_is_never_put_back() {
        // A stop in the middle of the copy leaves only the part: the folder stays as it is.
        let root = project("part");
        std::fs::create_dir_all(root.join("node_modules.layr-prune.part/pkg")).unwrap();
        assert_eq!(recover_with(&root.join("node_modules")), Ok(false));
        assert_eq!(std::fs::read_to_string(root.join("node_modules/pkg/index.js")).unwrap(), "whole");
        assert!(!root.join("node_modules.layr-prune.part").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_finished_copy_is_put_back_by_renames_and_leaves_nothing_behind() {
        let root = project("restore");
        std::fs::create_dir_all(root.join("node_modules.layr-prune/pkg")).unwrap();
        std::fs::write(root.join("node_modules.layr-prune/pkg/index.js"), "copy").unwrap();
        std::fs::remove_file(root.join("node_modules/pkg/index.js")).unwrap();
        assert_eq!(recover_with(&root.join("node_modules")), Ok(true));
        assert_eq!(std::fs::read_to_string(root.join("node_modules/pkg/index.js")).unwrap(), "copy");
        let mut left: Vec<String> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        left.sort();
        assert_eq!(left, vec!["node_modules".to_string()]);
        let _ = std::fs::remove_dir_all(root);
    }
}
