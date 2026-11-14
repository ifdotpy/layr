//! Optional candidate evidence for a specific pair of trees.
//!
//! Providers must include every changed entry (including metadata, rename descendants and
//! hard-link aliases). Candidates carry observations/ranges so comparison can reuse them.
//! `Complete` is valid only relative to the supplied base; an unknown relationship, unsupported
//! operation or failed observation must return `Partial` or an error. Neither authorizes pruning.
use crate::compare::EntryChange;
use anyhow::Result;
use std::path::Path;

#[must_use]
pub(crate) enum HintSet {
    Complete(Vec<EntryChange>),
    /// No completeness proof. Discard the hints and scan the entire pair.
    Partial,
}

pub(crate) trait ChangeHints: Sync {
    /// Both trees must be stable for the duration of comparison. A live working directory
    /// must first be captured; this contract is not a journal since the last cached status.
    fn between(&self, base: &Path, tree: &Path) -> Result<HintSet>;
}
