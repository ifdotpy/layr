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

/// One record of a trust history (`machine.trust`, `machine.untrust` or `trust.delegate`).
pub struct TrustRecord<'a> {
    pub signer: &'a str,
    pub time: i64,
    pub op: &'a str,
    pub data: &'a Value,
}

/// A broker's permission to trust other machines within limits (a `trust.delegate` record of a full machine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delegation {
    pub time: i64,
    pub max_hours: i64,
    pub role: Role,
    /// None: every overlay.
    pub overlays: Option<BTreeSet<String>>,
    pub name_prefix: Option<String>,
    /// A revoke: no delegation from its time on.
    pub revoked: bool,
}

impl Delegation {
    fn of(time: i64, data: &Value) -> Delegation {
        Delegation {
            time,
            max_hours: data["max_hours"].as_i64().unwrap_or(0),
            role: data["role"].as_str().and_then(Role::parse).unwrap_or(Role::Reader),
            overlays: data["overlays"]
                .as_array()
                .map(|names| names.iter().filter_map(Value::as_str).map(str::to_owned).collect()),
            name_prefix: data["name_prefix"].as_str().map(str::to_owned),
            revoked: data["revoked"].as_bool() == Some(true),
        }
    }

    /// A trust record's data made at `time` is inside this delegation: it ends at most `max_hours` later, with
    /// the same or a smaller role, overlays among the delegation's, and a machine name with the prefix.
    fn covers(&self, time: i64, data: &Value) -> bool {
        let Some(terms) = Terms::of_trust(data) else { return false };
        let ends = terms.expires_at.is_some_and(|end| end > time && end - time <= self.max_hours.saturating_mul(3_600_000));
        let overlays = match (&self.overlays, &terms.overlays) {
            (None, _) => true,
            (Some(allowed), Some(asked)) => asked.is_subset(allowed),
            (Some(_), None) => false,
        };
        let name =
            self.name_prefix.as_ref().is_none_or(|prefix| data["name"].as_str().is_some_and(|n| n.starts_with(prefix.as_str())));
        !self.revoked && ends && terms.role <= self.role && overlays && name
    }
}

/// The trust rules of a project: the limited machines and the brokers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    pub limited: BTreeMap<String, Terms>,
    /// The delegations of each broker, oldest first.
    pub brokers: BTreeMap<String, Vec<Delegation>>,
}

impl Rules {
    pub fn get(&self, machine: &str) -> Option<&Terms> {
        self.limited.get(machine)
    }

    pub fn contains_key(&self, machine: &str) -> bool {
        self.limited.contains_key(machine)
    }

    pub fn is_empty(&self) -> bool {
        self.limited.is_empty() && self.brokers.is_empty()
    }

    /// A `machine.trust` record of `signer` at `time` counts: the signer is no broker, or the trust is inside the
    /// broker's delegation of that time (the newest one made before it).
    pub fn trust_counts(&self, signer: &str, time: i64, data: &Value) -> bool {
        match self.brokers.get(signer) {
            None => true,
            Some(delegations) => delegations.iter().rev().find(|d| d.time <= time).is_some_and(|d| d.covers(time, data)),
        }
    }
}

