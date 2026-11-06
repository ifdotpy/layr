//! Limited machine trust (docs/ci-trust.md). A `machine.trust` record can end (`expires_at`, unix ms), give a role
//! (`reader` or `writer`; none is a full machine, as before) and name the overlays it covers. A `machine.untrust`
//! record ends a trust at once. A record of a limited machine outside its terms stays in the log (the sequence of
//! its machine stays whole) but has no effect anywhere: `refusal` says why.
//!
//! A limited machine never trusts, limits or untrusts another machine (`terms`). A machine always takes its own
//! records: the limits apply where other machines import them.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Makes no record that others take.
    Reader,
    /// A cache writer: cache states in the overlays it names and the save-back lines of the project.
    Writer,
    Full,
}

impl Role {
    pub fn parse(text: &str) -> Option<Role> {
        match text {
            "reader" => Some(Role::Reader),
            "writer" => Some(Role::Writer),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Writer => "writer",
            Role::Full => "full",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terms {
    pub expires_at: Option<i64>,
    pub role: Role,
    /// None: every overlay.
    pub overlays: Option<BTreeSet<String>>,
    pub untrusted_at: Option<i64>,
}

impl Terms {
    /// The limits of a `machine.trust` record's data; None for a full trust without an end.
    pub fn of_trust(data: &Value) -> Option<Terms> {
        let expires_at = data["expires_at"].as_i64();
        let role = data["role"].as_str().and_then(Role::parse).unwrap_or(Role::Full);
        let overlays =
            data["overlays"].as_array().map(|names| names.iter().filter_map(Value::as_str).map(str::to_owned).collect());
        (expires_at.is_some() || role != Role::Full || overlays.is_some()).then_some(Terms {
            expires_at,
            // A trust that ends or names overlays is a reader unless it says otherwise.
            role: if role == Role::Full { Role::Reader } else { role },
            overlays,
            untrusted_at: None,
        })
    }

    /// The machine may get new key slots and cache reader entries at `now`.
    pub fn live(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|end| now < end) && self.untrusted_at.is_none_or(|at| now < at)
    }

    /// The machine may read overlay `name`.
    pub fn reads(&self, name: &str) -> bool {
        self.overlays.as_ref().is_none_or(|names| names.contains(name))
    }

    /// Why a record of this machine, signed at `time`, has no effect; None when it counts. `overlay` is the overlay
    /// whose log holds the record (None: the project log). `own_lines` says that every line the record moves was
    /// made by a record of the same machine.
    pub fn refusal(&self, time: i64, op: &str, data: &Value, overlay: Option<&str>, own_lines: bool) -> Option<&'static str> {
        if self.untrusted_at.is_some_and(|at| time >= at) {
            return Some("signed after the machine was untrusted");
        }
        if self.expires_at.is_some_and(|end| time >= end) {
            return Some("signed after the trust of the machine ended");
        }
        if op == "machine" {
            return None;
        }
        match self.role {
            Role::Full => None,
            Role::Reader => Some("a reader machine makes no shared record"),
            Role::Writer => match overlay {
                // Machine-local settings (`config` of another machine is namespaced) and save-back lines.
                None if op == "config" => None,
                None if op == "line.create" && data["name"].as_str().is_some_and(is_save_back) && own_lines => None,
                None => Some("a cache writer makes only save-back lines in the project"),
                Some(name) if !self.reads(name) => Some("a cache writer writes only the overlays of its trust"),
                Some(_) if matches!(op, "line.create" | "commit" | "gc") && own_lines => None,
                Some(_) => Some("a cache writer saves only caches of its own lines"),
            },
        }
    }
}

/// A save-back line: `<line>.checks-<os>-<arch>`.
pub fn is_save_back(name: &str) -> bool {
    name.rsplit_once(".checks-").is_some_and(|(line, platform)| !line.is_empty() && platform.split('-').count() >= 2)
}

/// One record of a trust history (`machine.trust` or `machine.untrust`).
pub struct TrustRecord<'a> {
    pub signer: &'a str,
    pub time: i64,
    pub op: &'a str,
    pub data: &'a Value,
}

