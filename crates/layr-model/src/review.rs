//! Review of changes (docs/review.md, "Review threads, reactions and reviewers"): comment threads and their
//! resolution, reactions, and reviewer requests.
//!
//! Each piece is an append-only signed record. Machines that write at the same time never overwrite each
//! other, and this fold gives every machine the same answer from the same records:
//! - A thread status and a reviewer request are registers. Each record names the records of the same key
//!   that its writer saw (`prev`). The heads are the records that no other record names. One head, or heads
//!   that agree, give the value. Heads that disagree were written at the same time: the thread then stays
//!   open and the request stays active, so concurrent work never hides a thread or a reviewer. Clock skew
//!   between machines does not change the result, because `prev` and not the time orders the records.
//! - A reaction belongs to one writer: a person, or one user of one machine. The newest record of that
//!   writer for the target and the name decides. One machine writes its records in sequence.
//! - A comment is a thread when it replies to no comment. A reply belongs to the thread of the comment it
//!   replies to. An approval does not change a request record: the reviewer status is derived.

use crate::{validation::is_uuid, Project, Record};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const RESOLVE_OP: &str = "comment.resolve";
pub const UNRESOLVE_OP: &str = "comment.unresolve";
pub const REACT_OP: &str = "reaction.add";
pub const UNREACT_OP: &str = "reaction.remove";
pub const REQUEST_OP: &str = "review.request";
pub const CANCEL_OP: &str = "review.cancel";

/// The most records that one register record can name: more heads than this are not real concurrency.
const MAX_PREV: usize = 64;

// The payloads accept unknown fields: a later layr can add one, and an older machine still reads the record.

/// `data.thread_status` of `comment.resolve` and `comment.unresolve`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStatus {
    pub change_id: String,
    /// The id of the first comment of the thread.
    pub thread: String,
    pub resolved: bool,
    /// The status records of this thread that the writer saw.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prev: Vec<String>,
}

/// `data.reaction` of `reaction.add` and `reaction.remove`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub change_id: String,
    /// A comment id, or the change id for a reaction to the change.
    pub target: String,
    pub name: String,
    pub on: bool,
}

/// `data.review_request` of `review.request` and `review.cancel`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub change_id: String,
    /// `person:<login>`, `user:<uid>` or `machine:<machine id>`.
    pub reviewer: String,
    pub active: bool,
    /// The request records of this change and reviewer that the writer saw.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prev: Vec<String>,
}

/// A reaction name: 1 to 32 of `a-z`, `0-9`, `_`, `+` and `-`, for example `+1`, `heart` or `thumbs_up`.
pub fn reaction_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-'))
}

/// A reviewer principal: `person:<login>`, `user:<uid>` or `machine:<machine id>`.
pub fn reviewer(principal: &str) -> bool {
    if let Some(login) = principal.strip_prefix("person:") {
        return layr_store::person::login_name(login);
    }
    if let Some(uid) = principal.strip_prefix("user:") {
        return !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()) && uid.parse::<u32>().is_ok();
    }
    principal.strip_prefix("machine:").is_some_and(is_uuid)
}

fn check_prev(r: &Record, prev: &[String]) -> Result<()> {
    if prev.len() > MAX_PREV || prev.iter().any(|id| !is_uuid(id) || *id == r.id) {
        bail!("record {}: bad previous records of {}", r.seq, r.op);
    }
    Ok(())
}

/// The payload `key` of a record whose op is one of `ops`, else None. Another op's field of the same name is not
/// this payload: a later layr can use the name for something else.
fn payload<T: serde::de::DeserializeOwned>(r: &Record, ops: [&str; 2], key: &str) -> Option<serde_json::Result<T>> {
    ops.contains(&r.op.as_str()).then(|| r.data.get(key).map(|v| serde_json::from_value::<T>(v.clone())))?
}