/// The trust rules. Full machines grow from the project creator: a trust without limits by a full machine makes
/// a full machine, a trust with limits by a full machine limits its machine (and it is not full, even when another
/// full machine trusts it without limits). A machine that a full machine delegates to is a broker: its trusts count
/// only inside its delegation, so it never makes a full machine. Several limits on one machine take the narrowest:
/// the first end, the smaller role, the common overlays. An untrust counts when its signer is full and was not
/// untrusted before. Records of other signers change nothing.
pub fn rules<'a>(creator: &str, records: impl IntoIterator<Item = TrustRecord<'a>>) -> Rules {
    let mut records: Vec<TrustRecord<'a>> = records.into_iter().collect();
    records.sort_by(|a, b| (a.time, a.signer).cmp(&(b.time, b.signer)));
    let trusts: Vec<&TrustRecord<'a>> = records.iter().filter(|r| r.op == "machine.trust").collect();
    let delegations: Vec<&TrustRecord<'a>> = records.iter().filter(|r| r.op == "trust.delegate").collect();
    let target = |r: &TrustRecord<'a>| r.data["machine"].as_str().filter(|m| *m != creator);
    let brokers_of = |full: &BTreeSet<&str>| {
        let mut brokers: BTreeMap<String, Vec<Delegation>> = BTreeMap::new();
        for record in delegations.iter().filter(|r| full.contains(r.signer)) {
            if let Some(broker) = target(record) {
                brokers.entry(broker.to_string()).or_default().push(Delegation::of(record.time, record.data));
            }
        }
        Rules { limited: BTreeMap::new(), brokers }
    };
    // A machine that a full machine limits is never full. Each round finds more of them, so the rounds end.
    let mut never_full: BTreeSet<&str> = BTreeSet::new();
    let (full, mut rules) = loop {
        let mut full: BTreeSet<&str> = BTreeSet::from([creator]);
        loop {
            let brokers = brokers_of(&full);
            let more: Vec<&str> = trusts
                .iter()
                .filter(|r| {
                    full.contains(r.signer) && !brokers.brokers.contains_key(r.signer) && Terms::of_trust(r.data).is_none()
                })
                .filter_map(|r| target(r))
                .filter(|m| !never_full.contains(m) && !full.contains(m))
                .collect();
            if more.is_empty() {
                break;
            }
            full.extend(more);
        }
        let rules = brokers_of(&full);
        let hits: Vec<&str> = trusts
            .iter()
            .filter(|r| {
                full.contains(r.signer) && rules.trust_counts(r.signer, r.time, r.data) && Terms::of_trust(r.data).is_some()
            })
            .filter_map(|r| target(r))
            .filter(|m| full.contains(m))
            .collect();
        if hits.is_empty() {
            break (full, rules);
        }
        never_full.extend(hits);
    };
    let mut limited: BTreeMap<String, Terms> = BTreeMap::new();
    for record in trusts.iter().filter(|r| full.contains(r.signer) && rules.trust_counts(r.signer, r.time, r.data)) {
        let (Some(machine), Some(terms)) = (target(record), Terms::of_trust(record.data)) else { continue };
        limited.entry(machine.to_string()).and_modify(|have| *have = have.narrowest(&terms)).or_insert(terms);
    }
    rules.limited = limited;
    let mut untrusted: BTreeMap<String, i64> = BTreeMap::new();
    for record in records.iter().filter(|r| r.op == "machine.untrust" && full.contains(r.signer)) {
        let Some(machine) = target(record) else { continue };
        if untrusted.get(record.signer).is_some_and(|at| *at <= record.time) {
            continue;
        }
        untrusted.entry(machine.to_string()).or_insert(record.time);
    }
    for (machine, at) in untrusted {
        rules
            .limited
            .entry(machine)
            .or_insert(Terms { expires_at: None, role: Role::Full, overlays: None, untrusted_at: None })
            .untrusted_at = Some(at);
    }
    rules
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
        rules("root", records.iter().map(|(signer, time, op, data)| TrustRecord { signer, time: *time, op, data })).limited
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
    fn a_broker_trusts_only_inside_its_delegation() {
        const H: i64 = 3_600_000;
        let delegate =
            json!({"machine": "broker", "max_hours": 2, "role": "writer", "overlays": ["cache.target"], "name_prefix": "ci-"});
        let job = |name: &str, end: i64, role: &str, overlays: Value| json!({"machine": name, "expires_at": end, "role": role, "overlays": overlays, "name": name});
        let records = [
            trust("root", 10, json!({"machine": "broker"})),
            ("root", 20, "trust.delegate", delegate),
            trust("broker", 15, job("ci-early", 15 + H, "writer", json!(["cache.target"]))),
            trust("broker", 30, job("ci-ok", 30 + H, "writer", json!(["cache.target"]))),
            trust("broker", 31, job("ci-long", 31 + 3 * H, "writer", json!(["cache.target"]))),
            trust("broker", 32, job("ci-wide", 32 + H, "writer", json!(["cache.target", "cache.node"]))),
            trust("broker", 33, job("other", 33 + H, "writer", json!(["cache.target"]))),
            trust("broker", 34, json!({"machine": "ci-full", "name": "ci-full"})),
            ("root", 40, "trust.delegate", json!({"machine": "broker", "revoked": true})),
            trust("broker", 50, job("ci-late", 50 + H, "reader", json!(["cache.target"]))),
        ];
        let rules = rules("root", records.iter().map(|(signer, time, op, data)| TrustRecord { signer, time: *time, op, data }));
        assert_eq!(rules.limited.keys().collect::<Vec<_>>(), ["ci-ok"], "{rules:?}");
        assert!(!rules.trust_counts("broker", 34, &json!({"machine": "ci-full"})), "a broker makes no full machine");
        assert!(rules.trust_counts("root", 34, &json!({"machine": "x"})));
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