/// The terms of every limited or untrusted machine. Full machines grow from the project creator: a trust without
/// limits by a full machine makes a full machine, a trust with limits by a full machine limits its machine (and
/// it is not full, even when another full machine trusts it without limits). Several limits on one machine take
/// the narrowest: the first end, the smaller role, the common overlays. An untrust counts when its signer is full
/// and was not untrusted before. Records of other signers change nothing.
pub fn terms<'a>(creator: &str, records: impl IntoIterator<Item = TrustRecord<'a>>) -> BTreeMap<String, Terms> {
    let mut records: Vec<TrustRecord<'a>> = records.into_iter().collect();
    records.sort_by(|a, b| (a.time, a.signer).cmp(&(b.time, b.signer)));
    let trusts: Vec<&TrustRecord<'a>> = records.iter().filter(|r| r.op == "machine.trust").collect();
    let target = |r: &TrustRecord<'a>| r.data["machine"].as_str().filter(|m| *m != creator);
    // A machine that a full machine limits is never full. Each round finds more of them, so the rounds end.
    let mut never_full: BTreeSet<&str> = BTreeSet::new();
    let full = loop {
        let mut full: BTreeSet<&str> = BTreeSet::from([creator]);
        loop {
            let more: Vec<&str> = trusts
                .iter()
                .filter(|r| full.contains(r.signer) && Terms::of_trust(r.data).is_none())
                .filter_map(|r| target(r))
                .filter(|m| !never_full.contains(m) && !full.contains(m))
                .collect();
            if more.is_empty() {
                break;
            }
            full.extend(more);
        }
        let hits: Vec<&str> = trusts
            .iter()
            .filter(|r| full.contains(r.signer) && Terms::of_trust(r.data).is_some())
            .filter_map(|r| target(r))
            .filter(|m| full.contains(m))
            .collect();
        if hits.is_empty() {
            break full;
        }
        never_full.extend(hits);
    };
    let mut limited: BTreeMap<String, Terms> = BTreeMap::new();
    for record in trusts.iter().filter(|r| full.contains(r.signer)) {
        let (Some(machine), Some(terms)) = (target(record), Terms::of_trust(record.data)) else { continue };
        limited.entry(machine.to_string()).and_modify(|have| *have = have.narrowest(&terms)).or_insert(terms);
    }
    let mut untrusted: BTreeMap<String, i64> = BTreeMap::new();
    for record in records.iter().filter(|r| r.op == "machine.untrust" && full.contains(r.signer)) {
        let Some(machine) = target(record) else { continue };
        if untrusted.get(record.signer).is_some_and(|at| *at <= record.time) {
            continue;
        }
        untrusted.entry(machine.to_string()).or_insert(record.time);
    }
    for (machine, at) in untrusted {
        limited
            .entry(machine)
            .or_insert(Terms { expires_at: None, role: Role::Full, overlays: None, untrusted_at: None })
            .untrusted_at = Some(at);
    }
    limited
}

