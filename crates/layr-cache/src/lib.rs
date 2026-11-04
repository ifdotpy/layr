//! Language adapters that prune a cache folder of a line to its live set: the entries that the
//! project's standard commands still need. An adapter works offline and compiles nothing. A folder
//! that is stale, or that an adapter cannot prune safely, is skipped and left unchanged.
//! The contract and the overlay property are in docs/cache-overlays.md, "Pruning cache folders".

pub mod cargo;
pub mod pnpm;
pub mod sweep;

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// The adapter names. They are the values of the overlay property `prune=<name>|none`.
pub const ADAPTERS: [&str; 2] = ["cargo", "pnpm"];
/// The overlay property value that turns pruning off.
pub const NONE: &str = "none";

/// What prune did to a folder.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The stale entries are removed. `freed`: the bytes that the removal frees.
    Pruned { adapter: &'static str, freed: u64 },
    /// The folder is unchanged. `reason`: why the adapter skipped it.
    Skipped { reason: String },
}

/// Prunes the cache folder `folder` (an absolute path in a line). `setting` is the overlay property
/// `prune`: an adapter name, `none`, or None to detect the adapter (see `detect`). Returns an error only
/// for a setting that is not an adapter name. Every other outcome is an `Outcome`.
pub fn prune(setting: Option<&str>, folder: &Path) -> Result<Outcome> {
    let name = match setting {
        Some(NONE) => return Ok(skip("pruning is off (prune=none)")),
        Some(name) => name,
        None => match detect(folder) {
            Some(name) => name,
            None => return Ok(skip("no adapter matches the folder")),
        },
    };
    match name {
        "cargo" => Ok(cargo::prune(folder)),
        "pnpm" => Ok(pnpm::prune(folder)),
        other => bail!("unknown prune adapter '{other}' (cargo, pnpm or none)"),
    }
}

/// The adapter of a folder. `target` with `Cargo.toml` next to it is cargo. `node_modules` with
/// `pnpm-lock.yaml` next to it is pnpm. The file is looked up in the parent folder of `folder`.
pub fn detect(folder: &Path) -> Option<&'static str> {
    let root = folder.parent()?;
    match folder.file_name()?.to_str()? {
        "target" if root.join("Cargo.toml").is_file() => Some("cargo"),
        "node_modules" if root.join("pnpm-lock.yaml").is_file() => Some("pnpm"),
        _ => None,
    }
}

/// A skipped outcome with its reason.
pub(crate) fn skip(reason: impl Into<String>) -> Outcome {
    Outcome::Skipped { reason: reason.into() }
}

/// The stdout of a command. A command that fails is an error with its stderr.
pub fn output(cmd: &mut Command) -> Result<Vec<u8>> {
    let o = cmd.output().with_context(|| format!("cannot start {:?}", cmd.get_program()))?;
    if !o.status.success() {
        bail!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(o.stdout)
}
