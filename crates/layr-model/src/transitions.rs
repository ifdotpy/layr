//! Domain transitions and subjects, converted at the unchanged wire boundary.
use crate::LineState;
use layr_types::{StateId, TreeId, ViewId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineTransition {
    Create {
        after: LineState,
    },
    /// Equal endpoints are valid: restore/undo can change only working content.
    Move {
        before: LineState,
        after: LineState,
    },
    /// Old tombstones can delete a line without knowing its previous pointer.
    Delete {
        before: Option<LineState>,
    },
}

impl LineTransition {
    pub fn from_endpoints(before: Option<LineState>, after: Option<LineState>) -> Self {
        match (before, after) {
            (None, Some(after)) => Self::Create { after },
            (Some(before), Some(after)) => Self::Move { before, after },
            (before, None) => Self::Delete { before },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(from = "LineChangeWire", into = "LineChangeWire")]
pub struct LineChange {
    pub transition: LineTransition,
    pub working_before: Option<String>,
    pub working_after: Option<String>,
}
impl LineChange {
    pub fn before(&self) -> Option<&LineState> {
        match &self.transition {
            LineTransition::Create { .. } => None,
            LineTransition::Move { before, .. } => Some(before),
            LineTransition::Delete { before } => before.as_ref(),
        }
    }
    pub fn after(&self) -> Option<&LineState> {
        match &self.transition {
            LineTransition::Create { after } | LineTransition::Move { after, .. } => Some(after),
            LineTransition::Delete { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineChangeWire {
    pub before: Option<LineState>,
    pub after: Option<LineState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_after: Option<String>,
}
impl From<LineChangeWire> for LineChange {
    fn from(w: LineChangeWire) -> Self {
        Self {
            transition: LineTransition::from_endpoints(w.before, w.after),
            working_before: w.working_before,
            working_after: w.working_after,
        }
    }
}
impl From<LineChange> for LineChangeWire {
    fn from(c: LineChange) -> Self {
        let (before, after) = match c.transition {
            LineTransition::Create { after } => (None, Some(after)),
            LineTransition::Move { before, after } => (Some(before), Some(after)),
            LineTransition::Delete { before } => (before, None),
        };
        Self { before, after, working_before: c.working_before, working_after: c.working_after }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "SubjectWire", into = "SubjectWire")]
pub enum Subject {
    State(StateId),
    Tree(TreeId),
    View(ViewId),
    Unknown(UnknownSubject),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSubject {
    kind: String,
    id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubjectWire {
    pub kind: String,
    pub id: String,
}
impl Subject {
    pub fn state(id: &str) -> anyhow::Result<Self> {
        Ok(Self::State(canonical_state_id(id)?))
    }
    pub fn view(id: &str) -> anyhow::Result<Self> {
        Ok(Self::View(ViewId::parse(id).map_err(anyhow::Error::msg)?))
    }
    pub fn kind(&self) -> &str {
        match self {
            Self::State(_) => "state",
            Self::Tree(_) => "tree",
            Self::View(_) => "view",
            Self::Unknown(s) => &s.kind,
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::State(id) => id.as_str(),
            Self::Tree(id) => id.as_str(),
            Self::View(id) => id.as_str(),
            Self::Unknown(s) => &s.id,
        }
    }
}
impl TryFrom<SubjectWire> for Subject {
    type Error = anyhow::Error;
    fn try_from(w: SubjectWire) -> Result<Self, Self::Error> {
        match w.kind.as_str() {
            "state" => Self::state(&w.id),
            "tree" => Ok(Self::Tree(TreeId::parse(&w.id).map_err(anyhow::Error::msg)?)),
            "view" => Self::view(&w.id),
            _ => Ok(Self::Unknown(UnknownSubject { kind: w.kind, id: w.id })),
        }
    }
}
impl From<Subject> for SubjectWire {
    fn from(s: Subject) -> Self {
        Self { kind: s.kind().into(), id: s.id().into() }
    }
}

fn canonical_state_id(id: &str) -> anyhow::Result<StateId> {
    if !crate::validation::is_uuid(id) {
        anyhow::bail!("state id must be a canonical UUID");
    }
    Ok(StateId::parse(id)?)
}

/// Conflict provenance names whole states, even when a file is absent on one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictSides {
    Unrecorded,
    Recorded { base: StateId, ours: StateId, theirs: StateId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictEntryKind {
    File,
    Directory,
    Symlink,
    Special,
}
impl ConflictEntryKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "file" => Some(Self::File),
            "directory" => Some(Self::Directory),
            "symlink" => Some(Self::Symlink),
            "special" => Some(Self::Special),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Special => "special",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    Content,
    AddAdd,
    ModifyDelete,
    DeleteModify,
    Binary,
    SymlinkTarget,
    Mode,
    RenameDelete,
    /// The wire label did not escape its destination separator. Retain the exact
    /// display description rather than claiming an unambiguous pair of paths.
    RenameRename {
        destinations: String,
    },
    TypeChange {
        ours: ConflictEntryKind,
        theirs: ConflictEntryKind,
    },
    OursDeletedDirectoryWithAddedFiles,
    OursDeletedDirectoryWithChangedFile,
    TheirsDeletedChangedDirectory,
    DeriveCycle,
    /// An overlay's patch and its parent overlay changed a file in a way that no text merge joins (the parent
    /// deleted or renamed it, or the file is not text): the overlay keeps its version (docs/overlays.md section 1).
    Parent,
    DeriveFailed {
        name: String,
    },
    BinaryTooLarge {
        limit_mib: u64,
    },
    BinaryOverlappingBlocks,
    UnknownMergeDriver {
        name: String,
    },
    DriverFailed {
        name: String,
    },
    UnknownVerifyCommand {
        name: String,
    },
    VerifyFailed {
        name: String,
    },
}
impl ConflictKind {
    fn label(&self) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow::{Borrowed, Owned};
        match self {
            Self::Content => Borrowed("content"),
            Self::Parent => Borrowed("parent"),
            Self::AddAdd => Borrowed("add/add"),
            Self::ModifyDelete => Borrowed("modify/delete"),
            Self::DeleteModify => Borrowed("delete/modify"),
            Self::Binary => Borrowed("binary"),
            Self::SymlinkTarget => Borrowed("symlink target"),
            Self::Mode => Borrowed("mode"),
            Self::RenameDelete => Borrowed("rename/delete"),
            Self::RenameRename { destinations } => Owned(format!("rename/rename ({destinations})")),
            Self::TypeChange { ours, theirs } => Owned(format!("type change ({} vs {})", ours.label(), theirs.label())),
            Self::OursDeletedDirectoryWithAddedFiles => Borrowed("directory deleted by ours, theirs added files"),
            Self::OursDeletedDirectoryWithChangedFile => Borrowed("directory deleted by ours, theirs changed or added a file"),
            Self::TheirsDeletedChangedDirectory => Borrowed("theirs deleted a directory that ours changed"),
            Self::DeriveCycle => Borrowed("derive dependency cycle"),
            Self::DeriveFailed { name } => Owned(format!("derive {name} failed")),
            Self::BinaryTooLarge { limit_mib } => Owned(format!("binary (larger than {limit_mib} MiB)")),
            Self::BinaryOverlappingBlocks => Borrowed("binary (overlapping blocks)"),
            Self::UnknownMergeDriver { name } => Owned(format!("unknown merge driver {name}")),
            Self::DriverFailed { name } => Owned(format!("driver {name}")),
            Self::UnknownVerifyCommand { name } => Owned(format!("unknown verify command {name}")),
            Self::VerifyFailed { name } => Owned(format!("verify {name} failed")),
        }
    }
}
impl std::fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label().as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictDetails {
    /// All current producer kinds carry the same whole-state provenance. Missing
    /// file sides are represented by the kind, not by removing their state IDs.
    Known {
        kind: ConflictKind,
        sides: ConflictSides,
    },
    Unknown(UnknownConflict),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownConflict {
    kind: String,
    sides: [Option<String>; 3],
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "ConflictWire", into = "ConflictWire")]
pub struct Conflict {
    pub path: String,
    pub details: ConflictDetails,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictWire {
    pub path: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}
impl Conflict {
    pub fn new(path: &str, kind: ConflictKind, sides: ConflictSides) -> Self {
        Self { path: path.into(), details: ConflictDetails::Known { kind, sides } }
    }
    pub fn uses_markers(&self) -> bool {
        matches!(self.details, ConflictDetails::Known { kind: ConflictKind::Content | ConflictKind::AddAdd, .. })
    }
    pub fn kind(&self) -> std::borrow::Cow<'_, str> {
        match &self.details {
            ConflictDetails::Known { kind, .. } => kind.label(),
            ConflictDetails::Unknown(c) => std::borrow::Cow::Borrowed(&c.kind),
        }
    }
    pub fn sides(&self) -> [Option<&str>; 3] {
        match &self.details {
            ConflictDetails::Known { sides: ConflictSides::Unrecorded, .. } => [None; 3],
            ConflictDetails::Known { sides: ConflictSides::Recorded { base, ours, theirs }, .. } => {
                [Some(base.as_str()), Some(ours.as_str()), Some(theirs.as_str())]
            }
            ConflictDetails::Unknown(c) => c.sides.each_ref().map(|id| id.as_deref()),
        }
    }
}
impl TryFrom<ConflictWire> for Conflict {
    type Error = anyhow::Error;
    fn try_from(w: ConflictWire) -> Result<Self, Self::Error> {
        use ConflictKind::*;
        let kind = match w.kind.as_str() {
            "content" => Some(Content),
            "add/add" => Some(AddAdd),
            "modify/delete" => Some(ModifyDelete),
            "delete/modify" => Some(DeleteModify),
            "binary" => Some(Binary),
            "symlink target" => Some(SymlinkTarget),
            "mode" => Some(Mode),
            "rename/delete" => Some(RenameDelete),
            "directory deleted by ours, theirs added files" => Some(OursDeletedDirectoryWithAddedFiles),
            "directory deleted by ours, theirs changed or added a file" => Some(OursDeletedDirectoryWithChangedFile),
            "theirs deleted a directory that ours changed" => Some(TheirsDeletedChangedDirectory),
            "derive dependency cycle" => Some(DeriveCycle),
            "parent" => Some(Parent),
            "binary (overlapping blocks)" => Some(BinaryOverlappingBlocks),
            label => {
                if let Some(destinations) = label.strip_prefix("rename/rename (").and_then(|s| s.strip_suffix(')')) {
                    Some(RenameRename { destinations: destinations.into() })
                } else if let Some(pair) = label.strip_prefix("type change (").and_then(|s| s.strip_suffix(')')) {
                    pair.split_once(" vs ").and_then(|(ours, theirs)| {
                        Some(TypeChange { ours: ConflictEntryKind::parse(ours)?, theirs: ConflictEntryKind::parse(theirs)? })
                    })
                } else if let Some(limit) = label.strip_prefix("binary (larger than ").and_then(|s| s.strip_suffix(" MiB)")) {
                    limit.parse::<u64>().ok().filter(|n| n.to_string() == limit).map(|limit_mib| BinaryTooLarge { limit_mib })
                } else if let Some(name) = label.strip_prefix("derive ").and_then(|s| s.strip_suffix(" failed")) {
                    Some(DeriveFailed { name: name.into() })
                } else if let Some(name) = label.strip_prefix("unknown merge driver ") {
                    Some(UnknownMergeDriver { name: name.into() })
                } else if let Some(name) = label.strip_prefix("driver ") {
                    Some(DriverFailed { name: name.into() })
                } else if let Some(name) = label.strip_prefix("unknown verify command ") {
                    Some(UnknownVerifyCommand { name: name.into() })
                } else {
                    label
                        .strip_prefix("verify ")
                        .and_then(|s| s.strip_suffix(" failed"))
                        .map(|name| VerifyFailed { name: name.into() })
                }
            }
        };
        let Some(kind) = kind else {
            return Ok(Self {
                path: w.path,
                details: ConflictDetails::Unknown(UnknownConflict { kind: w.kind, sides: [w.base, w.ours, w.theirs] }),
            });
        };
        let sides = match (&w.base, &w.ours, &w.theirs) {
            (None, None, None) => ConflictSides::Unrecorded,
            (Some(base), Some(ours), Some(theirs)) => ConflictSides::Recorded {
                base: canonical_state_id(base)?,
                ours: canonical_state_id(ours)?,
                theirs: canonical_state_id(theirs)?,
            },
            _ => anyhow::bail!("incomplete conflict state provenance"),
        };
        Ok(Self::new(&w.path, kind, sides))
    }
}
impl From<Conflict> for ConflictWire {
    fn from(c: Conflict) -> Self {
        let [base, ours, theirs] = c.sides().map(|id| id.map(str::to_owned));
        Self { kind: c.kind().into_owned(), path: c.path, base, ours, theirs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn endpoint() -> serde_json::Value {
        json!({"name":"main","head":"01a12290-0000-7000-8000-000000000001","owner":1000,"machine":"01a12290-0000-7000-8000-000000000002"})
    }

    #[test]
    fn transitions_keep_legacy_tombstones_equal_pointers_and_independent_captures() {
        for (before, after) in
            [(None, Some(endpoint())), (Some(endpoint()), Some(endpoint())), (Some(endpoint()), None), (None, None)]
        {
            let wire = json!({"before":before,"after":after,"working_after":"01a12290-0000-7000-8000-000000000003"});
            let change: LineChange = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&change).unwrap(), wire);
            match (&before, &after, &change.transition) {
                (None, Some(_), LineTransition::Create { .. })
                | (Some(_), Some(_), LineTransition::Move { .. })
                | (_, None, LineTransition::Delete { .. }) => {}
                _ => panic!("wire endpoints mapped to the wrong transition"),
            }
        }
    }

    #[test]
    fn subjects_parse_known_ids_and_preserve_future_subjects() {
        for (kind, id) in [
            ("state", "01a12290-0000-7000-8000-000000000001"),
            ("view", &"a".repeat(64)),
            ("tree", &"b".repeat(64)),
            ("future", "opaque"),
        ] {
            let wire = json!({"kind":kind,"id":id});
            let subject: Subject = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!((subject.kind(), subject.id()), (kind, id));
            assert_eq!(serde_json::to_value(subject).unwrap(), wire);
        }
        assert!(serde_json::from_value::<Subject>(json!({"kind":"state","id":"short"})).is_err());
        assert!(serde_json::from_value::<Subject>(json!({"kind":"view","id":"short"})).is_err());
    }

    #[test]
    fn conflict_conversion_checks_known_provenance_and_preserves_unknown_kinds() {
        let base = "01a12290-0000-7000-8000-000000000001";
        let ours = "01a12290-0000-7000-8000-000000000002";
        let theirs = "01a12290-0000-7000-8000-000000000003";
        for wire in [
            json!({"path":"file","kind":"content"}),
            json!({"path":"file","kind":"modify/delete","base":base,"ours":ours,"theirs":theirs}),
            json!({"path":"file","kind":"future","ours":ours}),
        ] {
            let conflict: Conflict = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(conflict).unwrap(), wire);
        }
        assert!(serde_json::from_value::<Conflict>(json!({"path":"file","kind":"content","base":base})).is_err());
        assert!(serde_json::from_value::<Conflict>(
            json!({"path":"file","kind":"content","base":"wrong","ours":ours,"theirs":theirs})
        )
        .is_err());
    }

    #[test]
    fn unknown_operations_keep_the_wire_and_signature_bytes() {
        let id = "01a12290-0000-7000-8000-000000000001";
        let body = serde_json::to_string(&json!({
            "id":id,"machine":id,"project":id,"seq":1,"time":1,"op":"future.line.capture",
            "actor":{"uid":1000,"name":"test"},"lines":{id:{"before":null,"after":null,"working_after":id}},
            "data":{"future":true},"future_signed_field":42
        }))
        .unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[17; 32]);
        let sig = layr_store::keys::sign_bytes(body.as_bytes(), &key);
        let line = format!("{{\"r\":{body},\"s\":\"{sig}\"}}");
        let signed = layr_store::record::parse_line(&line).unwrap().verify(&key.verifying_key()).unwrap();
        let record: crate::Record = serde_json::from_str(signed.get()).unwrap();
        crate::validation::validate(&record).unwrap();
        assert_eq!(record.op, "future.line.capture");
        assert!(matches!(record.lines[id].transition, LineTransition::Delete { before: None }));
        assert_eq!(signed.get(), body);
        assert_eq!(signed.line(), line);
    }
    #[test]
    fn current_conflict_labels_are_typed_and_keep_their_wire_spelling() {
        let id = "01a12290-0000-7000-8000-000000000001";
        let labels = [
            "content",
            "add/add",
            "modify/delete",
            "delete/modify",
            "binary",
            "symlink target",
            "mode",
            "rename/delete",
            "rename/rename (a vs b vs c)",
            "type change (file vs directory)",
            "type change (special vs symlink)",
            "directory deleted by ours, theirs added files",
            "directory deleted by ours, theirs changed or added a file",
            "theirs deleted a directory that ours changed",
            "derive dependency cycle",
            "derive build failed",
            "binary (larger than 64 MiB)",
            "binary (overlapping blocks)",
            "unknown merge driver custom",
            "driver custom",
            "unknown verify command check",
            "verify check failed",
        ];
        for kind in labels {
            let wire = json!({"path":"file","kind":kind,"base":id,"ours":id,"theirs":id});
            let conflict: Conflict = serde_json::from_value(wire.clone()).unwrap();
            assert!(matches!(conflict.details, ConflictDetails::Known { .. }), "{kind}");
            assert_eq!(serde_json::to_value(conflict).unwrap(), wire, "{kind}");
            assert!(serde_json::from_value::<Conflict>(json!({"path":"file","kind":kind,"base":id})).is_err(), "{kind}");
        }
        for kind in ["binary (larger than MiB)", "derive failed", "verify failed"] {
            let wire = json!({"path":"file","kind":kind});
            let conflict: Conflict = serde_json::from_value(wire.clone()).unwrap();
            assert!(matches!(conflict.details, ConflictDetails::Unknown(_)));
            assert_eq!(serde_json::to_value(conflict).unwrap(), wire);
        }
    }

    #[test]
    fn wire_ids_require_the_existing_canonical_uuid_form() {
        let good = "01a12290-0000-7000-8000-000000000001";
        for bad in
            ["01A12290000070008000000000000001", "01A12290-0000-7000-8000-000000000001", "01a12290000070008000000000000001"]
        {
            assert!(StateId::parse(bad).is_ok(), "historical display parsing stays permissive");
            assert!(serde_json::from_value::<Subject>(json!({"kind":"state","id":bad})).is_err());
            assert!(serde_json::from_value::<Conflict>(
                json!({"path":"file","kind":"rename/delete","base":bad,"ours":good,"theirs":good})
            )
            .is_err());
        }
    }
}