/// The review fields of one record (called from `validation::validate`). Only the fields that the fold reads are
/// checked; a record of another op is not checked here.
pub fn validate(r: &Record) -> Result<()> {
    let uuid = |what: &str, v: &str| {
        if !is_uuid(v) {
            bail!("record {}: {what} is not an id: {v:?}", r.seq);
        }
        Ok(())
    };
    match r.op.as_str() {
        RESOLVE_OP | UNRESOLVE_OP => {
            let Some(Ok(s)) = payload::<ThreadStatus>(r, [RESOLVE_OP, UNRESOLVE_OP], "thread_status") else {
                bail!("record {}: {} needs a thread status", r.seq, r.op);
            };
            uuid("change", &s.change_id)?;
            uuid("thread", &s.thread)?;
            check_prev(r, &s.prev)?;
            if s.resolved != (r.op == RESOLVE_OP) {
                bail!("record {}: {} needs a thread status that matches it", r.seq, r.op);
            }
        }
        REACT_OP | UNREACT_OP => {
            let Some(Ok(x)) = payload::<Reaction>(r, [REACT_OP, UNREACT_OP], "reaction") else {
                bail!("record {}: {} needs a reaction", r.seq, r.op);
            };
            uuid("change", &x.change_id)?;
            uuid("reaction target", &x.target)?;
            if !reaction_name(&x.name) {
                bail!("record {}: bad reaction name {:?}: use 1 to 32 of a-z, 0-9, '_', '+' and '-'", r.seq, x.name);
            }
            if x.on != (r.op == REACT_OP) {
                bail!("record {}: {} needs a reaction that matches it", r.seq, r.op);
            }
        }
        REQUEST_OP | CANCEL_OP => {
            let Some(Ok(q)) = payload::<ReviewRequest>(r, [REQUEST_OP, CANCEL_OP], "review_request") else {
                bail!("record {}: {} needs a reviewer request", r.seq, r.op);
            };
            uuid("change", &q.change_id)?;
            if !reviewer(&q.reviewer) {
                bail!("record {}: bad reviewer {:?}: use person:<login>, user:<uid> or machine:<id>", r.seq, q.reviewer);
            }
            check_prev(r, &q.prev)?;
            if q.active != (r.op == REQUEST_OP) {
                bail!("record {}: {} needs a reviewer request that matches it", r.seq, r.op);
            }
        }
        "comment" => {
            if let Some(end) = r.data.get("line_end") {
                let end = end.as_u64().and_then(|n| u32::try_from(n).ok());
                let start = r.data["comment"]["line"].as_u64();
                if start.is_none() || end.is_none_or(|end| u64::from(end) < start.unwrap_or(0)) {
                    bail!("record {}: a comment line range needs a first line and an end line not before it", r.seq);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The thread status of a `comment.resolve` or `comment.unresolve` record.
fn thread_status_of(r: &Record) -> Option<ThreadStatus> {
    payload(r, [RESOLVE_OP, UNRESOLVE_OP], "thread_status")?.ok()
}

/// The reviewer request of a `review.request` or `review.cancel` record.
fn review_request_of(r: &Record) -> Option<ReviewRequest> {
    payload(r, [REQUEST_OP, CANCEL_OP], "review_request")?.ok()
}

/// The reaction of a `reaction.add` or `reaction.remove` record.
fn reaction_of(r: &Record) -> Option<Reaction> {
    payload(r, [REACT_OP, UNREACT_OP], "reaction")?.ok()
}

/// One record of a register.
struct Entry<'a> {
    record: &'a Record,
    prev: Vec<String>,
    value: bool,
}

/// The value of a register and the newest head record that gives it (see the module comment). None: no record.
fn register<'a>(entries: &[Entry<'a>], concurrent: bool) -> Option<(bool, &'a Record)> {
    let named: BTreeSet<&str> = entries.iter().flat_map(|e| e.prev.iter().map(String::as_str)).collect();
    let heads: Vec<&Entry> = entries.iter().filter(|e| !named.contains(e.record.id.as_str())).collect();
    let first = heads.first()?.value;
    let value = if heads.iter().all(|h| h.value == first) { first } else { concurrent };
    // Entries come in record order, so the last head with the value is the newest.
    heads.iter().rev().find(|h| h.value == value).map(|h| (value, h.record))
}

/// The writer of a reaction: a person whose key the project pins, else one user of one machine.
fn writer(project: &Project, r: &Record) -> String {
    if r.actor.name.starts_with("person:") && project.person_record_pinned(r) {
        r.actor.name.clone()
    } else {
        format!("user:{}@{}", r.actor.uid, r.machine)
    }
}

impl Project {
    /// The comment records of a change in record order, each with its comment as `comments` gives it.
    pub(crate) fn comment_records(&self, change: &str) -> Vec<(&Record, Value)> {
        self.records
            .iter()
            .filter_map(|record| {
                let mut comment = record.data.get("comment")?.clone();
                if comment["change_id"].as_str() != Some(change) {
                    return None;
                }
                comment["actor"] = serde_json::to_value(&record.actor).ok()?;
                comment["time"] = json!(record.time);
                if let Some(end) = record.data.get("line_end") {
                    comment["line_end"] = end.clone();
                }
                Some((record, comment))
            })
            .collect()
    }

    /// The comment whose id is `prefix`, or the one comment whose id starts with it (4 characters at least).
    pub fn find_comment(&self, prefix: &str) -> Result<Value> {
        let mut found: Vec<&Value> = Vec::new();
        for record in &self.records {
            let Some(comment) = record.data.get("comment") else { continue };
            let Some(id) = comment["id"].as_str() else { continue };
            if id == prefix {
                return Ok(comment.clone());
            }
            if prefix.len() >= 4 && id.starts_with(prefix) && !found.iter().any(|c| c["id"] == comment["id"]) {
                found.push(comment);
            }
        }
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("unknown comment '{prefix}'"),
            _ => bail!("ambiguous comment '{prefix}'"),
        }
    }

    /// The first comment of the thread of `comment` in its change. A reply to a comment that is not known
    /// (not synced yet) starts its own thread.
    pub fn thread_root(&self, change: &str, comment: &str) -> String {
        let parents: BTreeMap<String, Option<String>> = self
            .comment_records(change)
            .into_iter()
            .filter_map(|(_, c)| Some((c["id"].as_str()?.to_owned(), c["reply_to"].as_str().map(String::from))))
            .collect();
        Self::root_of(&parents, comment)
    }

    fn root_of(parents: &BTreeMap<String, Option<String>>, comment: &str) -> String {
        let mut id = comment.to_owned();
        let mut seen = BTreeSet::new();
        while let Some(Some(parent)) = parents.get(&id) {
            if !parents.contains_key(parent) || !seen.insert(id.clone()) {
                break;
            }
            id = parent.clone();
        }
        id
    }

    fn status_entries(&self, change: &str, thread: &str) -> Vec<Entry<'_>> {
        self.records
            .iter()
            .filter_map(|record| {
                let s = thread_status_of(record)?;
                (s.change_id == change && s.thread == thread).then_some(Entry { record, prev: s.prev, value: s.resolved })
            })
            .collect()
    }

    fn request_entries(&self, change: &str, reviewer: &str) -> Vec<Entry<'_>> {
        self.records
            .iter()
            .filter_map(|record| {
                let q = review_request_of(record)?;
                (q.change_id == change && q.reviewer == reviewer).then_some(Entry { record, prev: q.prev, value: q.active })
            })
            .collect()
    }

    fn heads(entries: &[Entry]) -> Vec<String> {
        let named: BTreeSet<&str> = entries.iter().flat_map(|e| e.prev.iter().map(String::as_str)).collect();
        entries.iter().map(|e| e.record.id.clone()).filter(|id| !named.contains(id.as_str())).collect()
    }

    /// Whether a thread is resolved, and the head status records that a new status record names as `prev`.
    pub fn thread_status(&self, change: &str, thread: &str) -> (bool, Vec<String>) {
        let entries = self.status_entries(change, thread);
        (register(&entries, false).is_some_and(|(v, _)| v), Self::heads(&entries))
    }

    /// Whether a reviewer request is active, and its head records that a new request record names as `prev`.
    pub fn review_request(&self, change: &str, reviewer: &str) -> (bool, Vec<String>) {
        let entries = self.request_entries(change, reviewer);
        (register(&entries, true).is_some_and(|(v, _)| v), Self::heads(&entries))
    }

    /// Reaction counts of a change: target (the change id or a comment id) to reaction name to count.
    pub fn reactions(&self, change: &str) -> BTreeMap<String, BTreeMap<String, u64>> {
        let mut latest: BTreeMap<(String, String, String), &Record> = BTreeMap::new();
        let mut on: BTreeMap<(String, String, String), bool> = BTreeMap::new();
        for record in &self.records {
            let Some(x) = reaction_of(record) else {
                continue;
            };
            if x.change_id != change {
                continue;
            }
            let key = (writer(self, record), x.target, x.name);
            // One machine writes in sequence: its sequence number orders its records, also when its clock went back.
            if latest.get(&key).is_some_and(|old| old.machine == record.machine && old.seq > record.seq) {
                continue;
            }
            latest.insert(key.clone(), record);
            on.insert(key, x.on);
        }
        let mut out: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        for ((_, target, name), on) in on {
            if on {
                *out.entry(target).or_default().entry(name).or_default() += 1;
            }
        }
        out
    }

    /// Whether `writer` (as `reaction_writer` names it) has reaction `name` on `target`.
    pub fn has_reaction(&self, change: &str, writer_key: &str, target: &str, name: &str) -> bool {
        let mut state: Option<(&Record, bool)> = None;
        for record in &self.records {
            let Some(x) = reaction_of(record) else {
                continue;
            };
            if x.change_id != change || x.target != target || x.name != name || writer(self, record) != writer_key {
                continue;
            }
            if state.is_some_and(|(old, _)| old.machine == record.machine && old.seq > record.seq) {
                continue;
            }
            state = Some((record, x.on));
        }
        state.is_some_and(|(_, on)| on)
    }

    /// The writer key of a reaction by a user of a machine (see `writer`).
    pub fn reaction_writer(uid: u32, machine: &str) -> String {
        format!("user:{uid}@{machine}")
    }

    /// Whether a record was written by a reviewer principal. A user is one of the perspective's own machines.
    fn written_by(&self, r: &Record, principal: &str) -> bool {
        let person = r.actor.name.starts_with("person:");
        if principal.starts_with("person:") {
            return r.actor.name == principal && self.person_record_pinned(r);
        }
        if let Some(uid) = principal.strip_prefix("user:").and_then(|u| u.parse::<u32>().ok()) {
            return !person && r.actor.uid == uid && self.admins.contains(&r.machine);
        }
        principal.strip_prefix("machine:").is_some_and(|m| !person && r.machine == m)
    }

    /// Whether a reviewer principal has an active approval of exactly this state.
    fn approved_by(&self, state: &str, principal: &str) -> bool {
        if let Some(login) = principal.strip_prefix("person:") {
            return self.person_approvals(state).iter().any(|(l, _)| l == login);
        }
        if let Some(uid) = principal.strip_prefix("user:").and_then(|u| u.parse::<u32>().ok()) {
            return self.approvals(state).iter().any(|(u, _)| *u == uid);
        }
        let Some(machine) = principal.strip_prefix("machine:") else { return false };
        self.attestations.iter().any(|(index, a)| {
            a.subject.id == state
                && a.kind == "approval"
                && a.signer == machine
                && !self.is_revoked(&a.attestation_id)
                && self.records[*index].data.get("declassify").is_none()
        })
    }

    /// The review part of `change show`: threads, reactions to the change, and reviewers.
    pub fn review(&self, change: &str) -> Value {
        let versions = self.change_versions(change);
        let label = |state: &str| versions.iter().position(|v| v.id == state).map(|i| format!("v{}", i + 1));
        let reactions = self.reactions(change);
        let comments = self.comment_records(change);
        let parents: BTreeMap<String, Option<String>> = comments
            .iter()
            .filter_map(|(_, c)| Some((c["id"].as_str()?.to_owned(), c["reply_to"].as_str().map(String::from))))
            .collect();
        let mut threads: Vec<Value> = Vec::new();
        let mut index: BTreeMap<String, usize> = BTreeMap::new();
        for (_, comment) in &comments {
            let Some(id) = comment["id"].as_str() else { continue };
            let mut comment = comment.clone();
            comment["reactions"] = json!(reactions.get(id).cloned().unwrap_or_default());
            let root = Self::root_of(&parents, id);
            if let Some(&i) = index.get(&root) {
                if let Some(replies) = threads[i]["replies"].as_array_mut() {
                    replies.push(comment);
                }
                continue;
            }
            let entries = self.status_entries(change, &root);
            let status = register(&entries, false).filter(|(v, _)| *v);
            index.insert(root.clone(), threads.len());
            threads.push(json!({
                "id": root,
                "version": comment["version"],
                "version_label": comment["version"].as_str().and_then(label),
                "path": comment.get("path").cloned().unwrap_or(Value::Null),
                "line": comment.get("line").cloned().unwrap_or(Value::Null),
                "line_end": comment.get("line_end").cloned().unwrap_or(Value::Null),
                "resolved": status.is_some(),
                "resolved_by": status.map(|(_, r)| json!(r.actor)).unwrap_or(Value::Null),
                "resolved_time": status.map(|(_, r)| json!(r.time)).unwrap_or(Value::Null),
                "comment": comment,
                "replies": [],
            }));
        }
        let mut reviewers: BTreeMap<String, Value> = BTreeMap::new();
        let names: BTreeSet<String> =
            self.records.iter().filter_map(review_request_of).filter(|q| q.change_id == change).map(|q| q.reviewer).collect();
        for name in names {
            let entries = self.request_entries(change, &name);
            let Some((true, by)) = register(&entries, true) else { continue };
            let approved = versions.iter().rposition(|v| self.approved_by(&v.id, &name));
            let commented = comments.iter().any(|(r, _)| self.written_by(r, &name));
            let status = match approved {
                Some(i) if i + 1 == versions.len() => "approved",
                Some(_) => "outdated",
                None if commented => "commented",
                None => "pending",
            };
            reviewers.insert(
                name.clone(),
                json!({
                    "reviewer": name,
                    "status": status,
                    "approved_version": approved.map(|i| format!("v{}", i + 1)),
                    "commented": commented,
                    "requested_by": by.actor,
                    "requested_time": by.time,
                }),
            );
        }
        json!({
            "threads": threads,
            "reactions": reactions.get(change).cloned().unwrap_or_default(),
            "reviewers": reviewers.into_values().collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{fold, record, state};
    use crate::StateKind;

    fn on(seq: u64, machine: &str, time: i64, op: &str, data: Value) -> Record {
        let mut r = record(seq, op);
        r.id = format!("{machine}-{seq}");
        r.machine = machine.into();
        r.time = time;
        r.data = data;
        r
    }
    fn versions() -> Vec<Record> {
        let mut first = record(1, "commit");
        first.states.push(state("s1", StateKind::Commit, &[]));
        let mut second = record(2, "commit");
        second.states.push(state("s2", StateKind::Commit, &["s1"]));
        vec![first, second]
    }
    fn comment(seq: u64, machine: &str, id: &str, reply_to: Option<&str>) -> Record {
        let mut c = json!({"id":id,"change_id":"change","version":"s1","text":format!("text {id}")});
        if let Some(to) = reply_to {
            c["reply_to"] = json!(to);
        }
        on(seq, machine, seq as i64, "comment", json!({"comment":c}))
    }
    fn status(seq: u64, machine: &str, time: i64, resolved: bool, prev: &[&str]) -> Record {
        let s = ThreadStatus {
            change_id: "change".into(),
            thread: "c1".into(),
            resolved,
            prev: prev.iter().map(|p| p.to_string()).collect(),
        };
        on(seq, machine, time, if resolved { RESOLVE_OP } else { UNRESOLVE_OP }, json!({"thread_status":s}))
    }
    fn request(seq: u64, machine: &str, time: i64, reviewer: &str, active: bool, prev: &[&str]) -> Record {
        let q = ReviewRequest {
            change_id: "change".into(),
            reviewer: reviewer.into(),
            active,
            prev: prev.iter().map(|p| p.to_string()).collect(),
        };
        on(seq, machine, time, if active { REQUEST_OP } else { CANCEL_OP }, json!({"review_request":q}))
    }
    fn react(seq: u64, machine: &str, time: i64, target: &str, name: &str, add: bool) -> Record {
        let x = Reaction { change_id: "change".into(), target: target.into(), name: name.into(), on: add };
        on(seq, machine, time, if add { REACT_OP } else { UNREACT_OP }, json!({"reaction":x}))
    }

    #[test]
    fn replies_join_the_thread_of_the_comment_they_answer() {
        let mut records = versions();
        records.push(comment(3, "a", "c1", None));
        records.push(comment(4, "b", "c2", Some("c1")));
        records.push(comment(5, "a", "c3", Some("c2")));
        records.push(comment(6, "a", "c4", None));
        // A reply to a comment that has not arrived yet starts its own thread.
        records.push(comment(7, "b", "c5", Some("missing")));
        let mut ranged = comment(8, "a", "c6", None);
        ranged.data["comment"]["path"] = json!("src/a.rs");
        ranged.data["comment"]["line"] = json!(3);
        ranged.data["line_end"] = json!(9);
        records.push(ranged);
        let p = fold(records);
        let threads = p.review("change")["threads"].clone();
        let ids: Vec<_> = threads.as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_owned()).collect();
        assert_eq!(ids, ["c1", "c4", "c5", "c6"]);
        let replies: Vec<_> = threads[0]["replies"].as_array().unwrap().iter().map(|c| c["id"].clone()).collect();
        assert_eq!(replies, [json!("c2"), json!("c3")]);
        assert_eq!(threads[0]["version_label"], "v1");
        assert_eq!((threads[3]["line"].clone(), threads[3]["line_end"].clone()), (json!(3), json!(9)));
        assert_eq!(p.comments("change")[5]["line_end"], 9);
        assert_eq!(p.thread_root("change", "c3"), "c1");
        assert!(threads.as_array().unwrap().iter().all(|t| t["resolved"] == false && t["resolved_by"].is_null()));
    }

    #[test]
    fn concurrent_resolve_and_unresolve_leave_the_thread_open_until_one_record_names_both() {
        let base = || {
            let mut r = versions();
            r.push(comment(3, "a", "c1", None));
            r.push(status(4, "a", 10, true, &[]));
            r
        };
        let mut records = base();
        // Both machines saw the first resolve; a reopened, b resolved again at the same time.
        records.push(status(5, "a", 20, false, &["a-4"]));
        records.push(status(1, "b", 30, true, &["a-4"]));
        let p = fold(records.clone());
        assert_eq!(p.thread_status("change", "c1"), (false, vec!["a-5".to_string(), "b-1".to_string()]));
        // A later record that saw both heads decides.
        records.push(status(6, "a", 40, true, &["a-5", "b-1"]));
        let p = fold(records);
        assert_eq!(p.thread_status("change", "c1"), (true, vec!["a-6".to_string()]));
        let thread = &p.review("change")["threads"][0];
        assert_eq!((thread["resolved"].clone(), thread["resolved_time"].clone()), (json!(true), json!(40)));
        // Clock skew: b reopened after it saw the resolve, but its clock is behind. `prev`, not the time, orders them.
        let mut skewed = base();
        skewed.push(status(1, "b", 5, false, &["a-4"]));
        assert_eq!(fold(skewed).thread_status("change", "c1").0, false);
    }

    #[test]
    fn concurrent_request_and_cancel_keep_the_request() {
        let mut records = versions();
        records.push(request(3, "a", 10, "user:20", true, &[]));
        records.push(request(1, "b", 20, "user:20", false, &["a-3"]));
        records.push(request(4, "a", 30, "user:20", true, &["a-3"]));
        let p = fold(records.clone());
        assert!(p.review_request("change", "user:20").0);
        records.push(request(5, "a", 40, "user:20", false, &["a-4", "b-1"]));
        let p = fold(records);
        assert!(!p.review_request("change", "user:20").0);
        assert_eq!(p.review("change")["reviewers"], json!([]));
    }

    #[test]
    fn a_writer_has_one_reaction_per_target_and_name() {
        let mut records = versions();
        records.push(comment(3, "a", "c1", None));
        records.push(react(4, "a", 10, "c1", "+1", true));
        records.push(react(5, "a", 11, "c1", "+1", true));
        records.push(react(1, "b", 12, "c1", "+1", true));
        records.push(react(2, "b", 13, "change", "heart", true));
        // Machine c removed its reaction later, but its clock went back: its sequence decides.
        records.push(react(1, "c", 14, "c1", "+1", true));
        records.push(react(2, "c", 3, "c1", "+1", false));
        let p = fold(records.clone());
        let counts = p.reactions("change");
        assert_eq!(counts["c1"]["+1"], 2);
        assert_eq!(counts["change"]["heart"], 1);
        let review = p.review("change");
        assert_eq!(review["reactions"], json!({"heart":1}));
        assert_eq!(review["threads"][0]["comment"]["reactions"], json!({"+1":2}));
        assert!(p.has_reaction("change", &Project::reaction_writer(10, "a"), "c1", "+1"));
        assert!(!p.has_reaction("change", &Project::reaction_writer(10, "c"), "c1", "+1"));
        records.push(react(6, "a", 20, "c1", "+1", false));
        assert_eq!(fold(records).reactions("change")["c1"]["+1"], 1);
    }

    #[test]
    fn reviewer_status_follows_approvals_and_comments() {
        let mut records = versions();
        // Uid 10 on the perspective machine `a` approved the newest version, uid 11 an older one.
        records.push(on(3, "a", 3, "approve", json!({"state":"s2"})));
        let mut older = on(4, "a", 4, "approve", json!({"state":"s1"}));
        older.actor.uid = 11;
        records.push(older);
        let mut talk = comment(5, "a", "c1", None);
        talk.actor.uid = 12;
        records.push(talk);
        // Machine b approved the newest version.
        records.push(on(1, "b", 6, "approve", json!({"state":"s2"})));
        for (seq, who) in
            [(6, "user:10"), (7, "user:11"), (8, "user:12"), (9, "user:13"), (10, "machine:b"), (11, "person:ann@example.com")]
        {
            records.push(request(seq, "a", 10 + seq as i64, who, true, &[]));
        }
        let p = fold(records);
        let rows = p.review("change")["reviewers"].clone();
        let status: BTreeMap<String, (String, Value)> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (r["reviewer"].as_str().unwrap().into(), (r["status"].as_str().unwrap().into(), r["approved_version"].clone()))
            })
            .collect();
        assert_eq!(status["user:10"], ("approved".into(), json!("v2")));
        assert_eq!(status["user:11"], ("outdated".into(), json!("v1")));
        assert_eq!(status["user:12"], ("commented".into(), Value::Null));
        assert_eq!(status["user:13"], ("pending".into(), Value::Null));
        assert_eq!(status["machine:b"], ("approved".into(), json!("v2")));
        // A person without a pinned key has no approvals and no comments.
        assert_eq!(status["person:ann@example.com"], ("pending".into(), Value::Null));
        assert_eq!(rows[0]["requested_by"]["uid"], 10);
    }

    #[test]
    fn validation_refuses_bad_review_records() {
        let id = |n: u8| format!("01900000-0000-7000-8000-0000000000{n:02}");
        let base = |op: &str, data: Value| {
            let mut r = Record::new(op);
            r.id = id(1);
            r.machine = id(2);
            r.seq = 1;
            r.data = data;
            r
        };
        let status = |resolved: bool, prev: Vec<String>| json!({"thread_status":{"change_id":id(3),"thread":id(4),"resolved":resolved,"prev":prev}});
        assert!(validate(&base(RESOLVE_OP, status(true, vec![id(5)]))).is_ok());
        assert!(validate(&base(RESOLVE_OP, status(false, vec![]))).is_err(), "the op and the value differ");
        assert!(validate(&base(RESOLVE_OP, status(true, vec![id(1)]))).is_err(), "a record cannot name itself");
        assert!(validate(&base(RESOLVE_OP, json!({}))).is_err(), "the op needs its data");
        let reaction = |name: &str| json!({"reaction":{"change_id":id(3),"target":id(4),"name":name,"on":true}});
        assert!(validate(&base(REACT_OP, reaction("+1"))).is_ok());
        assert!(validate(&base(REACT_OP, reaction("Thumbs Up"))).is_err());
        assert!(validate(&base(REACT_OP, reaction(""))).is_err());
        let request = |who: &str| json!({"review_request":{"change_id":id(3),"reviewer":who,"active":true}});
        for good in ["person:ann@example.com", "user:501", &format!("machine:{}", id(6))] {
            assert!(validate(&base(REQUEST_OP, request(good))).is_ok(), "{good}");
        }
        for bad in ["ann", "user:ann", "group:20", "machine:laptop", "*"] {
            assert!(validate(&base(REQUEST_OP, request(bad))).is_err(), "{bad}");
        }
        let comment = |line: Value, end: Value| json!({"comment":{"id":id(7),"change_id":id(3),"version":id(8),"path":"a.rs","line":line,"text":"x"},"line_end":end});
        assert!(validate(&base("comment", comment(json!(3), json!(5)))).is_ok());
        assert!(validate(&base("comment", comment(json!(3), json!(2)))).is_err());
        assert!(validate(&base("comment", comment(Value::Null, json!(2)))).is_err());
        // A later layr can add a field to a payload, or use a payload name in another op.
        let mut later = status(true, vec![]);
        later["thread_status"]["weight"] = json!(2);
        assert!(validate(&base(RESOLVE_OP, later.clone())).is_ok(), "an unknown field of a payload is accepted");
        assert_eq!(thread_status_of(&base(RESOLVE_OP, later.clone())).map(|s| s.resolved), Some(true));
        let mut later_reaction = reaction("+1");
        later_reaction["reaction"]["emoji"] = json!("x");
        assert!(validate(&base(REACT_OP, later_reaction)).is_ok());
        let mut later_request = request("user:501");
        later_request["review_request"]["due"] = json!(1);
        assert!(validate(&base(REQUEST_OP, later_request)).is_ok());
        let other = base("future.op", json!({"thread_status": "a string", "reaction": 1, "review_request": [], "line_end": "x"}));
        assert!(validate(&other).is_ok(), "another op's fields of the same names are not review payloads");
        assert!(thread_status_of(&other).is_none() && reaction_of(&other).is_none() && review_request_of(&other).is_none());
        assert!(thread_status_of(&base("future.op", later)).is_none(), "a payload counts only in its own op");
    }
}
