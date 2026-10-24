//! Key authority follows signed manager changes. The parent carries their public proofs.
use crate::{model::Record, oplog, repo::Repo};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub struct Authority {
    pub machines: BTreeSet<String>,
    pub previous: Option<String>,
    retained: BTreeMap<String, String>,
    changes: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
pub struct Change {
    pub parent: String,
    pub overlay: String,
    pub name: String,
    pub machines: BTreeSet<String>,
    pub previous: Option<String>,
    pub retained: BTreeMap<String, String>,
}

pub fn digest(line: &str) -> String {
    hex::encode(Sha256::digest(line.as_bytes()))
}

impl Authority {
    pub fn permits(&self, record: &Record, line: &str) -> bool {
        self.machines.contains(&record.machine) || self.retained.get(&record.id) == Some(&digest(line))
    }
    pub fn require(&self, machine: &str) -> Result<()> {
        if !self.machines.contains(machine) {
            bail!("machine {machine} is not an overlay key manager");
        }
        Ok(())
    }
    pub fn validate(&self, lines: &[String], overlay: &str, keys: &BTreeMap<String, VerifyingKey>) -> Result<()> {
        for line in lines {
            let record = oplog::parse_line(line, None)?.record;
            let key = keys.get(&record.machine).with_context(|| format!("overlay signer {} is not trusted", record.machine))?;
            let record = oplog::parse_line(line, Some(key))?.record;
            if record.project != overlay {
                bail!("overlay record belongs to another project");
            }
            if record.op == "overlay.managers" {
                if self.changes.get(&record.id) != Some(&digest(line)) {
                    bail!("overlay manager record from {} has no valid parent authority proof", record.machine);
                }
            } else if protected(&record) && !self.permits(&record, line) {
                bail!("overlay record {} signer {} is not a key manager", record.op, record.machine);
            }
        }
        Ok(())
    }
}

pub fn protected(record: &Record) -> bool {
    record.op == "overlay.key"
        || record.op == "overlay.readers"
        || record.data["config"].get("access").is_some()
        || record.data["config"].get("deny").is_some()
        || record.data["config"].get("overlay.revocation").is_some()
}

pub fn from_lines(parent: &str, lines: &[String], keys: &BTreeMap<String, VerifyingKey>) -> Result<BTreeMap<String, Authority>> {
    let mut records = Vec::new();
    for line in lines {
        let record = oplog::parse_line(line, None)?.record;
        if record.op == "overlay.create" || record.op == "overlay.authority" {
            let key = keys.get(&record.machine).context("overlay authority signer is not trusted")?;
            oplog::parse_line(line, Some(key))?;
            records.push(record);
        }
    }
    records.sort_by(|a, b| (a.time, &a.machine, a.seq).cmp(&(b.time, &b.machine, b.seq)));
    let mut authorities = BTreeMap::new();
    let mut ids = BTreeMap::new();
    let mut pending = Vec::new();
    for record in records {
        if record.op == "overlay.create" {
            let name = record.data["name"].as_str().context("overlay creation has no name")?;
            crate::store::check_name("overlay", name)?;
            if authorities.contains_key(name) {
                bail!("conflicting overlay creator for {name}");
            }
            let id = record.data["config"][format!("overlay.{name}")]["id"].as_str().map(str::to_owned);
            ids.insert(name.to_string(), id);
            authorities.insert(
                name.to_string(),
                Authority {
                    machines: BTreeSet::from([record.machine]),
                    previous: None,
                    retained: BTreeMap::new(),
                    changes: BTreeMap::new(),
                },
            );
        } else {
            let signed = record.data["authority"].as_str().context("overlay authority proof is missing")?;
            let change = oplog::parse_line(signed, None)?.record;
            let key = keys.get(&change.machine).context("overlay manager signer is not trusted")?;
            let change = oplog::parse_line(signed, Some(key))?.record;
            crate::model::validate(&change)?;
            if change.op != "overlay.managers" {
                bail!("invalid overlay authority proof");
            }
            let data: Change = serde_json::from_value(change.data.clone())?;
            if data.parent != parent
                || change.project != data.overlay
                || !crate::model::is_uuid(&data.overlay)
                || data.machines.is_empty()
                || data.machines.iter().any(|id| !crate::model::is_uuid(id))
                || record.data["name"].as_str() != Some(&data.name)
            {
                bail!("overlay manager proof has invalid project bindings");
            }
            pending.push((change, signed.to_string(), data));
        }
    }
    // The signed previous-record link gives the causal order, regardless of machine clocks.
    while !pending.is_empty() {
        let position = pending
            .iter()
            .position(|(_, _, change)| authorities.get(&change.name).is_some_and(|a| a.previous == change.previous))
            .context("overlay manager history has a gap or a fork")?;
        let (record, line, change) = pending.remove(position);
        let authority = authorities.get_mut(&change.name).context("unknown overlay authority")?;
        if ids[&change.name].as_ref().is_some_and(|id| id != &change.overlay) {
            bail!("overlay manager proof names another overlay");
        }
        authority.require(&record.machine)?;
        if !change.machines.contains(&record.machine) {
            bail!("a remaining key manager must sign manager removal: {}", record.machine);
        }
        for (id, hash) in &authority.retained {
            if change.retained.get(id) != Some(hash) {
                bail!("overlay manager change loses authenticated history");
            }
        }
        if change.retained.values().any(|hash| hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())) {
            bail!("invalid retained overlay record digest");
        }
        authority.machines = change.machines;
        authority.retained = change.retained;
        authority.changes.insert(record.id.clone(), digest(&line));
        authority.previous = Some(record.id);
    }
    Ok(authorities)
}

