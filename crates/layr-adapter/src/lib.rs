//! The interface between layr core and the cache adapters (issue #167). Core depends on this crate
//! only. An adapter crate implements `CacheAdapter`, and the binary registers the adapters at startup.
//! Core never names an adapter.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// What core gives an adapter: the processes to run, and the time limit.
pub trait Host {
    /// A command for `program` that runs as the line owner, with a clean environment and its own process
    /// group. An error when core cannot run commands for the owner.
    fn command(&self, program: &str) -> std::io::Result<Command>;
    /// The moment at which the adapter stops its processes and skips the folder.
    fn deadline(&self) -> Instant;
}

/// The command that names a toolchain (`CacheAdapter::toolchain_command`).
pub struct Toolchain {
    pub program: &'static str,
    pub args: &'static [&'static str],
    pub files: &'static [&'static str],
}

/// A cache adapter. It prunes a cache folder of a line to its live set.
pub trait CacheAdapter: Send + Sync {
    /// The adapter name. It is the value of the overlay property `prune`.
    fn name(&self) -> &'static str;
    /// Whether the adapter applies to `folder` when the overlay names no adapter.
    fn detects(&self, folder: &Path) -> bool;
    /// Prunes `folder`. It never compiles, never runs a package script and never uses the network. A stale or
    /// failing state returns `Outcome::Skipped` and leaves the folder as it was.
    fn prune(&self, host: &dyn Host, folder: &Path) -> Outcome;
    /// Builds the project's standard commands into `folder`, so that the folder matches the line's files. A follow
    /// pass runs it before the prune and the save, so lines from the followed line start warm after a change that
    /// came without a cache. Ok: the build ran. Err: the reason it did not (the folder stays consistent). The
    /// default has no build.
    fn warm(&self, host: &dyn Host, folder: &Path) -> Result<(), String> {
        let _ = (host, folder);
        Err("the adapter has no build".to_string())
    }
    /// Puts `folder` back in the state before an interrupted `prune` (its processes were stopped at the hard
    /// limit). Ok(true): it restored a copy. Ok(false): there is nothing to restore. Err: the folder may be
    /// changed and is not restored. Runs no process, so it needs no host.
    fn recover(&self, folder: &Path) -> Result<bool, String> {
        let _ = folder;
        Ok(false)
    }
    /// `folder` came from another path (the cache of a new line, or of a merge), and it is used at `at` from now on.
    /// Makes stale the build outputs that name the old place, so the next build makes them again. Returns the number
    /// of outputs made stale. Runs no process, so it needs no host. The default changes nothing.
    fn relocated(&self, folder: &Path, at: &Path) -> Result<usize, String> {
        let _ = (folder, at);
        Ok(0)
    }
    /// The environment variables that change what a build writes into `folder`. A cache built with other values
    /// builds again, so their values are parts of the cache key. The default names none.
    fn key_env(&self) -> &'static [&'static str] {
        &[]
    }
    /// The environment that the project pins for the builds of `folder` (it wins over the caller's), read from the
    /// project's files. Runs no process. The default pins nothing.
    fn pinned_env(&self, folder: &Path) -> Vec<(String, String)> {
        let _ = folder;
        Vec::new()
    }
    /// The command whose output names the toolchain that builds `folder`: the program, its arguments, and the
    /// files of the folder's parent that select the toolchain. Core runs it as the line owner in an empty folder
    /// with copies of those files; the output is a part of the cache key. The default has none.
    fn toolchain_command(&self) -> Option<Toolchain> {
        None
    }
}

/// What an adapter did to a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The stale entries are removed. `freed`: the bytes that the removal frees.
    Pruned { freed: u64 },
    /// The folder is unchanged. `reason`: why the adapter skipped it.
    Skipped { reason: String },
}
