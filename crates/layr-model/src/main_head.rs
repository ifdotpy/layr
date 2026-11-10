//! Select main from the project creator's configured merge machine.
use crate::{Project, Record};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use std::collections::BTreeMap;

pub fn select(project: &mut Project, records: &[Record], trusted: &BTreeMap<String, VerifyingKey>) -> Result<()> {
    let Some(value) = project.config.get("queue.merge-machine") else {
        return Ok(());
    };
    let machine = value.as_str().context("invalid queue.merge-machine")?;
    let mains: Vec<_> = project.line_list().into_iter().filter(|line| line.name == "main").collect();
    if mains.len() != 1 {
        bail!("project must have exactly one main line");
    }
    let main = &mains[0];
    if project.state(&main.head).is_none() {
        bail!("creator main state is absent");
    }
    let view = project.lines.get(&main.id).context("main line is absent")?;
    if !view.heads.contains_key(machine) {
        return Ok(());
    }
    if !trusted.contains_key(machine) {
        bail!("queue.merge-machine is not trusted");
    }
    // Adoption selects a worker's private copy, not the published queue result.
    // Keep explicit unmatched advances visible instead of accepting them as main.
    let Some((_, merged)) = records
        .iter()
        .filter(|record| record.machine == machine && record.op != "line.adopt")
        .filter_map(|record| {
            record.lines.get(&main.id).and_then(|change| change.after.as_ref()).map(|line| (record, line.head.clone()))
        })
        .max_by(|(a, _), (b, _)| crate::order(a).cmp(&crate::order(b)))
    else {
        return Ok(());
    };
    if merged == main.head || project.ancestors(&main.head).contains(&merged) {
        return Ok(());
    }
    if project.state(&merged).is_none() || !project.ancestors(&merged).contains(&main.head) {
        bail!("merge machine main does not extend creator main");
    }
    let completed = records.iter().any(|record| {
        let after = &record.data["queue_change"]["after"];
        record.machine == machine
            && record.op == "queue.result"
            && after["status"] == "done"
            && after["target_line"] == main.id
            && after["result"] == merged
            && record.lines.get(&main.id).and_then(|change| change.after.as_ref()).is_some_and(|line| line.head == merged)
    });
    if !completed {
        bail!("merge machine main has no matching queue completion");
    }
    project.lines.get_mut(&main.id).context("main line is absent")?.line.head = merged;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (Project, Vec<Record>, BTreeMap<String, VerifyingKey>) {
        let mut owner = Record::new("project");
        owner.id = "r1".into();
        owner.machine = "owner".into();
        owner.seq = 1;
        owner.time = 1;
        owner.data = json!({"id":"p","config":{"queue.merge-machine":"merger"}});
        owner.states.push(serde_json::from_value(json!({"id":"s0","kind":"commit","parents":[],"subvol":"", "author":{"name":"a","email":"a@example.com"},"time":1})).unwrap());
        owner.lines = serde_json::from_value(
            json!({"line":{"before":null,"after":{"name":"main","head":"s0","owner":1,"machine":"owner"}}}),
        )
        .unwrap();
        let mut merged = Record::new("queue.result");
        merged.id = "r2".into();
        merged.machine = "merger".into();
        merged.seq = 1;
        merged.time = 2;
        merged.states.push(serde_json::from_value(json!({"id":"s1","kind":"commit","parents":["s0"],"subvol":"", "author":{"name":"a","email":"a@example.com"},"time":2})).unwrap());
        merged.lines = serde_json::from_value(json!({"line":{"before":{"name":"main","head":"s0","owner":1,"machine":"owner"},"after":{"name":"main","head":"s1","owner":1,"machine":"owner"}}})).unwrap();
        merged.data = json!({"queue_change":{"id":"q","before":null,"after":{"id":"q","into":"main","state":"s1","added_by":1,"time":2,"status":"done","target_line":"line","result":"s1"}}});
        let records = vec![owner, merged];
        let project = Project::from_records("p", records.clone(), "owner").unwrap();
        let trusted = BTreeMap::from([("merger".into(), ed25519_dalek::SigningKey::from_bytes(&[9; 32]).verifying_key())]);
        (project, records, trusted)
    }

    #[test]
    fn main_keeps_a_newer_creator_head_until_the_queue_extends_it() {
        let (_, mut records, trusted) = fixture();
        let mut owner = records[0].clone();
        owner.id = "r3".into();
        owner.op = "save".into();
        owner.seq = 2;
        owner.time = 3;
        owner.data = json!({});
        owner.states[0].id = "s2".into();
        owner.states[0].parents = vec!["s1".into()];
        owner.states[0].time = 3;
        owner.lines.get_mut("line").unwrap().after.as_mut().unwrap().head = "s2".into();
        records.push(owner);
        let mut project = Project::from_records("p", records.clone(), "owner").unwrap();
        select(&mut project, &records, &trusted).unwrap();
        assert_eq!(project.line_list()[0].head, "s2", "old merger head cannot roll main back");

        let mut completed = records[1].clone();
        completed.id = "r4".into();
        completed.seq = 2;
        completed.time = 4;
        completed.states[0].id = "s3".into();
        completed.states[0].parents = vec!["s2".into()];
        completed.states[0].time = 4;
        let change = completed.lines.get_mut("line").unwrap();
        change.before.as_mut().unwrap().head = "s2".into();
        change.after.as_mut().unwrap().head = "s3".into();
        completed.data["queue_change"]["id"] = json!("q2");
        completed.data["queue_change"]["after"]["id"] = json!("q2");
        completed.data["queue_change"]["after"]["state"] = json!("s3");
        completed.data["queue_change"]["after"]["result"] = json!("s3");
        records.push(completed);
        let mut project = Project::from_records("p", records.clone(), "owner").unwrap();
        assert_eq!(project.line_list()[0].head, "s2", "the local owner view remains separate");
        select(&mut project, &records, &trusted).unwrap();
        assert_eq!(project.line_list()[0].head, "s3");
        assert_eq!(project.line_list()[0].machine, "owner");
    }

    #[test]
    fn main_uses_only_a_trusted_matching_queue_result() {
        let (mut project, records, trusted) = fixture();
        assert_eq!(project.line_list()[0].head, "s0");
        select(&mut project, &records, &trusted).unwrap();
        assert_eq!(project.line_list()[0].head, "s1");

        let (mut project, mut records, trusted) = fixture();
        records[1].op = "save".into();
        assert!(select(&mut project, &records, &trusted).is_err());
        assert_eq!(project.line_list()[0].head, "s0");

        let (mut project, records, _) = fixture();
        assert!(select(&mut project, &records, &BTreeMap::new()).is_err());
        assert_eq!(project.line_list()[0].head, "s0");

        let (mut project, records, _) = fixture();
        project.lines.get_mut("line").unwrap().heads.remove("merger");
        select(&mut project, &records, &BTreeMap::new()).unwrap();
        assert_eq!(project.line_list()[0].head, "s0");

        let (_, mut records, trusted) = fixture();
        records[1].states[0].parents.clear();
        let mut project = Project::from_records("p", records.clone(), "owner").unwrap();
        assert!(select(&mut project, &records, &trusted).is_err());
        assert_eq!(project.line_list()[0].head, "s0");
    }
    #[test]
    fn a_later_private_adoption_does_not_undo_the_completed_queue_head() {
        let (_, mut records, trusted) = fixture();
        let mut adoption = records[1].clone();
        adoption.id = "r3".into();
        adoption.seq = 2;
        adoption.time = 3;
        adoption.op = "line.adopt".into();
        adoption.states.clear();
        adoption.data = serde_json::json!({});
        adoption.lines.get_mut("line").unwrap().after.as_mut().unwrap().head = "s0".into();
        records.push(adoption);
        let mut project = Project::from_records("p", records.clone(), "owner").unwrap();
        assert_eq!(project.lines["line"].heads["merger"], "s0");
        select(&mut project, &records, &trusted).unwrap();
        assert_eq!(project.line_list()[0].head, "s1");
        assert_eq!(project.lines["line"].heads["merger"], "s0", "the private head stays in history");
    }

    #[test]
    fn a_noop_reset_keeps_the_existing_verified_queue_head() {
        let (_, mut records, trusted) = fixture();
        let mut reset = records[1].clone();
        reset.id = "r3".into();
        reset.seq = 2;
        reset.time = 3;
        reset.op = "reset".into();
        reset.states.clear();
        reset.data = serde_json::json!({});
        records.push(reset);
        let mut project = Project::from_records("p", records.clone(), "owner").unwrap();
        select(&mut project, &records, &trusted).unwrap();
        assert_eq!(project.line_list()[0].head, "s1");
    }
}
