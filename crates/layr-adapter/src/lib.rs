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

/// A cache adapter. It prunes a cache folder of a line to its live set.
pub trait CacheAdapter: Send + Sync {
    /// The adapter name. It is the value of the overlay property `prune`.
    fn name(&self) -> &'static str;
    /// Whether the adapter applies to `folder` when the overlay names no adapter.
    fn detects(&self, folder: &Path) -> bool;
    /// Prunes `folder`. It never compiles, never runs a package script and never uses the network. A stale or
    /// failing state returns `Outcome::Skipped` and leaves the folder as it was.
    fn prune(&self, host: &dyn Host, folder: &Path) -> Outcome;
    /// Puts `folder` back in the state before an interrupted `prune` (its processes were stopped at the hard
    /// limit). Ok(true): it restored a copy. Ok(false): there is nothing to restore. Err: the folder may be
    /// changed and is not restored. Runs no process, so it needs no host.
    fn recover(&self, folder: &Path) -> Result<bool, String> {
        let _ = folder;
        Ok(false)
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
