//! Optional candidate evidence for a specific pair of trees.
//!
//! Providers must include every changed entry (including metadata, rename descendants and
//! hard-link aliases). Candidates carry observations/ranges so comparison can reuse them.
//! `Complete` is valid only relative to the supplied base; an unknown relationship, unsupported
//! operation or failed observation must return `Partial` or an error. Neither authorizes pruning.
use crate::evidence::EntryChange;
use crate::FsError;
use std::path::Path;

#[must_use]
pub enum HintSet {
    Complete(Vec<EntryChange>),
    /// No completeness proof. Discard the hints and scan the entire pair.
    Partial,
}

pub trait PathChangeHints: Sync {
    /// Both trees must be stable for the duration of comparison. A live working directory
    /// must first be captured; this contract is not a journal since the last cached status.
    fn between(&self, base: &Path, tree: &Path) -> Result<HintSet, FsError>;
}

/// Candidate evidence bound to issued immutable trees. Partial evidence always selects a scan.
pub trait ChangeHints: Send + Sync {
    fn between(&self, base: &crate::FrozenTree, tree: &crate::FrozenTree) -> Result<HintSet, FsError>;
}
