//! Validate limited merge records before deriving reader policy.
//! Input signatures and contiguous machine prefixes must already be verified.
use crate::{changes, Record};
use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use layr_store::{public, record};
use std::collections::{BTreeMap, BTreeSet};

pub struct Effective {
    pub lines: Vec<String>,
    /// Keys for canonical-main selection only, never manifest or epoch authority.
    pub merge_keys: BTreeMap<String, VerifyingKey>,
}

pub fn effective_lines(project: &str, lines: &[String], now: i64) -> Result<Effective> {
    let parsed = lines
        .iter()
        .map(|line| -> Result<(&String, Record)> { Ok((line, serde_json::from_str(record::parse_line(line, None)?.get())?)) })
        .collect::<Result<Vec<_>>>()?;
    // Change numbers count only from a full or a merge machine, as in the store's trust rules
    // (`layr_store::trust`): a reader or a cache writer that holds the numbering role gives no number.
    let limited: BTreeSet<&str> = parsed
        .iter()
        .filter(|(_, r)| r.op == "machine.trust" && layr_store::trust::Terms::of_trust(&r.data).is_some())
        .filter_map(|(_, r)| r.data["machine"].as_str())
        .collect();
    let numbering = |r: &Record| matches!(r.op.as_str(), changes::NUMBER_OP | changes::HANDOFF_OP);
    let limited_numbers = parsed.iter().any(|(_, r)| numbering(r) && limited.contains(r.machine.as_str()));
    // Preserve existing reader contracts when the project has no merge-role trust.
    if !parsed.iter().any(|(_, r)| r.op == "machine.trust" && r.data["role"] == "merge") {
        if !limited_numbers {
            return Ok(Effective { lines: lines.to_vec(), merge_keys: BTreeMap::new() });
        }
        let writers = public::release_writers_checked(project, lines)?;
        let kept = parsed
            .iter()
            .filter(|(_, r)| !numbering(r) || writers.full.contains_key(&r.machine))
            .map(|(line, _)| (*line).clone())
            .collect();
        return Ok(Effective { lines: kept, merge_keys: BTreeMap::new() });
    }
    let writers = public::release_writers_checked(project, lines)?;
    let mut ordered = parsed.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| (a.1.time, &a.1.machine, a.1.seq).cmp(&(b.1.time, &b.1.machine, b.1.seq)));
    let mut kept = Vec::with_capacity(lines.len());
    let mut current = Folded::new(now);
    for (line, r) in ordered {
        current.at = r.time;
        if let Some((_, terms)) = writers.merge.get(&r.machine) {
            if terms.refusal_in(r.time, &r.op, &r.data, None, &payload_of(r), &r.machine, &current).is_some() {
                continue;
            }
        } else if limited_numbers && numbering(r) && !writers.full.contains_key(&r.machine) {
            continue;
        }
        current.apply(r);
        kept.push((*line).clone());
    }
    Ok(Effective { lines: kept, merge_keys: writers.merge.into_iter().map(|(id, (key, _))| (id, key)).collect() })
}

/// What a record changes besides its data, for `layr_store::trust`.
fn payload_of(record: &Record) -> layr_store::trust::Payload<'_> {
    let lines = record
        .lines
        .iter()
        .map(|(id, change)| {
            let name = change.after.as_ref().map(|after| after.name.as_str());
            let after = change.after.as_ref();
            layr_store::trust::LineMove {
                id,
                new: change.before.is_none(),
                name,
                own: false,
                renamed: change.before.as_ref().is_some_and(|before| Some(before.name.as_str()) != name),
                head: after.map(|a| a.head.as_str()),
                owner: after.map(|a| a.owner),
                machine: after.map(|a| a.machine.as_str()),
            }
        })
        .collect();
    layr_store::trust::Payload { states: record.states.iter().map(|s| s.line.as_deref()).collect(), lines }
}

