//! Records of the operation log and the objects they describe.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Author {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub uid: u32,
    /// Agent label, for example "codex worker 3".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StateKind {
    /// A commit: a named state with a message.
    Commit,
    /// An automatic snapshot of a line's working content (an undo point).
    Auto,
    /// Saved work of `layr stash`.
    Stash,
    /// A state made from a git commit of the remote.
    Import,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRec {
    pub id: String,
    pub kind: StateKind,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// Taken while processes ran in the line: crash-consistent only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
    /// Snapshots of regenerable layers: layer path to snapshot name in `states/`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub layers: BTreeMap<String, String>,
}

/// The pointers of a line at one time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineState {
    pub name: String,
    pub head: String,
    pub owner: u32,
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineChange {
    pub before: Option<LineState>,
    pub after: Option<LineState>,
    /// Automatic states of the working content before and after the operation, when the
    /// operation changed the working content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub uid: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
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

impl Record {
    pub fn new(op: &str) -> Record {
        Record {
            id: String::new(),
            machine: String::new(),
            seq: 0,
            time: 0,
            op: op.to_string(),
            project: String::new(),
            actor: Actor { uid: 0, name: String::new(), agent: None },
            states: Vec::new(),
            lines: BTreeMap::new(),
            data: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Line {
    pub id: String,
    pub name: String,
    pub head: String,
    pub owner: u32,
    pub machine: String,
    pub group: Option<String>,
}
