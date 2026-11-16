//! Records of the operation log and the objects they describe.

use serde::{Deserialize, Serialize};
pub use crate::transitions::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Author {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub uid: u32,
    /// The label of the session that made it (a terminal, a CI job, a tool), from
    /// `LAYR_SESSION`. Free text: layr shows it and never decides anything from it.
    #[serde(default, alias = "agent", skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StateKind {
    /// A named state with a message.
    Commit,
    /// An automatic snapshot of a line's working content (an undo point).
    Auto,
    /// Saved work of `layr park`.
    Stash,
    /// A state made from a git commit of the remote.
    Import,
    /// Reusable data outside source history.
    Cache,
    /// Output of a computation outside source history.
    Derived,
}

impl StateKind {
    pub fn auxiliary(self) -> bool {
        matches!(self, Self::Cache | Self::Derived)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRec {
    pub id: String,
    pub kind: StateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// btrfs UUID of the snapshot on the machine that made it.
    pub subvol: String,
    pub author: Author,
    #[serde(default)]
    pub message: String,
    pub time: i64,
    /// Taken while processes ran in the line: crash-consistent only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
    /// Snapshots of regenerable caches: cache path to snapshot name in `states/`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(rename = "layers", alias = "caches")]
    pub caches: BTreeMap<String, String>,
    /// The user who made the cache snapshots (a uid of the state's machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "layer_by", alias = "cache_by")]
    pub cache_by: Option<u32>,
    /// The caches may go to lines of other users: they were saved on a protected line by someone
    /// who may save there. Other caches go only to lines of the user who made them, because a
    /// cache holds code that review does not see.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "layer_shared", alias = "cache_shared")]
    pub cache_shared: bool,
    /// The platform the cache snapshots were made on (`cache_platform()`). Build output of one
    /// operating system or processor does not run on another, so a line takes a cache only from
    /// its own platform. None: saved by an older layr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "layer_platform", alias = "cache_platform")]
    pub cache_platform: Option<String>,
    /// The parts of the cache key (`cache_platform`) by name, each a short hash of its value: key
    /// files (`file:<path>`), environment variables (`env:<name>`) and the toolchain. A line that
    /// does not take a cache names the parts that differ. Empty: saved by an older layr.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub cache_key_parts: std::collections::BTreeMap<String, String>,
    /// The part of this state on each overlay (docs/overlays.md, section 1a): the overlay name to
    /// the id of its state in the overlay store `<project>%<overlay>`. The base part is the state
    /// itself. Empty: a state without overlays, or one saved before parts (its overlay lines hold
    /// the history). The JSON key is `parts`, because `layers` is the key of `caches`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parts: BTreeMap<String, String>,
    /// The project state whose working folder this cache snapshot was taken with. None: an
    /// older save, or a merge state of cache overlays. Read by `merge --caches-only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_of: Option<String>,
    /// The root tree of the state in the project's object store (docs/objects.md, section 8), as
    /// 64 hex digits. New saves carry it after migration; a later `state.tree` record of
    /// `layr migrate` adds it after the tree was checked out and compared with the snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The times table that goes with `tree`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<String>,
}

pub const REF_PREFIXES: &[&str] = &["env/", "release/", "pins/", "holds/", "leases/", "caches/", "derived/"];

pub fn ref_namespace(name: &str) -> bool {
    REF_PREFIXES.iter().any(|p| name.starts_with(p))
}

pub const RESERVED_PREFIXES: &[&str] =
    &["env/", "release/", "pins/", "holds/", "leases/", "caches/", "derived/", "changes/", "views/", "attestations/"];

pub fn reserved_revision(name: &str) -> bool {
    RESERVED_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefState {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefChange {
    pub name: String,
    pub before: Option<RefState>,
    pub after: Option<RefState>,
}

/// An exact target head, resolved when an operation is prepared.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub struct HeadExpectation(String);

impl TryFrom<String> for HeadExpectation {
    type Error = &'static str;

    fn try_from(head: String) -> Result<Self, Self::Error> {
        if !crate::validation::is_uuid(&head) {
            return Err("expected head must be a full state UUID");
        }
        Ok(Self(head))
    }
}

impl From<HeadExpectation> for String {
    fn from(head: HeadExpectation) -> Self {
        head.0
    }
}

impl HeadExpectation {
    pub fn check(&self, found: &str) -> Result<(), HeadMismatch> {
        if self.0 == found {
            Ok(())
        } else {
            Err(HeadMismatch { expected: self.0.clone(), found: found.to_string() })
        }
    }
}

/// A target head mismatch is permanent for the prepared operation, unlike a retryable head race.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeadMismatch {
    pub expected: String,
    pub found: String,
}

impl std::fmt::Display for HeadMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "target head mismatch: expected {}, found {}", self.expected, self.found)
    }
}