/// The accepted history before the record being checked, in the model's time/machine/sequence order.
struct Folded {
    lines: BTreeMap<String, (String, u32, String)>,
    queue: BTreeMap<String, serde_json::Value>,
    refs: BTreeMap<String, String>,
    expires: BTreeMap<String, i64>,
    /// The sole signer in the current target run; a mixed-signature run has none.
    setters: BTreeMap<String, String>,
    now: i64,
    at: i64,
}

impl Folded {
    fn new(now: i64) -> Folded {
        Folded {
            lines: BTreeMap::new(),
            queue: BTreeMap::new(),
            refs: BTreeMap::new(),
            expires: BTreeMap::new(),
            setters: BTreeMap::new(),
            now,
            at: now,
        }
    }

    fn apply(&mut self, record: &Record) {
        for (id, change) in &record.lines {
            match &change.after {
                Some(line) => {
                    self.lines.insert(id.clone(), (line.name.clone(), line.owner, line.machine.clone()));
                }
                None => {
                    self.lines.remove(id);
                }
            }
        }
        self.apply_queue(record);
        let change = &record.data["ref_change"];
        if let Some(name) = change["name"].as_str() {
            match change["after"]["expires_at"].as_i64() {
                Some(end) => {
                    self.expires.insert(name.to_string(), end);
                }
                None => {
                    self.expires.remove(name);
                }
            }
            match change["after"]["target"].as_str() {
                Some(target) if self.refs.get(name).map(String::as_str) == Some(target) => {
                    // Every set in this target run must have the same signer. A retry cannot undo ambiguity.
                    if self.setters.get(name) != Some(&record.machine) {
                        self.setters.remove(name);
                    }
                }
                Some(target) => {
                    self.refs.insert(name.to_string(), target.to_string());
                    self.setters.insert(name.to_string(), record.machine.clone());
                }
                None => {
                    self.refs.remove(name);
                    self.setters.remove(name);
                }
            };
        }
    }

    fn apply_queue(&mut self, record: &Record) {
        let change = &record.data["queue_change"];
        if let Some(id) = change["id"].as_str() {
            match &change["after"] {
                serde_json::Value::Null => self.queue.remove(id),
                after => self.queue.insert(id.to_string(), after.clone()),
            };
        }
    }
}

impl layr_store::trust::Current for Folded {
    fn line(&self, id: &str) -> Option<(String, u32, String)> {
        self.lines.get(id).cloned()
    }

    fn queue_entry(&self, id: &str) -> Option<serde_json::Value> {
        self.queue.get(id).cloned()
    }

    fn ref_target(&self, name: &str) -> Option<String> {
        if self.expires.get(name).is_some_and(|end| self.at >= *end) {
            return None;
        }
        self.refs.get(name).cloned()
    }

    fn ref_setter(&self, name: &str) -> Option<String> {
        if self.expires.get(name).is_some_and(|end| self.at >= *end) {
            return None;
        }
        self.setters.get(name).cloned()
    }

    fn now(&self) -> i64 {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Model, Project};
    use ed25519_dalek::SigningKey;
    use serde_json::{json, Value};

