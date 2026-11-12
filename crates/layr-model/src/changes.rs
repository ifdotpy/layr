//! Change metadata that records give beside the versions: the number (`#12`), the title and
//! description, and the closed state (docs/review.md, "Change numbers, titles and closing").
//!
//! Every rule here must give the same result on every machine for the same set of records, in
//! any arrival order:
//! - Title, description, close and reopen: the last record in the model order (time, machine,
//!   sequence) wins. Title and description are separate fields: an edit changes only the fields
//!   it names.
//! - Numbers: one machine at a time gives numbers, the numbering machine. It is the project
//!   creator until that machine hands the role to another machine. The numbering machine writes
//!   its numbers in its own sequence, so they never collide, and the first number of a change
//!   stays its number.
use crate::{Actor, Record};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Sets the title or the description of a change: `{"change_id", "title"?, "description"?}`.
/// A field that is absent stays as it is; null or an empty string clears it.
pub const EDIT_OP: &str = "change.edit";
/// Closes a change: `{"change_id", "reason"?}`.
pub const CLOSE_OP: &str = "change.close";
/// Opens a closed change again: `{"change_id"}`.
pub const REOPEN_OP: &str = "change.reopen";
/// Numbers of the numbering machine: `{"numbers": {"<change id>": <number>}}`.
pub const NUMBER_OP: &str = "change.number";
/// The numbering machine gives its role to another machine: `{"machine", "next"}`. The new
/// machine gives numbers from `next` on; the old machine's numbers after this record count no more.
pub const HANDOFF_OP: &str = "change.number.authority";

/// Longest title in characters.
pub const MAX_TITLE: usize = 300;
/// Longest description in bytes.
pub const MAX_DESCRIPTION: usize = 64 << 10;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Closed>,
}

/// Who closed a change, when and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub actor: Actor,
    pub machine: String,
    pub time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Closed,
    Merged,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Closed => "closed",
            Status::Merged => "merged",
        }
    }
}

/// Apply one record to the metadata. Call it in model order: the last record wins.
pub fn apply(meta: &mut BTreeMap<String, Meta>, r: &Record) {
    if !matches!(r.op.as_str(), EDIT_OP | CLOSE_OP | REOPEN_OP) {
        return;
    }
    let Some(change) = r.data["change_id"].as_str().filter(|id| !id.is_empty()) else { return };
    let m = meta.entry(change.to_string()).or_default();
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
    match r.op.as_str() {
        EDIT_OP => {
            if let Some(v) = r.data.get("title") {
                m.title = text(v);
            }
            if let Some(v) = r.data.get("description") {
                m.description = text(v);
            }
        }
        CLOSE_OP => {
            m.closed =
                Some(Closed { actor: r.actor.clone(), machine: r.machine.clone(), time: r.time, reason: text(&r.data["reason"]) })
        }
        _ => m.closed = None,
    }
}

/// The numbers of the changes and the current numbering machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Numbering {
    /// The machine that gives the next numbers. None when the project has no single creator.
    pub authority: Option<String>,
    /// The next free number of the numbering machine.
    pub next: u64,
    pub numbers: BTreeMap<String, u64>,
    pub changes: BTreeMap<u64, String>,
}

