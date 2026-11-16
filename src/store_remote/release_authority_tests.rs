//! Ordered release authority regression fixtures, shared with the Cloud reader.

use super::*;
use layr_store::record;
use ed25519_dalek::SigningKey;
use layr_model::{Model, Project};
use serde_json::{json, Value};

fn signed(machine: &str, seq: u64, time: i64, op: &str, data: Value, key: &SigningKey) -> String {
    let mut r = Record::untrusted_preview(op);
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
    serde_json::from_str(record::parse_line(line).unwrap().get()).unwrap()
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
fn model(lines: &[String]) -> (Vec<Record>, Project) {
    // Match the signature boundary of the real readers before filtering.
    let mut keys = BTreeMap::new();
    for line in lines {
        let r = decode(line);
        if r.op == "machine" && r.seq == 1 {
            keys.insert(r.machine, record::machine_key(record::parse_line(line).unwrap().raw()).unwrap());
        }
    }
    let history = reader::Records { lines: lines.to_vec(), keys, overlaps: 0, gaps: Vec::new() };
    let project = super::model("01a1c000-0000-7000-8000-000000000003", &history, "01a1c000-0000-7000-8000-000000000001").unwrap();
    let records = authorized_records("01a1c000-0000-7000-8000-000000000003", &history).unwrap()
        .into_iter().map(|r| r.record().clone()).collect();
    (records, project)
}

#[test]
fn accepts_real_queue_state_and_rejects_claimed_before() {
    let (mut lines, key, pending) = fixture();
    let done = completion(&key, &pending);
    lines.push(sign(done.clone(), &key));
    let (effective, project) = model(&lines);
    assert!(project.state("01a1c000-0000-7000-8000-000000000005").is_some());
    // Newer/unknown queue fields must survive the older typed model.
    assert!(effective.iter().any(|line| line.id == done.id));
    let mut forged = done;
    forged.data["queue_change"]["before"]["title"] = json!("forged");
    lines.pop();
    lines.push(sign(forged, &key));
    let raw = Model::preview_lines(lines.iter().map(String::as_str), "01a1c000-0000-7000-8000-000000000001").unwrap();
    assert_eq!(raw.projects["01a1c000-0000-7000-8000-000000000003"].queue_list(None)[0].status, "done");
    let (effective, project) = model(&lines);
    assert!(!effective.iter().any(|l| l.op == "queue.result"));
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
    let raw = Model::preview_lines(lines.iter().map(String::as_str), "01a1c000-0000-7000-8000-000000000001").unwrap();
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
    (match &mut first.lines.get_mut("01a1c000-0000-7000-8000-000000000006").unwrap().transition { crate::model::LineTransition::Move { after, .. } | crate::model::LineTransition::Create { after } => after, _ => panic!("fixture needs after") }).owner = 0;
    lines.push(sign(first, &key));
    let mut second = completion(&key, &pending2);
    second.id = "01a1c000-0000-7000-9000-000000000103".into();
    second.seq = 3;
    second.time = 6;
    second.data["queue_change"]["id"] = pending2["id"].clone();
    second.data["queue_change"]["after"]["result"] = pending2["state"].clone();
    second.states[0].id = pending2["state"].as_str().unwrap().into();
    second.states[0].parents = vec!["01a1c000-0000-7000-8000-000000000005".into()];
    (match &mut second.lines.get_mut("01a1c000-0000-7000-8000-000000000006").unwrap().transition { crate::model::LineTransition::Move { after, .. } | crate::model::LineTransition::Create { after } => after, _ => panic!("fixture needs after") }).head =
        second.states[0].id.clone();
    // The second real pending entry cannot restore ownership from root to uid 1.
    lines.push(sign(second, &key));
    let (effective, project) = model(&lines);
    assert_eq!(effective.iter().filter(|l| l.op == "queue.result").count(), 1);
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
    assert!(effective.iter().any(|l| l.op == "release.files"));
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
    assert!(!effective.iter().any(|line| line.op == "release.files"));
}

#[test]
fn untrusted_machine_announcement_does_not_block_the_release_model() {
    let (mut lines, _, _) = fixture();
    let unknown = "01a1c000-0000-7000-8000-000000000099";
    let key = SigningKey::from_bytes(&[99; 32]);
    let before = model(&lines).1;
    let mut announcement = decode(&signed(unknown, 1, 10, "machine", json!({"public_key":hex::encode(key.verifying_key().to_bytes())}), &key));
    announcement.id = crate::ids::new_id();
    lines.push(sign(announcement, &key));
    let (authorized, after) = model(&lines);
    assert_eq!(after.line_list(), before.line_list());
    assert!(!after.machines.contains_key(unknown));
    assert!(authorized.iter().all(|r| r.machine != unknown));
}
