//! Optional candidate evidence for a specific pair of trees.
//!
//! Providers must include every changed entry (including metadata, rename descendants and
//! hard-link aliases). Candidates carry observations/ranges so comparison can reuse them.
//! `Complete` is valid only relative to the supplied base; an unknown relationship, unsupported
//! operation or failed observation must return `Partial` or an error. Neither authorizes pruning.
use crate::evidence::EntryChange;
use crate::FsError;

#[must_use]
pub enum HintSet {
    Complete(Vec<EntryChange>),
    /// No completeness proof. Discard the hints and scan the entire pair.
    Partial,
}

/// Candidate evidence bound to issued immutable trees. Partial evidence always selects a scan.
pub trait ChangeHints: Send + Sync {
    fn between(
        &self,
        base: &crate::FrozenTree,
        tree: &crate::FrozenTree,
        request: crate::ChangeRequest,
    ) -> Result<HintSet, FsError>;
}