impl Numbering {
    /// Fold the number records. `records` holds every record of the project.
    pub fn from_records(project: &str, records: &[Record]) -> Numbering {
        let mut n = Numbering { next: 1, ..Default::default() };
        let own_records = records.iter().filter(|r| r.project == project);
        let Ok(mut machine) = crate::project_perspective(project, own_records) else { return n };
        // A segment: the numbers of one machine in [low, high), from its records with a sequence
        // in (after, until). A machine that gets the role back starts after its last handoff.
        let mut low = 1u64;
        let mut last_handoff = BTreeMap::<String, u64>::new();
        for _ in 0..=records.len() {
            let after = last_handoff.get(&machine).copied().unwrap_or(0);
            let mut own: Vec<&Record> = records
                .iter()
                .filter(|r| r.machine == machine && r.seq > after && matches!(r.op.as_str(), NUMBER_OP | HANDOFF_OP))
                .collect();
            own.sort_by_key(|r| r.seq);
            let handoff = own.iter().find(|r| {
                r.op == HANDOFF_OP
                    && r.data["machine"].as_str().is_some_and(|m| !m.is_empty())
                    && r.data["next"].as_u64().is_some_and(|next| next >= low)
            });
            let until = handoff.map_or(u64::MAX, |r| r.seq);
            let high = handoff.and_then(|r| r.data["next"].as_u64()).unwrap_or(u64::MAX);
            for r in own.iter().filter(|r| r.op == NUMBER_OP && r.seq < until) {
                let Some(numbers) = r.data["numbers"].as_object() else { continue };
                let mut pairs: Vec<(u64, &String)> =
                    numbers.iter().filter_map(|(change, v)| v.as_u64().map(|v| (v, change))).collect();
                pairs.sort();
                for (number, change) in pairs {
                    if number < low || number >= high || change.is_empty() {
                        continue;
                    }
                    if n.numbers.contains_key(change) || n.changes.contains_key(&number) {
                        continue;
                    }
                    n.numbers.insert(change.clone(), number);
                    n.changes.insert(number, change.clone());
                }
            }
            match handoff {
                Some(h) => {
                    last_handoff.insert(machine.clone(), h.seq);
                    machine = h.data["machine"].as_str().unwrap_or_default().to_string();
                    low = high;
                }
                None => {
                    n.next = low.max(n.changes.keys().next_back().map_or(1, |max| max + 1));
                    n.authority = Some(machine);
                    break;
                }
            }
        }
        n
    }

    pub fn number(&self, change: &str) -> Option<u64> {
        self.numbers.get(change).copied()
    }
    pub fn change(&self, number: u64) -> Option<&str> {
        self.changes.get(&number).map(String::as_str)
    }
}

/// One change as tools show it: `change list --json` and the change view of the web UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub change_id: String,
    /// Null until the numbering machine has given a number.
    pub number: Option<u64>,
    /// The title set with `change edit`, else the first line of the newest version's message.
    pub title: String,
    /// The description set with `change edit`, else the rest of that message (null when empty).
    pub description: Option<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Closed>,
    pub versions: usize,
    /// The newest version.
    pub state: String,
    pub message: String,
}

/// The title and description that a change shows: the edited fields, else its newest message.
pub fn text(meta: Option<&Meta>, message: &str) -> (String, Option<String>) {
    let mut lines = message.splitn(2, '\n');
    let first = lines.next().unwrap_or("").trim().to_string();
    let rest = lines.next().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let title = meta.and_then(|m| m.title.clone()).unwrap_or(first);
    let description = match meta.and_then(|m| m.description.clone()) {
        Some(d) => Some(d),
        None => rest,
    };
    (title, description)
}

