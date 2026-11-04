//! The pnpm adapter. It prunes `node_modules` (next to `pnpm-lock.yaml`) to the lockfile, in two pnpm
//! commands. Both run offline, with no lifecycle script:
//! 1. `pnpm install --frozen-lockfile`: the lockfile must match `package.json`, and the store must hold every
//!    package that the lockfile needs. pnpm creates folders before it finds a missing package, so the
//!    folder is copied first and restored on any failure.
//! 2. `pnpm prune`: removes the packages that the lockfile no longer has, from `node_modules` and `.pnpm`.

use crate::{output, skip, sweep, Outcome};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// The pnpm commands of a prune, in order.
const STEPS: [&[&str]; 2] = [
    &["install", "--frozen-lockfile", "--offline", "--ignore-scripts", "--reporter=silent"],
    &["prune", "--ignore-scripts", "--reporter=silent"],
];

/// Prunes `folder`, a `node_modules` folder. Any failure restores the folder and skips it.
pub fn prune(folder: &Path) -> Outcome {
    let named = folder.file_name().is_some_and(|n| n == "node_modules");
    let Some(root) = folder.parent().filter(|r| named && r.join("pnpm-lock.yaml").is_file()) else {
        return skip("pnpm needs a folder named node_modules next to pnpm-lock.yaml");
    };
    let copy_path = root.join("node_modules.layr-prune");
    if copy_path.exists() {
        return skip(format!("{} is left from an earlier run: remove it first", copy_path.display()));
    }
    let before = sweep::tree_bytes(folder);
    if let Err(e) = copy(folder, &copy_path) {
        return skip(format!("cannot copy node_modules: {e:#}"));
    }
    for args in STEPS {
        if let Err(e) = run(root, args) {
            return match restore(folder, &copy_path) {
                Ok(()) => skip(format!("pnpm {}: {e:#}; node_modules unchanged", args[0])),
                Err(r) => skip(format!(
                    "pnpm {}: {e:#}; restoring node_modules failed, the copy is at {}: {r:#}",
                    args[0],
                    copy_path.display()
                )),
            };
        }
    }
    let freed = before.saturating_sub(sweep::tree_bytes(folder));
    // A copy that stays behind is reported by the check at the start of the next prune.
    let _ = std::fs::remove_dir_all(&copy_path);
    Outcome::Pruned { adapter: "pnpm", freed }
}

/// Runs one pnpm command in the project folder `root`. The command reads no input and has no network.
fn run(root: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("pnpm");
    cmd.args(args).current_dir(root).stdin(Stdio::null()).env("pnpm_config_offline", "true");
    output(&mut cmd).map(drop)
}

/// A copy-on-write copy of `src` at `dst`, with its times and modes. A failed copy leaves nothing behind.
fn copy(src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new("cp");
    if cfg!(target_os = "macos") {
        cmd.args(["-cRp"]);
    } else {
        cmd.args(["-a", "--reflink=auto"]);
    }
    let made = output(cmd.arg(src).arg(dst)).map(drop);
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
