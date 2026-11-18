//! Synchronous ports between local replica policy and a remote transfer session.
//!
//! LocalReplica metadata is supplied by the application while the implementation still lives there.
//! Its associated types are owned record/state/selection values, never local storage or transport handles.
//! LocalReplica is coordinator-owned: no Send/Sync bound and no project lock spans a transport call.

use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub struct Imported {
    pub count: usize,
    pub bootstrap: bool,
    pub project: String,
}

pub struct FetchSelection<S, L> {
    pub missing: Vec<S>,
    pub left: Vec<String>,
    pub omitted: Vec<String>,
    pub clone_lines: Option<Vec<L>>,
    pub restricted: bool,
}

/// A selected local source protected by command-duration pins. Dropping a view does not release them.
/// Opening it may rebuild local objects/Git content but must never initiate a remote restore.
pub trait StateRead {
    fn legacy_root(&self) -> &Path;
}

pub trait LocalReplica {
    type State;
    type Line;
    type Selection;

    /// Visit exact signed bytes; None visits all machines, Some visits one machine after seq.
    fn visit_records(&self, machine: Option<(&str, u64)>, visit: &mut dyn FnMut(&str) -> Result<()>) -> Result<()>;
    fn record_machines(&self) -> Result<Vec<String>>;
    fn record_line(&self, machine: &str, seq: u64) -> Result<Option<String>>;
    fn verify_records(&self) -> Result<()>;
    fn import_records(&mut self, lines: &[String], keys: &BTreeMap<String, VerifyingKey>) -> Result<Imported>;
    fn finish_import(&mut self, keys: &BTreeMap<String, VerifyingKey>) -> Result<()>;
    fn fetch_selection(
        &mut self,
        records: &[String],
        selection: &Self::Selection,
        available: &[String],
        clone: bool,
    ) -> Result<FetchSelection<Self::State, Self::Line>>;
    /// Optional W7 local-presence preflight. None requests remote discovery; it must
    /// not import remote availability or treat unpublished record-only states as held.
    fn fetch_preflight(
        &mut self,
        _records: &[String],
        _selection: &Self::Selection,
        _clone: bool,
    ) -> Result<Option<FetchSelection<Self::State, Self::Line>>> {
        Ok(None)
    }
    /// Optional unused pid-named directory in the replica's swept work area.
    /// None disables the rebuildable operation cache; cache failure cannot fail fetch.
    fn fetch_temp_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn push_candidates(
        &self,
        records: &[String],
        selection: &Self::Selection,
        present: &BTreeSet<String>,
        omitted: &mut Vec<String>,
    ) -> Result<Vec<Self::State>>;
    fn omitted_cache_refs(&self, selection: &Self::Selection) -> Result<Vec<String>>;
    fn tips(&self) -> Result<BTreeSet<String>>;
    /// The first ancestor of `id` (breadth first, without `id`) that `wanted` takes. The walk goes no further.
    fn nearest_ancestor(&self, id: &str, wanted: &mut dyn FnMut(&str) -> bool) -> Result<Option<String>>;
    fn pin_state(&self, id: &str) -> Result<()>;
    fn check_secrets(&self, ids: &[String]) -> Result<()>;
    fn has_state(&self, id: &str) -> bool;
    fn is_overlay(&self) -> bool;
    /// The caller pins the selected state for the command before opening a view.
    fn open_state(&self, id: &str) -> Result<Box<dyn StateRead + '_>>;
    /// Choose a local parent without opening it or initiating network access.
    fn restore_base(&self, state: &Self::State, available: &BTreeMap<String, String>) -> Option<String>;
    /// Stage a restore from a selected local parent, or an empty tree. No lock escapes this call.
    fn stage_state(&mut self, parent: Option<&str>) -> Result<std::path::PathBuf>;
    /// Publish the completed stage using the existing restore/publish path. Failure must abort the stage.
    fn install_state(&mut self, state: &Self::State, transid: u64, files: usize) -> Result<usize>;
    fn abort_state(&mut self, error: anyhow::Error) -> anyhow::Error;

    // Imported Git history is a separate existing codec; only its local operations belong here.
    fn git_has_store(&self) -> bool;
    fn git_init(&self) -> Result<()>;
    fn git_imported(&self) -> Result<Vec<String>>;
    fn git_has_commits(&self, commits: &[String]) -> Result<Vec<bool>>;
    fn git_outside(&self, commits: &[String], covered: &[String]) -> Result<Vec<String>>;
    fn git_pack(&self, commits: &[String], covered: &[String]) -> Result<Vec<u8>>;
    fn git_install(&self, pack: &[u8]) -> Result<()>;
}

/// Application operation boundary. Context/local metadata remain application supplied while
/// orchestration lives there; transfer engines consume LocalReplica rather than this context.
pub trait RemoteSync<Context, Local, Selection> {
    type Report;
    fn fetch(&mut self, context: &Context, local: &mut Local, selection: &Selection) -> Result<Self::Report>;
    fn push(&mut self, context: &Context, local: &mut Local, selection: &Selection) -> Result<Self::Report>;
    fn clone_into(&mut self, context: &Context, local: &mut Local, selection: &Selection) -> Result<Self::Report>;
}
