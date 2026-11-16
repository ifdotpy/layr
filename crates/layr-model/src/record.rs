//! Signature proof and project permission are separate capabilities.
use crate::{merge_authority, Record};
use anyhow::{ensure, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_store::record::{parse_line, VerifiedRecord};
use layr_types::{MachineId, ProjectId};
use std::collections::BTreeMap;

/// The project whose authority accepted the signer. Kept with every checked record.
#[derive(Clone, Debug)]
pub struct ProjectScope {
    project: ProjectId,
    terms: Option<layr_store::trust::Terms>,
}

impl ProjectScope {
    pub fn project(&self) -> ProjectId {
        self.project
    }
    pub fn terms(&self) -> Option<&layr_store::trust::Terms> {
        self.terms.as_ref()
    }
}

/// No public constructor or Deserialize implementation can manufacture this proof.
#[derive(Clone, Debug)]
#[must_use]
pub struct AuthorizedRecord<Scope> {
    record: Record,
    signed: VerifiedRecord,
    scope: Scope,
}

impl<Scope> AuthorizedRecord<Scope> {
    pub fn record(&self) -> &Record {
        &self.record
    }
    pub fn signed(&self) -> &VerifiedRecord {
        &self.signed
    }
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
    pub(crate) fn into_record(self) -> Record {
        self.record
    }
}

/// Check project trust against authenticated history, then apply each signer's limits.
/// Supplied keys anchor the reader's known machine identities; announcements alone
/// cannot replace them. Policy records from limited machines never grant authority.
pub fn authorize_lines(
    project: &str,
    lines: &[String],
    known_keys: &BTreeMap<String, VerifyingKey>,
    now: i64,
) -> Result<Vec<AuthorizedRecord<ProjectScope>>> {
    authorize(project, lines, known_keys, now, None)
}

/// A local catalog explicitly grants these project keys authority. Its own machine
/// keeps its records even when another reader's policy would refuse them; overlays
/// use their parent's trust limits. All signatures and limits are still checked here.
pub struct LocalAuthority<'a> {
    pub machine: MachineId,
    /// Signing identities admitted by a server cannot grant local machine trust.
    pub trust_writers: &'a BTreeMap<String, VerifyingKey>,
    pub limits: &'a layr_store::trust::Rules,
    pub overlay: Option<&'a str>,
}

pub fn authorize_local_lines(
    project: &str,
    lines: &[String],
    trusted_keys: &BTreeMap<String, VerifyingKey>,
    authority: LocalAuthority<'_>,
    now: i64,
) -> Result<Vec<AuthorizedRecord<ProjectScope>>> {
    authorize(project, lines, trusted_keys, now, Some(authority))
}