impl Terms {
    fn narrowest(&self, other: &Terms) -> Terms {
        Terms {
            expires_at: match (self.expires_at, other.expires_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            role: self.role.min(other.role),
            overlays: match (&self.overlays, &other.overlays) {
                (Some(a), Some(b)) => Some(a.intersection(b).cloned().collect()),
                (a, b) => a.clone().or(b.clone()),
            },
            untrusted_at: self.untrusted_at.or(other.untrusted_at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn trust(signer: &'static str, time: i64, data: Value) -> (&'static str, i64, &'static str, Value) {
        (signer, time, "machine.trust", data)
    }

    fn solve(records: &[(&'static str, i64, &'static str, Value)]) -> BTreeMap<String, Terms> {
        terms("root", records.iter().map(|(signer, time, op, data)| TrustRecord { signer, time: *time, op, data }))
    }

    #[test]
    fn a_limited_machine_limits_nobody() {
        let records = [
            trust("root", 10, json!({"machine": "job", "expires_at": 100, "role": "writer", "overlays": ["cache-target"]})),
            // A full trust of the same machine does not lift the limits.
            trust("root", 20, json!({"machine": "job"})),
            // The job trusts a machine, limits and untrusts the full ones; none of it counts, also backdated.
            trust("job", 30, json!({"machine": "evil"})),
            trust("job", 1, json!({"machine": "root", "expires_at": 2})),
            trust("job", 1, json!({"machine": "peer", "expires_at": 2})),
            ("job", 31, "machine.untrust", json!({"machine": "root"})),
            ("job", 1, "machine.untrust", json!({"machine": "peer"})),
            trust("root", 5, json!({"machine": "peer"})),
        ];
        let terms = solve(&records);
        let job = &terms["job"];
        assert_eq!((job.expires_at, job.role), (Some(100), Role::Writer));
        assert!(job.reads("cache-target") && !job.reads("cache-node"));
        assert_eq!(terms.keys().collect::<Vec<_>>(), ["job"]);
    }

    #[test]
    fn a_machine_that_a_limited_machine_trusted_is_limited_too_when_full_machines_say_so() {
        // peer is full and trusts b without limits; root limits b: b is limited, and b's trust of c counts for nothing.
        let records = [
            trust("root", 10, json!({"machine": "peer"})),
            trust("peer", 11, json!({"machine": "b"})),
            trust("root", 12, json!({"machine": "b", "expires_at": 50})),
            trust("b", 13, json!({"machine": "c"})),
        ];
        let terms = solve(&records);
        assert_eq!(terms["b"].expires_at, Some(50));
        assert!(!terms.contains_key("c") && !terms.contains_key("peer"));
    }

    #[test]
    fn an_untrust_ends_a_machine_and_its_later_untrusts() {
        let records = [
            trust("root", 10, json!({"machine": "a"})),
            trust("root", 10, json!({"machine": "b"})),
            ("root", 50, "machine.untrust", json!({"machine": "a"})),
            ("a", 40, "machine.untrust", json!({"machine": "b"})),
            ("a", 60, "machine.untrust", json!({"machine": "root"})),
        ];
        let terms = solve(&records);
        assert_eq!(terms["a"].untrusted_at, Some(50));
        assert_eq!(terms["a"].role, Role::Full);
        assert_eq!(terms["b"].untrusted_at, Some(40), "an untrust made before its signer's untrust stays");
        assert!(!terms.contains_key("root"), "the creator is never untrusted");
        assert!(terms["a"].live(49) && !terms["a"].live(50));
    }

    #[test]
    fn the_role_says_which_records_count() {
        let writer = Terms::of_trust(&json!({"expires_at": 100, "role": "writer", "overlays": ["cache-target"]})).unwrap();
        let reader = Terms::of_trust(&json!({"expires_at": 100})).unwrap();
        let back = json!({"name": "main.checks-linux-x86_64"});
        assert_eq!(reader.role, Role::Reader);
        assert!(reader.refusal(5, "machine", &json!({}), None, true).is_none());
        assert!(reader.refusal(5, "config", &json!({}), None, true).is_some());
        assert!(writer.refusal(5, "line.create", &back, None, true).is_none());
        assert!(writer.refusal(5, "line.create", &json!({"name": "main"}), None, true).is_some());
        assert!(writer.refusal(5, "commit", &json!({}), None, true).is_some(), "no source state");
        assert!(writer.refusal(5, "line.delete", &json!({}), None, true).is_some());
        assert!(writer.refusal(5, "machine.trust", &json!({}), None, true).is_some());
        assert!(writer.refusal(5, "commit", &json!({}), Some("cache-target"), true).is_none());
        assert!(writer.refusal(5, "commit", &json!({}), Some("cache-target"), false).is_some(), "another machine's line");
        assert!(writer.refusal(5, "commit", &json!({}), Some("cache-node"), true).is_some());
        assert!(writer.refusal(100, "commit", &json!({}), Some("cache-target"), true).is_some(), "after the end");
        assert!(Terms::of_trust(&json!({"machine": "x", "public_key": "00"})).is_none());
        assert!(is_save_back("main.checks-linux-x86_64") && !is_save_back("main") && !is_save_back(".checks-linux-x86_64"));
    }
}