/// Add the fields of a view (`number`, `title`, `description`, `status`, `closed`) to the JSON
/// object of `change show`.
pub fn add_view_fields(out: &mut Value, view: &View) {
    out["number"] = serde_json::json!(view.number);
    out["title"] = serde_json::json!(view.title);
    out["description"] = serde_json::json!(view.description);
    out["status"] = serde_json::json!(view.status);
    if let Some(closed) = &view.closed {
        out["closed"] = serde_json::json!(closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{record, state};
    use crate::{LineChange, LineState, Project, StateKind};
    use serde_json::json;

    fn at(machine: &str, seq: u64, time: i64, op: &str, data: Value) -> Record {
        let mut r = record(seq, op);
        r.id = format!("{machine}-{seq}");
        r.machine = machine.into();
        r.time = time;
        r.data = data;
        r
    }
    fn creator() -> Record {
        at("a", 1, 1, "project", json!({"id":"project","name":"p"}))
    }
    fn commit(machine: &str, seq: u64, change: &str) -> Record {
        let mut r = at(machine, seq, seq as i64, "commit", Value::Null);
        let mut s = state(&format!("s-{change}"), StateKind::Commit, &[]);
        s.change_id = Some(change.into());
        s.message = format!("{change} message\n\nbody of {change}");
        r.states.push(s);
        r
    }
    /// Fold the records in the given order and in the reverse order: both must agree.
    fn both_orders(records: Vec<Record>) -> Project {
        let one = Project::from_records("project", records.clone(), "a").unwrap();
        let other = Project::from_records("project", records.into_iter().rev().collect::<Vec<_>>(), "b").unwrap();
        assert_eq!(one.change_views(), other.change_views(), "arrival order or perspective changed the views");
        assert_eq!(one.numbering(), other.numbering());
        one
    }

    #[test]
    fn concurrent_edits_take_the_last_record_per_field() {
        let records = vec![
            creator(),
            commit("a", 2, "c1"),
            at("a", 3, 5, EDIT_OP, json!({"change_id":"c1","title":"title of a","description":"text of a"})),
            at("b", 1, 7, EDIT_OP, json!({"change_id":"c1","title":"title of b"})),
        ];
        let view = both_orders(records).change_view("c1").unwrap();
        assert_eq!(view.title, "title of b");
        assert_eq!(view.description.as_deref(), Some("text of a"), "an edit of the title keeps the description");
    }

    #[test]
    fn an_equal_time_falls_back_to_machine_order_and_unset_fields_use_the_message() {
        let records = vec![
            creator(),
            commit("a", 2, "c1"),
            at("b", 1, 5, EDIT_OP, json!({"change_id":"c1","title":"title of b"})),
            at("a", 3, 5, EDIT_OP, json!({"change_id":"c1","title":"title of a"})),
        ];
        let p = both_orders(records.clone());
        assert_eq!(p.change_view("c1").unwrap().title, "title of b", "machine b sorts after machine a at the same time");
        let mut cleared = records;
        cleared.push(at("a", 4, 9, EDIT_OP, json!({"change_id":"c1","title":""})));
        let view = both_orders(cleared).change_view("c1").unwrap();
        assert_eq!(view.title, "c1 message");
        assert_eq!(view.description.as_deref(), Some("body of c1"));
    }

    #[test]
    fn close_and_reopen_on_two_machines_take_the_later_record() {
        let base = vec![creator(), commit("a", 2, "c1")];
        let mut closed_last = base.clone();
        closed_last.push(at("b", 1, 6, REOPEN_OP, json!({"change_id":"c1"})));
        closed_last.push(at("a", 3, 8, CLOSE_OP, json!({"change_id":"c1","reason":"not needed"})));
        let view = both_orders(closed_last).change_view("c1").unwrap();
        assert_eq!(view.status, Status::Closed);
        assert_eq!(view.closed.as_ref().and_then(|c| c.reason.as_deref()), Some("not needed"));

        let mut reopened_last = base;
        reopened_last.push(at("a", 3, 6, CLOSE_OP, json!({"change_id":"c1"})));
        reopened_last.push(at("b", 1, 8, REOPEN_OP, json!({"change_id":"c1"})));
        let p = both_orders(reopened_last);
        assert_eq!(p.change_view("c1").unwrap().status, Status::Open);
        assert_eq!(p.open_changes().len(), 1);
    }

    #[test]
    fn a_merged_change_stays_merged_when_a_close_arrives() {
        let mut main = commit("a", 2, "c1");
        main.lines.insert(
            "line".into(),
            LineChange {
                before: None,
                after: Some(LineState { name: "main".into(), head: "s-c1".into(), owner: 10, machine: "a".into() }),
                working_before: None,
                working_after: None,
            },
        );
        let p = both_orders(vec![creator(), main, at("b", 1, 9, CLOSE_OP, json!({"change_id":"c1"}))]);
        let view = p.change_view("c1").unwrap();
        assert_eq!(view.status, Status::Merged);
        assert!(view.closed.is_none());
        assert!(p.open_changes().is_empty());
    }

    #[test]
    fn a_closed_change_leaves_the_open_changes() {
        let p = both_orders(vec![
            creator(),
            commit("a", 2, "c1"),
            commit("a", 3, "c2"),
            at("b", 1, 9, CLOSE_OP, json!({"change_id":"c2"})),
        ]);
        let open: Vec<_> = p.open_changes().into_iter().filter_map(|s| s.change_id).collect();
        assert_eq!(open, ["c1"]);
        assert_eq!(p.change_views().iter().map(|v| v.status).collect::<Vec<_>>(), [Status::Open, Status::Closed]);
    }

    #[test]
    fn only_the_numbering_machine_gives_numbers_and_a_number_never_changes() {
        let records = vec![
            creator(),
            commit("a", 2, "c1"),
            commit("b", 1, "c2"),
            commit("b", 2, "c3"),
            // Machine b is not the numbering machine: its numbers count for nothing.
            at("b", 3, 3, NUMBER_OP, json!({"numbers":{"c3":1,"c2":7}})),
            at("a", 3, 4, NUMBER_OP, json!({"numbers":{"c1":1,"c2":2}})),
            // A second number of a change, and a number that another change has, are ignored.
            at("a", 4, 5, NUMBER_OP, json!({"numbers":{"c1":3,"c3":2}})),
            at("a", 5, 6, NUMBER_OP, json!({"numbers":{"c3":3}})),
        ];
        let p = both_orders(records);
        let n = p.numbering();
        assert_eq!(n.authority.as_deref(), Some("a"));
        assert_eq!((n.number("c1"), n.number("c2"), n.number("c3")), (Some(1), Some(2), Some(3)));
        assert_eq!(n.change(2), Some("c2"));
        assert_eq!(n.next, 4);
        assert_eq!(p.change_view("c3").unwrap().number, Some(3));
    }

    #[test]
    fn a_handoff_moves_numbering_without_a_collision() {
        let records = vec![
            creator(),
            commit("a", 2, "c1"),
            commit("a", 3, "c2"),
            commit("b", 1, "c3"),
            commit("b", 2, "c4"),
            at("a", 4, 4, NUMBER_OP, json!({"numbers":{"c1":1,"c2":2}})),
            at("a", 5, 5, HANDOFF_OP, json!({"machine":"b","next":3})),
            // Machine a writes after its handoff (it had not seen it yet): no effect.
            at("a", 6, 20, NUMBER_OP, json!({"numbers":{"c3":3}})),
            // Machine b gives numbers from 3 on; a lower number is not in its range.
            at("b", 3, 6, NUMBER_OP, json!({"numbers":{"c4":3,"c3":2}})),
            at("b", 4, 7, NUMBER_OP, json!({"numbers":{"c3":4}})),
        ];
        let p = both_orders(records.clone());
        let n = p.numbering();
        assert_eq!(n.authority.as_deref(), Some("b"));
        assert_eq!((n.number("c1"), n.number("c2"), n.number("c3"), n.number("c4")), (Some(1), Some(2), Some(4), Some(3)));
        assert_eq!(n.next, 5);
        // The role goes back to a: its old records stay in its old range, its new ones start at b's next.
        let mut back = records;
        back.push(commit("a", 7, "c5"));
        back.push(at("b", 5, 8, HANDOFF_OP, json!({"machine":"a","next":5})));
        back.push(at("a", 8, 21, NUMBER_OP, json!({"numbers":{"c5":5}})));
        let n = both_orders(back).numbering().clone();
        assert_eq!(n.authority.as_deref(), Some("a"));
        assert_eq!(n.number("c5"), Some(5));
        assert_eq!(n.number("c3"), Some(4), "machine a's stale record after its first handoff still has no effect");
        assert_eq!(n.next, 6);
    }

    #[test]
    fn a_project_without_one_creator_has_no_numbering_machine() {
        let p = both_orders(vec![commit("a", 2, "c1"), at("a", 3, 3, NUMBER_OP, json!({"numbers":{"c1":1}}))]);
        assert_eq!(p.numbering().authority, None);
        assert_eq!(p.change_view("c1").unwrap().number, None);
    }
}