fn authorize(
    project: &str,
    lines: &[String],
    known_keys: &BTreeMap<String, VerifyingKey>,
    now: i64,
    local: Option<LocalAuthority<'_>>,
) -> Result<Vec<AuthorizedRecord<ProjectScope>>> {
    let project_id = ProjectId::parse(project)?;
    let mut verified = Vec::new();
    for line in lines {
        let signed = parse_line(line)?;
        let record: Record = serde_json::from_str(signed.get())?;
        crate::validation::validate(&record)?;
        ensure!(
            record.project == project || (record.project.is_empty() && record.op == "machine" && record.seq == 1),
            "policy records belong to another project"
        );
        let key = known_keys.get(&record.machine).context("record signer is not a known project machine")?;
        verified.push((record, signed.verify(key)?));
    }
    let resolved;
    let (trusted, limits) = match &local {
        Some(local) => (known_keys, local.limits),
        None => {
            resolved = layr_store::public::project_signers(project, verified.iter().map(|(_, signed)| signed))?;
            (&resolved.0, &resolved.1)
        }
    };
    let overlay = verified
        .iter()
        .find(|(r, _)| r.op == "project")
        .and_then(|(r, _)| r.data["config"]["overlay"]["name"].as_str())
        .map(str::to_owned);
    let overlay = local.as_ref().and_then(|local| local.overlay).or(overlay.as_deref());
    verified.sort_by(|a, b| (a.0.time, &a.0.machine, a.0.seq).cmp(&(b.0.time, &b.0.machine, b.0.seq)));
    let mut current = merge_authority::Folded::new(now);
    let mut out = Vec::new();
    for (record, signed) in verified {
        if trusted.get(&record.machine) != Some(signed.key()) {
            continue;
        }
        current.at = record.time;
        let own = local.as_ref().is_some_and(|local| record.machine == local.machine.to_string());
        if !own
            && matches!(record.op.as_str(), "machine.trust" | "machine.untrust" | "trust.delegate")
            && local.as_ref().is_some_and(|local| local.trust_writers.get(&record.machine) != Some(signed.key()))
        {
            continue;
        }
        if !own && record.op == "machine.trust" && !limits.trust_counts(&record.machine, record.time, &record.data) {
            continue;
        }
        if let Some(terms) = limits.get(&record.machine).filter(|_| !own) {
            use layr_store::trust::Current;
            let mut payload = merge_authority::payload_of(&record);
            for movement in &mut payload.lines {
                movement.own = current.line(movement.id).map_or(movement.new, |(_, _, machine)| machine == record.machine);
            }
            if terms.refusal_in(record.time, &record.op, &record.data, overlay, &payload, &record.machine, &current).is_some() {
                continue;
            }
        }
        current.apply(&record);
        out.push(AuthorizedRecord {
            scope: ProjectScope { project: project_id, terms: limits.get(&record.machine).cloned() },
            record,
            signed,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Actor, DraftRecord, Model, RecordIdentity};
    use ed25519_dalek::SigningKey;
    use layr_types::{MachineId, RecordId};
    use serde_json::json;
    use std::num::NonZeroU64;

    const PROJECT: &str = "01a12290-0000-7000-8000-000000000001";
    const OWNER: &str = "01a12290-0000-7000-8000-000000000002";
    const READER: &str = "01a12290-0000-7000-8000-000000000003";

    fn line(machine: &str, seq: u64, op: &str, data: serde_json::Value, key: &SigningKey) -> String {
        let mut draft = DraftRecord::new(op);
        draft.data = data;
        draft
            .sign(
                RecordIdentity {
                    id: RecordId::parse(&format!(
                        "01a12290-0000-7000-8000-{:012x}",
                        100 + seq + if machine == READER { 1000 } else { 0 }
                    ))
                    .unwrap(),
                    project: ProjectId::parse(PROJECT).unwrap(),
                    machine: MachineId::parse(machine).unwrap(),
                    seq: NonZeroU64::new(seq).unwrap(),
                    time: i64::try_from(seq).unwrap_or(i64::MAX),
                    actor: Actor { uid: 1000, name: "test".into(), session: None },
                },
                key,
            )
            .unwrap()
            .line()
            .into()
    }

    fn history() -> (Vec<String>, BTreeMap<String, VerifyingKey>) {
        let owner = SigningKey::from_bytes(&[1; 32]);
        let reader = SigningKey::from_bytes(&[2; 32]);
        let keys = BTreeMap::from([(OWNER.into(), owner.verifying_key()), (READER.into(), reader.verifying_key())]);
        let lines = vec![
            line(OWNER, 1, "machine", json!({"public_key":hex::encode(owner.verifying_key().to_bytes())}), &owner),
            line(OWNER, 2, "project", json!({"id":PROJECT,"name":"test"}), &owner),
            line(
                OWNER,
                3,
                "machine.trust",
                json!({"machine":READER,"public_key":hex::encode(reader.verifying_key().to_bytes()),"role":"reader"}),
                &owner,
            ),
            line(READER, 1, "machine", json!({"public_key":hex::encode(reader.verifying_key().to_bytes())}), &reader),
            line(
                READER,
                2,
                "machine.trust",
                json!({"machine":OWNER,"public_key":hex::encode(reader.verifying_key().to_bytes())}),
                &reader,
            ),
        ];
        (lines, keys)
    }

    #[test]
    fn signature_validity_does_not_authorize_a_reader_to_change_trust() {
        let (lines, keys) = history();
        let checked = authorize_lines(PROJECT, &lines, &keys, 100).unwrap();
        assert!(checked.iter().all(|r| r.record().machine != READER || r.record().op == "machine"));
        let reader = checked.iter().find(|r| r.record().machine == READER).unwrap();
        assert_eq!(reader.scope().project().to_string(), PROJECT);
        assert_eq!(reader.scope().terms().unwrap().role, layr_store::trust::Role::Reader);
        assert_eq!(reader.signed().key(), &keys[READER]);
        assert!(Model::from_verified_lines(checked, OWNER).unwrap().projects.contains_key(PROJECT));
        let mut wrong = keys.clone();
        wrong.insert(OWNER.into(), keys[READER]);
        assert!(authorize_lines(PROJECT, &lines, &wrong, 100).is_err());
        assert!(authorize_lines(READER, &lines, &keys, 100).is_err());
    }

    #[test]
    fn authorization_retains_unknown_fields_and_exact_signed_whitespace() {
        let (mut lines, keys) = history();
        let key = SigningKey::from_bytes(&[1; 32]);
        let body = format!(
            "{{ \"future\": true, {} }}",
            &parse_line(&lines[1]).unwrap().get()[1..parse_line(&lines[1]).unwrap().get().len() - 1]
        );
        let sig = layr_store::keys::sign_bytes(body.as_bytes(), &key);
        lines[1] = format!("{{\"r\":{body},\"s\":\"{sig}\"}}");
        let checked = authorize_lines(PROJECT, &lines, &keys, 100).unwrap();
        let creation = checked.iter().find(|r| r.record().op == "project").unwrap();
        assert_eq!(creation.signed().get(), body);
        assert_eq!(creation.signed().line(), lines[1]);
        assert!(
            authorize_lines(PROJECT, &lines.iter().map(|l| l.replace("true", "false")).collect::<Vec<_>>(), &keys, 100).is_err()
        );
    }
    #[test]
    fn brokers_cannot_authorize_trust_outside_their_delegation() {
        let owner = SigningKey::from_bytes(&[1; 32]);
        let broker = SigningKey::from_bytes(&[2; 32]);
        let (mut lines, keys) = history();
        lines.truncate(2);
        lines.push(line(
            OWNER,
            3,
            "machine.trust",
            json!({"machine":READER,"public_key":hex::encode(broker.verifying_key().to_bytes())}),
            &owner,
        ));
        lines.push(line(OWNER, 4, "trust.delegate", json!({"machine":READER,"max_hours":1,"role":"reader"}), &owner));
        lines.push(line(READER, 1, "machine", json!({"public_key":hex::encode(broker.verifying_key().to_bytes())}), &broker));
        let target = "01a12290-0000-7000-8000-000000000099";
        let mut trust: Record = serde_json::from_str(
            parse_line(&line(
                READER,
                2,
                "machine.trust",
                json!({
                    "machine":target,"public_key":hex::encode(owner.verifying_key().to_bytes())
                }),
                &broker,
            ))
            .unwrap()
            .get(),
        )
        .unwrap();
        trust.time = 5;
        let body = serde_json::to_string(&trust).unwrap();
        let signature = layr_store::keys::sign_bytes(body.as_bytes(), &broker);
        lines.push(format!("{{\"r\":{body},\"s\":\"{signature}\"}}"));
        let authorized = authorize_lines(PROJECT, &lines, &keys, 10).unwrap();
        assert!(!authorized.iter().any(|r| r.record().machine == READER && r.record().op == "machine.trust"));
        let model = Model::from_verified_lines(authorized, OWNER).unwrap();
        assert!(!model.projects[PROJECT].trusted.contains_key(target));
    }

    #[test]
    fn denied_records_keep_raw_watermarks_without_entering_index_or_fallback_models() {
        use layr_store::index::{Data, Index};
        let (lines, keys) = history();
        let model = Model::from_verified_lines(authorize_lines(PROJECT, &lines, &keys, 100).unwrap(), "").unwrap();
        let snapshot = Index {
            v: 1,
            project: PROJECT.into(),
            machine: OWNER.into(),
            data: Data::Heads(
                crate::index::heads(&model.projects[PROJECT], crate::index::watermark(&lines).unwrap(), 1).unwrap(),
            ),
        };
        if let Data::Heads(heads) = &snapshot.data {
            assert_eq!(heads.counts.records, 4);
            assert_eq!(heads.watermark[READER].seq, 2);
        }
        let reconciled = crate::index::reconcile(&snapshot, &lines, &keys, 100).unwrap();
        let fallback = crate::index::load_or_full(Err(anyhow::anyhow!("invalid snapshot")), PROJECT, &lines, &keys, 100).unwrap();
        assert!(!fallback.used_index);
        assert_eq!(
            crate::index::heads(&fallback.project, crate::index::watermark(&lines).unwrap(), 1).unwrap(),
            crate::index::heads(&reconciled, crate::index::watermark(&lines).unwrap(), 1).unwrap()
        );
        assert_eq!(
            crate::index::heads(&fallback.project, crate::index::watermark(&lines).unwrap(), 1).unwrap().counts.records,
            4
        );
    }
    #[test]
    fn projectless_bootstrap_stays_in_its_authorized_project() {
        let other_project = "01a12290-0000-7000-8000-000000000010";
        let mut authorized = Vec::new();
        for (project, machine, seed, legacy) in [(PROJECT, OWNER, 1, true), (other_project, READER, 2, false)] {
            let key = SigningKey::from_bytes(&[seed; 32]);
            let keys = BTreeMap::from([(machine.to_string(), key.verifying_key())]);
            let rules = Default::default();
            let mut lines = Vec::new();
            for (seq, op) in [(1, "machine"), (2, "project")] {
                let mut record = Record::untrusted_preview(op);
                record.id = format!("01a12290-0000-7000-8001-{:012}", u64::from(seed) * 100 + seq);
                record.project = if legacy && seq == 1 { "" } else { project }.into();
                record.machine = machine.into();
                record.seq = seq;
                record.data = if op == "project" {
                    json!({"id":project})
                } else {
                    json!({"public_key":hex::encode(key.verifying_key().to_bytes())})
                };
                let body = serde_json::to_string(&record).unwrap();
                let signature = layr_store::keys::sign_bytes(body.as_bytes(), &key);
                lines.push(format!("{{\"r\":{body},\"s\":\"{signature}\"}}"));
            }
            authorized.extend(
                authorize_local_lines(
                    project,
                    &lines,
                    &keys,
                    LocalAuthority {
                        machine: MachineId::parse(machine).unwrap(),
                        trust_writers: &keys,
                        limits: &rules,
                        overlay: None,
                    },
                    100,
                )
                .unwrap(),
            );
        }
        let model = Model::from_verified_lines(authorized, READER).unwrap();
        assert!(model.projects[PROJECT].machines.contains_key(OWNER));
        assert!(!model.projects[other_project].machines.contains_key(OWNER));
        assert!(model.projects[other_project].machines.contains_key(READER));
    }
}