    fn signed(machine: &str, seq: u64, time: i64, op: &str, data: Value, key: &SigningKey) -> String {
        let mut r = Record::new(op);
        r.id = format!("01a1c000-0000-7000-9000-{:012x}", seq + if machine.ends_with("2") { 100 } else { 0 });
        r.project = "01a1c000-0000-7000-8000-000000000003".into();
        r.machine = machine.into();
        r.seq = seq;
        r.time = time;
        r.data = data;
        sign(r, key)
    }
    fn sign(r: Record, key: &SigningKey) -> String {
        let body = serde_json::to_string(&r).unwrap();
        let signature = layr_store::keys::sign_bytes(body.as_bytes(), key);
        format!(r#"{{"r":{body},"s":"{signature}"}}"#)
    }
    fn decode(line: &str) -> Record {
        serde_json::from_str(record::parse_line(line, None).unwrap().get()).unwrap()
    }
    fn fixture() -> (Vec<String>, SigningKey, Value) {
        let owner = SigningKey::from_bytes(&[7; 32]);
        let merger = SigningKey::from_bytes(&[8; 32]);
        let mut lines = vec![
            signed(
                "01a1c000-0000-7000-8000-000000000001",
                1,
                1,
                "machine",
                json!({"public_key":hex::encode(owner.verifying_key().to_bytes())}),
                &owner,
            ),
            signed(
                "01a1c000-0000-7000-8000-000000000002",
                1,
                1,
                "machine",
                json!({"public_key":hex::encode(merger.verifying_key().to_bytes())}),
                &merger,
            ),
        ];
        let mut project = decode(&signed(
            "01a1c000-0000-7000-8000-000000000001",
            2,
            2,
            "project",
            json!({"id":"01a1c000-0000-7000-8000-000000000003","config":{"queue.merge-machine":"01a1c000-0000-7000-8000-000000000002"}}),
            &owner,
        ));
        project.states.push(serde_json::from_value(json!({"id":"01a1c000-0000-7000-8000-000000000004","kind":"commit","parents":[],"subvol":"","author":{"name":"a","email":"a@example.com"},"time":2})).unwrap());
        project.lines = serde_json::from_value(json!({"01a1c000-0000-7000-8000-000000000006":{"before":null,"after":{"name":"main","head":"01a1c000-0000-7000-8000-000000000004","owner":1,"machine":"01a1c000-0000-7000-8000-000000000001"}}})).unwrap();
        lines.push(sign(project, &owner));
        lines.push(signed("01a1c000-0000-7000-8000-000000000001",3,3,"machine.trust",json!({"machine":"01a1c000-0000-7000-8000-000000000002","public_key":hex::encode(merger.verifying_key().to_bytes()),"role":"merge"}),&owner));
        let pending = json!({"id":"01a1c000-0000-7000-8000-000000000007","into":"main","target_line":"01a1c000-0000-7000-8000-000000000006","state":"01a1c000-0000-7000-8000-000000000005","added_by":1,"time":4,"status":"pending","title":"feat: source","future_field":{"keep":true}});
        lines.push(signed(
            "01a1c000-0000-7000-8000-000000000001",
            4,
            4,
            "queue.add",
            json!({"queue_change":{"id":"01a1c000-0000-7000-8000-000000000007","before":null,"after":pending}}),
            &owner,
        ));
        (lines, merger, pending)
    }
    fn completion(key: &SigningKey, pending: &Value) -> Record {
        let mut done = pending.clone();
        done["status"] = json!("done");
        done["result"] = json!("01a1c000-0000-7000-8000-000000000005");
        let mut r = decode(&signed(
            "01a1c000-0000-7000-8000-000000000002",
            2,
            5,
            "queue.result",
            json!({"queue_change":{"id":"01a1c000-0000-7000-8000-000000000007","before":pending,"after":done}}),
            key,
        ));
        r.states.push(serde_json::from_value(json!({"id":"01a1c000-0000-7000-8000-000000000005","kind":"commit","parents":["01a1c000-0000-7000-8000-000000000004"],"subvol":"","author":{"name":"a","email":"a@example.com"},"time":5})).unwrap());
        r.lines=serde_json::from_value(json!({"01a1c000-0000-7000-8000-000000000006":{"before":{"name":"main","head":"01a1c000-0000-7000-8000-000000000004","owner":1,"machine":"01a1c000-0000-7000-8000-000000000001"},"after":{"name":"main","head":"01a1c000-0000-7000-8000-000000000005","owner":1,"machine":"01a1c000-0000-7000-8000-000000000001"}}})).unwrap();
        r
    }
    fn model(lines: &[String]) -> (Effective, Project) {
        // Match the signature boundary of the real readers before filtering.
        let mut keys = BTreeMap::new();
        for line in lines {
            let r = decode(line);
            if r.op == "machine" && r.seq == 1 {
                keys.insert(r.machine, record::machine_key(record::parse_line(line, None).unwrap()).unwrap());
            }
        }
        for line in lines {
            let r = decode(line);
            record::verify_line(line.as_bytes(), &keys[&r.machine]).unwrap();
        }
        let effective = effective_lines("01a1c000-0000-7000-8000-000000000003", lines, 100).unwrap();
        let m = Model::from_verified_lines(effective.lines.iter().map(String::as_str), "01a1c000-0000-7000-8000-000000000001")
            .unwrap();
        (effective, m.projects["01a1c000-0000-7000-8000-000000000003"].clone())
    }
    #[test]
    fn numbers_count_from_a_merge_machine_but_not_its_handoff_nor_a_cache_writer() {
        let (owner, merge, writer) = (
            "01a1c000-0000-7000-8000-000000000001",
            "01a1c000-0000-7000-8000-000000000002",
            "01a1c000-0000-7000-8000-000000000009",
        );
        let (mut lines, merger, _) = fixture();
        let owner_key = SigningKey::from_bytes(&[7; 32]);
        let writer_key = SigningKey::from_bytes(&[9; 32]);
        let own = |seq: u64, time: i64, op: &str, data: Value, key: &SigningKey, machine: &str| {
            let mut r = decode(&signed(machine, seq, time, op, data, key));
            r.id = format!("01a1c000-0000-7000-a000-{:04x}{:08x}", machine.len() + machine.ends_with('9') as usize, seq);
            sign(r, key)
        };
        let writer_public = hex::encode(writer_key.verifying_key().to_bytes());
        lines.push(own(1, 1, "machine", json!({"public_key": writer_public}), &writer_key, writer));
        lines.push(own(
            5,
            5,
            "machine.trust",
            json!({"machine": writer, "public_key": writer_public, "role": "writer",
            "expires_at": 1_000_000}),
            &owner_key,
            owner,
        ));
        lines.push(own(6, 6, changes::HANDOFF_OP, json!({"machine": merge, "next": 1}), &owner_key, owner));
        lines.push(own(2, 7, changes::NUMBER_OP, json!({"numbers": {"c1": 1}}), &merger, merge));
        // The merge machine hands the role on: refused. The creator takes it for the cache writer, whose numbers
        // do not count.
        lines.push(own(3, 8, changes::HANDOFF_OP, json!({"machine": owner, "next": 2}), &merger, merge));
        lines.push(own(7, 9, changes::HANDOFF_OP, json!({"machine": writer, "next": 2}), &owner_key, owner));
        lines.push(own(2, 10, changes::NUMBER_OP, json!({"numbers": {"c2": 2}}), &writer_key, writer));
        let (effective, project) = model(&lines);
        let kept = |machine: &str, op: &str| effective.lines.iter().any(|l| decode(l).machine == machine && decode(l).op == op);
        assert!(kept(merge, changes::NUMBER_OP) && !kept(merge, changes::HANDOFF_OP));
        assert!(!kept(writer, changes::NUMBER_OP));
        let n = project.numbering();
        assert_eq!((n.number("c1"), n.number("c2"), n.authority.as_deref()), (Some(1), None, Some(writer)));
    }

    #[test]
    fn accepts_real_queue_state_and_rejects_claimed_before() {
        let (mut lines, key, pending) = fixture();
        let done = completion(&key, &pending);
        lines.push(sign(done.clone(), &key));
        let (effective, mut project) = model(&lines);
        let records = effective.lines.iter().map(|l| decode(l)).collect::<Vec<_>>();
        crate::main_head::select(&mut project, &records, &effective.merge_keys).unwrap();
        assert_eq!(project.line_list()[0].head, "01a1c000-0000-7000-8000-000000000005");
        // Newer/unknown queue fields must survive the older typed model.
        assert!(effective.lines.iter().any(|line| decode(line).id == done.id));
        let mut forged = done;
        forged.data["queue_change"]["before"]["title"] = json!("forged");
        lines.pop();
        lines.push(sign(forged, &key));
        let raw = Model::from_verified_lines(lines.iter().map(String::as_str), "01a1c000-0000-7000-8000-000000000001").unwrap();
        assert_eq!(raw.projects["01a1c000-0000-7000-8000-000000000003"].queue_list(None)[0].status, "done");
        let (effective, project) = model(&lines);
        assert!(!effective.lines.iter().any(|l| decode(l).op == "queue.result"));
        assert_eq!(project.line_list()[0].head, "01a1c000-0000-7000-8000-000000000004");
    }
    #[test]
    fn rejects_arbitrary_ref_and_never_moves_a_release_ref() {
        let (mut lines, key, _) = fixture();
        let reference =
            |name: &str, target: &str| json!({"ref_change":{"name":name,"before":null,"after":{"name":name,"target":target}}});
        lines.push(signed(
            "01a1c000-0000-7000-8000-000000000002",
            2,
            5,
            "ref.set",
            reference("release/v1", "01a1c000-0000-7000-8000-000000000004"),
            &key,
        ));
        lines.push(signed(
            "01a1c000-0000-7000-8000-000000000002",
            3,
            6,
            "ref.set",
            reference("release/v1", "01a1c000-0000-7000-8000-000000000005"),
            &key,
        ));
        lines.push(signed(
            "01a1c000-0000-7000-8000-000000000002",
            4,
            7,
            "ref.set",
            reference("env/production", "01a1c000-0000-7000-8000-000000000004"),
            &key,
        ));
        let raw = Model::from_verified_lines(lines.iter().map(String::as_str), "01a1c000-0000-7000-8000-000000000001").unwrap();
        assert_eq!(raw.projects["01a1c000-0000-7000-8000-000000000003"].ref_list("", 100).len(), 2);
        let (_, project) = model(&lines);
        let refs = project.ref_list("", 100);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "release/v1");
        assert_eq!(refs[0].target, "01a1c000-0000-7000-8000-000000000004");
    }
    #[test]
    fn updates_line_ownership_before_the_next_merge_record() {
        let (mut lines, key, pending) = fixture();
        let owner = SigningKey::from_bytes(&[7; 32]);
        let mut pending2 = pending.clone();
        pending2["id"] = json!("01a1c000-0000-7000-8000-000000000008");
        pending2["state"] = json!("01a1c000-0000-7000-8000-000000000009");
        lines.push(signed(
            "01a1c000-0000-7000-8000-000000000001",
            5,
            4,
            "queue.add",
            json!({"queue_change":{"id":pending2["id"],"before":null,"after":pending2}}),
            &owner,
        ));
        let mut first = completion(&key, &pending);
        first.lines.get_mut("01a1c000-0000-7000-8000-000000000006").unwrap().after.as_mut().unwrap().owner = 0;
        lines.push(sign(first, &key));
        let mut second = completion(&key, &pending2);
        second.id = "01a1c000-0000-7000-9000-000000000103".into();
        second.seq = 3;
        second.time = 6;
        second.data["queue_change"]["id"] = pending2["id"].clone();
        second.data["queue_change"]["after"]["result"] = pending2["state"].clone();
        second.states[0].id = pending2["state"].as_str().unwrap().into();
        second.states[0].parents = vec!["01a1c000-0000-7000-8000-000000000005".into()];
        second.lines.get_mut("01a1c000-0000-7000-8000-000000000006").unwrap().after.as_mut().unwrap().head =
            second.states[0].id.clone();
        // The second real pending entry cannot restore ownership from root to uid 1.
        lines.push(sign(second, &key));
        let (effective, project) = model(&lines);
        assert_eq!(effective.lines.iter().filter(|l| decode(l).op == "queue.result").count(), 1);
        assert!(project.state("01a1c000-0000-7000-8000-000000000009").is_none());
    }
    #[test]
    fn a_later_full_writer_change_does_not_erase_an_earlier_valid_merge() {
        let (mut lines, key, pending) = fixture();
        let owner = SigningKey::from_bytes(&[7; 32]);
        let done = completion(&key, &pending);
        lines.push(sign(done.clone(), &key));
        lines.push(signed(
            "01a1c000-0000-7000-8000-000000000001",
            5,
            9,
            "queue.remove",
            json!({"queue_change":{"id":pending["id"],"before":done.data["queue_change"]["after"],"after":null}}),
            &owner,
        ));
        let (_, project) = model(&lines);
        assert!(project.state("01a1c000-0000-7000-8000-000000000005").is_some());
        assert!(project.queue_list(None).is_empty());
    }
    #[test]
    fn expired_refs_are_not_live_authority() {
        use layr_store::trust::Current;
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut state = Folded::new(100);
        let r = decode(&signed(
            "01a1c000-0000-7000-8000-000000000001",
            5,
            5,
            "ref.set",
            json!({"ref_change":{"name":"release/v1","after":{"name":"release/v1","target":"01a1c000-0000-7000-8000-000000000004","expires_at":50}}}),
            &key,
        ));
        state.apply(&r);
        assert!(state.ref_target("release/v1").is_none());
        assert!(state.ref_setter("release/v1").is_none());
        let mut live = r;
        live.data["ref_change"]["after"]["expires_at"] = serde_json::Value::Null;
        state.apply(&live);
        assert!(state.ref_target("release/v1").is_some());
    }
    #[test]
    fn a_later_ref_expiry_does_not_erase_an_authorized_attachment() {
        let (mut lines, key, pending) = fixture();
        let owner = SigningKey::from_bytes(&[7; 32]);
        let target = "01a1c000-0000-7000-8000-000000000005";
        lines.push(sign(completion(&key, &pending), &key));
        let reference = |expires: Value| json!({"ref_change":{"name":"release/v1","before":null,"after":{"name":"release/v1","target":target,"expires_at":expires}}});
        lines.push(signed("01a1c000-0000-7000-8000-000000000002", 3, 6, "ref.set", reference(Value::Null), &key));
        lines.push(signed("01a1c000-0000-7000-8000-000000000001", 5, 8, "ref.set", reference(json!(9)), &owner));
        lines.push(signed("01a1c000-0000-7000-8000-000000000002",4,7,"release.files",json!({"reference":"release/v1","state":target,"files":[{"name":"artifact","size":1,"sha256":"a".repeat(64),"object":"files/01a1c000-0000-7000-8000-000000000010.age"}]}),&key));
        lines.push(signed("01a1c000-0000-7000-8000-000000000001", 6, 9, "ref.set", reference(Value::Null), &owner));
        let (effective, project) = model(&lines);
        assert!(effective.lines.iter().any(|l| decode(l).op == "release.files"));
        assert_eq!(project.release_files("release/v1", 100).unwrap().len(), 1);
    }
    #[test]
    fn another_signer_in_the_same_target_run_blocks_attachment() {
        let (mut lines, key, pending) = fixture();
        let owner = SigningKey::from_bytes(&[7; 32]);
        lines.push(sign(completion(&key, &pending), &key));
        let target = "01a1c000-0000-7000-8000-000000000005";
        let reference = json!({"ref_change":{"name":"release/v1","before":null,"after":{"name":"release/v1","target":target}}});
        lines.push(signed("01a1c000-0000-7000-8000-000000000002", 3, 6, "ref.set", reference.clone(), &key));
        lines.push(signed("01a1c000-0000-7000-8000-000000000001", 5, 7, "ref.set", reference.clone(), &owner));
        // A retry must not restore sole setter authority after the creator's same-target set.
        lines.push(signed("01a1c000-0000-7000-8000-000000000002", 4, 8, "ref.set", reference, &key));
        lines.push(signed("01a1c000-0000-7000-8000-000000000002",5,9,"release.files",json!({"reference":"release/v1","state":target,"files":[{"name":"artifact","size":1,"sha256":"a".repeat(64),"object":"files/01a1c000-0000-7000-8000-000000000010.age"}]}),&key));
        let (effective, _) = model(&lines);
        assert!(!effective.lines.iter().any(|line| decode(line).op == "release.files"));
    }
}
