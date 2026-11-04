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

/// A node package tool: its program, its lockfile, the commands of a prune (offline, no scripts) and of a warm
/// step (online, with scripts).
pub struct Tool {
    pub program: &'static str,
    pub lockfile: &'static str,
    pub prune: &'static [&'static [&'static str]],
    pub warm: &'static [&'static str],
}

pub const PNPM: Tool = Tool {
    program: "pnpm",
    lockfile: "pnpm-lock.yaml",
    prune: &[
        &["install", "--frozen-lockfile", "--offline", "--ignore-scripts", "--reporter=silent"],
        &["prune", "--ignore-scripts", "--reporter=silent"],
    ],
    warm: &["install", "--frozen-lockfile", "--reporter=silent"],
};

pub const NPM: Tool = Tool {
    program: "npm",
    lockfile: "package-lock.json",
    prune: &[&["prune", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error"]],
    warm: &["install", "--no-audit", "--no-fund", "--loglevel=error"],
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
    if !copy_path.exists() {
        return Ok(false);
    }
    restore(folder, &copy_path).map(|_| true).map_err(|e| format!("{e:#}"))
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

    fn warm(&self, host: &dyn Host, folder: &Path) -> Result<(), String> {
        warm(host, folder).map_err(|e| format!("{e:#}"))
    }

    fn recover(&self, folder: &Path) -> Result<bool, String> {
        recover_with(folder)
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
}

<<<<<<< head
/// The pnpm prune (`prune_with` for pnpm).
||||||| base
/// Prunes `folder`, a `node_modules` folder. Any failure restores the folder and skips it.
=======
/// The install of a warm step: online (a merged change can add a package) and with the lifecycle scripts, as the
/// project's own install runs.
const WARM: &[&str] = &["install", "--frozen-lockfile", "--reporter=silent"];

/// Installs the lockfile into `folder`, a `node_modules` folder. The folder is copied first and restored on a
/// failure, as in `prune`; `recover` puts the copy back after a stop at the hard limit.
pub fn warm(host: &dyn Host, folder: &Path) -> Result<()> {
    let named = folder.file_name().is_some_and(|n| n == "node_modules");
    let Some(root) = folder.parent().filter(|r| named && r.join("pnpm-lock.yaml").is_file()) else {
        anyhow::bail!("pnpm needs a folder named node_modules next to pnpm-lock.yaml");
    };
    let copy_path = root.join("node_modules.layr-prune");
    if copy_path.exists() {
        restore(folder, &copy_path).context("cannot restore node_modules from its copy")?;
    }
    if folder.exists() {
        copy(host, folder, &copy_path).context("cannot copy node_modules")?;
    }
    if let Err(e) = exec(host, "pnpm", |c| c.args(WARM).current_dir(root)) {
        if copy_path.exists() {
            restore(folder, &copy_path).with_context(|| format!("pnpm install: {e:#}; restoring node_modules failed"))?;
        }
        return Err(e.context("pnpm install"));
    }
    let _ = std::fs::remove_dir_all(&copy_path);
    Ok(())
}

/// Prunes `folder`, a `node_modules` folder. Any failure restores the folder and skips it.
>>>>>>> origin-https/main
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

/// Prunes `folder`, a `node_modules` folder, with `tool`. Any failure restores the folder and skips it.
pub fn prune_with(tool: &Tool, host: &dyn Host, folder: &Path) -> Outcome {
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

/// A copy-on-write copy of `src` at `dst`, with its times and modes. A failed copy leaves nothing behind.
fn copy(host: &dyn Host, src: &Path, dst: &Path) -> Result<()> {
    let made = exec(host, "cp", |c| {
        if cfg!(target_os = "macos") {
            c.args(["-cRp"]).arg(src).arg(dst)
        } else {
            c.args(["-a", "--reflink=auto"]).arg(src).arg(dst)
        }
    })
    .map(drop);
    if made.is_err() {
        let _ = std::fs::remove_dir_all(dst);
    }
    made
}

/// Puts the copy of `folder` back in place of the changed `folder`.
fn restore(folder: &Path, copy_path: &Path) -> Result<()> {
    std::fs::remove_dir_all(folder).context("cannot remove the changed folder")?;
    std::fs::rename(copy_path, folder).context("cannot move the copy back")
}