pub fn of(repo: &Repo, name: &str) -> Result<Authority> {
    let lines: Vec<_> = repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    from_lines(&repo.p.id, &lines, &crate::store_remote::overlays::trusted_keys(repo)?)?
        .remove(name)
        .context("overlay has no creator authority")
}

pub fn validate_import(project: &crate::store::Project, incoming: &[String]) -> Result<()> {
    let Some((parent, name)) = project.name.split_once('%') else { return Ok(()) };
    let repo = Repo { p: crate::store::Project::open(project.store, parent)? };
    let authority = of(&repo, name)?;
    let mut lines: BTreeSet<String> = project.export_jsonl()?.lines().map(str::to_owned).collect();
    lines.extend(incoming.iter().cloned());
    let id = if project.id.is_empty() {
        incoming
            .first()
            .map(|line| oplog::parse_line(line, None).map(|signed| signed.record.project))
            .transpose()?
            .unwrap_or_default()
    } else {
        project.id.clone()
    };
    authority.validate(&lines.into_iter().collect::<Vec<_>>(), &id, &crate::store_remote::overlays::trusted_keys(&repo)?)
}

pub fn retained(authority: &Authority, repo: &Repo) -> Result<BTreeMap<String, String>> {
    let mut retained = authority.retained.clone();
    for line in repo.p.export_jsonl()?.lines() {
        let record = oplog::parse_line(line, None)?.record;
        if protected(&record) {
            if !authority.permits(&record, line) {
                bail!("overlay record signer {} is not a key manager", record.machine);
            }
            retained.insert(record.id, digest(line));
        }
    }
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(op: &str, project: &str, machine: &str, data: serde_json::Value, seq: u64) -> Record {
        let mut r = Record::new(op);
        r.id = crate::ids::new_id();
        r.project = project.into();
        r.machine = machine.into();
        r.seq = seq;
        r.time = seq as i64;
        r.data = data;
        r
    }
    #[test]
    fn manager_removal_keeps_exact_old_records_and_refuses_new_records_from_that_signer() {
        let (a, b, d) = (crate::ids::new_id(), crate::ids::new_id(), crate::ids::new_id());
        let (parent, overlay) = (crate::ids::new_id(), crate::ids::new_id());
        let ka = ed25519_dalek::SigningKey::from_bytes(&[1; 32]);
        let kb = ed25519_dalek::SigningKey::from_bytes(&[2; 32]);
        let kd = ed25519_dalek::SigningKey::from_bytes(&[3; 32]);
        let keys =
            BTreeMap::from([(a.clone(), ka.verifying_key()), (b.clone(), kb.verifying_key()), (d.clone(), kd.verifying_key())]);
        let creation = record(
            "overlay.create",
            &parent,
            &a,
            serde_json::json!({"name":"secrets","config":{"overlay.secrets":{"id":overlay}}}),
            1,
        );
        let old = record("overlay.key", &overlay, &a, serde_json::json!({}), 3);
        let old_line = oplog::sign_line(&old, &ka).unwrap();
        let retained = BTreeMap::from([(old.id.clone(), digest(&old_line))]);
        let first = record(
            "overlay.managers",
            &overlay,
            &a,
            serde_json::to_value(Change {
                parent: parent.clone(),
                overlay: overlay.clone(),
                name: "secrets".into(),
                machines: BTreeSet::from([a.clone(), d.clone()]),
                previous: None,
                retained: retained.clone(),
            })
            .unwrap(),
            4,
        );
        let second = record(
            "overlay.managers",
            &overlay,
            &d,
            serde_json::to_value(Change {
                parent: parent.clone(),
                overlay: overlay.clone(),
                name: "secrets".into(),
                machines: BTreeSet::from([d.clone()]),
                previous: Some(first.id.clone()),
                retained,
            })
            .unwrap(),
            2,
        );
        let mut lines = vec![oplog::sign_line(&creation, &ka).unwrap()];
        for (change, key) in [(&second, &kd), (&first, &ka)] {
            let proof = oplog::sign_line(change, key).unwrap();
            let wrapper =
                record("overlay.authority", &parent, &change.machine, serde_json::json!({"name":"secrets","authority":proof}), 9);
            lines.push(oplog::sign_line(&wrapper, key).unwrap());
        }
        let authority = from_lines(&parent, &lines, &keys).unwrap().remove("secrets").unwrap();
        assert_eq!(authority.machines, BTreeSet::from([d.clone()]));
        assert!(authority.permits(&old, &old_line));
        let mut changed = old.clone();
        changed.data = serde_json::json!({"recipient":"different"});
        assert!(!authority.permits(&changed, &oplog::sign_line(&changed, &ka).unwrap()));
        let bad = record("overlay.readers", &overlay, &b, serde_json::json!({"config":{"access":{}}}), 10);
        assert!(authority
            .validate(&[oplog::sign_line(&bad, &kb).unwrap()], &overlay, &keys)
            .unwrap_err()
            .to_string()
            .contains(&b));
        let next = record("overlay.key", &overlay, &d, serde_json::json!({}), 10);
        assert!(authority.permits(&next, &oplog::sign_line(&next, &kd).unwrap()));
    }
}