impl std::error::Error for HeadMismatch {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntry {
    /// The target must still have this head when the runner takes its line lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<HeadExpectation>,
    /// A machine-readable failure, alongside the existing human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<HeadMismatch>,
    pub id: String,
    pub into: String,
    pub target_line: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub added_by: u32,
    pub time: i64,
    pub status: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probes: Option<serde_json::Value>,
    /// The conventional title of the entry: the queue merges it as one state with this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueChange {
    pub id: String,
    pub before: Option<QueueEntry>,
    pub after: Option<QueueEntry>,
}

pub const ATTESTATION_KINDS: &[&str] = &["check", "approval", "origin", "import", "quarantine", "revoke", "bisect"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attestation {
    pub format: u32,
    pub attestation_id: String,
    pub subject: Subject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub kind: String,
    pub payload_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_ref: Option<String>,
    pub signer: String,
    pub time: i64,
}

/// The pointers of a line at one time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineState {
    pub name: String,
    pub head: String,
    pub owner: u32,
    pub machine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub uid: u32,
    pub name: String,
    /// See `Author::session`.
    #[serde(default, alias = "agent", skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub machine: String,
    pub seq: u64,
    pub time: i64,
    pub op: String,
    pub project: String,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<StateRec>,
    /// Line id to its pointer change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lines: BTreeMap<String, LineChange>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// An operation being prepared, before the appender assigns its identity and sequence.
#[derive(Debug, Clone)]
#[must_use]
pub struct DraftRecord {
    pub op: String,
    pub states: Vec<StateRec>,
    pub lines: BTreeMap<String, LineChange>,
    pub data: serde_json::Value,
}

/// Identity assigned under the project lock. Zero is reserved for untrusted previews
/// and legacy machine bootstrap records read from the wire, never for a new append.
pub struct RecordIdentity {
    pub id: layr_types::RecordId,
    pub machine: layr_types::MachineId,
    pub project: layr_types::ProjectId,
    pub seq: std::num::NonZeroU64,
    pub time: i64,
    pub actor: Actor,
}

impl DraftRecord {
    pub fn new(op: &str) -> Self {
        Self { op: op.into(), states: Vec::new(), lines: BTreeMap::new(), data: serde_json::Value::Null }
    }

    pub fn sign(
        self,
        identity: RecordIdentity,
        key: &ed25519_dalek::SigningKey,
    ) -> anyhow::Result<layr_store::record::SignedRecord> {
        let record = Record {
            id: identity.id.to_string(),
            machine: identity.machine.to_string(),
            project: identity.project.to_string(),
            seq: identity.seq.get(),
            time: identity.time,
            actor: identity.actor,
            op: self.op,
            states: self.states,
            lines: self.lines,
            data: self.data,
        };
        crate::validation::validate(&record)?;
        let body = serde_json::to_string(&record)?;
        let signature = layr_store::keys::sign_bytes(body.as_bytes(), key);
        layr_store::record::parse_line(&format!("{{\"r\":{body},\"s\":\"{signature}\"}}"))
    }
}

impl Record {
    /// An explicitly untrusted preview/fixture, with no publishable identity. This
    /// never supplies signature or authorization proof and must not be appended.
    pub fn untrusted_preview(op: &str) -> Self {
        Self {
            id: String::new(),
            machine: String::new(),
            project: String::new(),
            seq: 0,
            time: 0,
            actor: Actor { uid: 0, name: String::new(), session: None },
            op: op.into(),
            states: Vec::new(),
            lines: BTreeMap::new(),
            data: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Line {
    pub id: String,
    pub name: String,
    pub head: String,
    pub owner: u32,
    pub machine: String,
}

/// Discussion attached to an immutable version of a logical change.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub id: String,
    pub change_id: String,
    /// Full state id, not a mutable revision or ordinal.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[cfg(test)]
mod head_expectation_tests {
    use super::*;

    const BEFORE: &str = "01a12290-0000-7000-8000-000000000001";
    const AFTER: &str = "01a12290-0000-7000-8000-000000000002";

    #[test]
    fn exact_head_check_returns_typed_mismatch() {
        let expected = HeadExpectation::try_from(BEFORE.to_string()).unwrap();
        assert!(expected.check(BEFORE).is_ok());
        let mismatch = expected.check(AFTER).unwrap_err();
        assert_eq!(mismatch, HeadMismatch { expected: BEFORE.into(), found: AFTER.into() });
        assert_eq!(mismatch.to_string(), format!("target head mismatch: expected {BEFORE}, found {AFTER}"));
        assert_eq!(serde_json::to_value(&expected).unwrap(), BEFORE);
        assert!(serde_json::from_str::<HeadExpectation>(r#""not-a-state""#).is_err());
    }

    #[test]
    fn legacy_queue_entries_omit_expectation_and_failure() {
        let value = serde_json::json!({
            "id": BEFORE, "into": "main", "target_line": BEFORE, "state": AFTER,
            "added_by": 1000, "time": 1, "status": "pending", "attempts": 0
        });
        let mut entry: QueueEntry = serde_json::from_value(value.clone()).unwrap();
        assert!(entry.expected_head.is_none());
        assert!(entry.failure.is_none());
        assert_eq!(serde_json::to_value(&entry).unwrap(), value);
        entry.expected_head = Some(HeadExpectation::try_from(BEFORE.to_string()).unwrap());
        entry.failure = Some(entry.expected_head.as_ref().unwrap().check(AFTER).unwrap_err());
        let encoded = serde_json::to_value(&entry).unwrap();
        assert_eq!(encoded["expected_head"], BEFORE);
        assert_eq!(encoded["failure"], serde_json::json!({"expected": BEFORE, "found": AFTER}));
        assert_eq!(serde_json::from_value::<QueueEntry>(encoded).unwrap(), entry);
    }
}
