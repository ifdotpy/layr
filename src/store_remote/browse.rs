//! Read store trees without a local project or snapshot backend.

pub(crate) use super::format::Entry;
use super::{backend::Backend, keys, reader, Descriptor, StorePin};
use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use std::collections::{BTreeMap, HashMap};

#[derive(Default, Clone)]
pub struct Access {
    pub machine: Option<(String, SigningKey)>,
    pub paper: Option<String>,
    pub identity: Option<String>,
    pub pin: Option<StorePin>,
}

pub struct Line {
    pub name: String,
    pub head: String,
    pub kind: String,
    pub machine: String,
    pub time: i64,
    pub files: BTreeMap<String, Entry>,
}

pub struct Browse {
    backend: Backend,
    identity: String,
    keys: BTreeMap<String, ed25519_dalek::VerifyingKey>,
    pub lines: Vec<Line>,
    pub pin: StorePin,
}

impl Browse {
    pub fn open(url: &str, env: &HashMap<String, String>, access: &Access) -> Result<Self> {
        let stage = crate::trace::span("disk descriptor");
        let backend = Backend::for_reader(url, env)?;
        let descriptor: Descriptor = serde_json::from_slice(&backend.get("store.json")?.context("store.json is missing")?)?;
        if descriptor.format != 2 || !crate::model::is_uuid(&descriptor.project) {
            bail!("invalid store descriptor");
        }
        if let Some(pin) = &access.pin {
            pin.check(&descriptor)?;
        }
        let pin = StorePin { project: descriptor.project.clone(), recipient: descriptor.recipient.clone() };
        drop(stage);
        let stage = crate::trace::span("disk slot");
        let identity = if let Some(identity) = &access.identity {
            identity.clone()
        } else if let Some((machine, key)) = &access.machine {
            if let Some(slot) = backend.get(&format!("slots/machine-{machine}.age"))? {
                keys::open_machine_slot(&slot, key)?
            } else {
                paper_identity(&backend, access.paper.as_deref())?
            }
        } else {
            paper_identity(&backend, access.paper.as_deref())?
        };
        if keys::recipient_of(&identity)? != descriptor.recipient {
            bail!("the disk key does not match the store recipient");
        }
        drop(stage);
        let stage = crate::trace::span("disk records");
        backend.unlock_reader(&identity)?;
        let records = reader::records(&backend, &identity, &descriptor.project)?;
        if !records.gaps.is_empty() {
            bail!("the store has record gaps; fetch the complete record history before mounting");
        }
        drop(stage);
        let stage = crate::trace::span("disk metadata");
        // Derived tables exist only in memory. No local project is created.
        let db = crate::db::Db::open(std::path::Path::new(":memory:"))?;
        for line in &records.lines {
            db.derive(&crate::oplog::parse_line(line, None)?.record, &|_| false, "")?;
        }
        drop(stage);
        let stage = crate::trace::span("disk trees");
        let attempts = reader::attempts(&backend)?;
        let mut decoder = reader::Reader::new(&backend, &identity, &records.keys);
        let mut lines = Vec::new();
        for line in db.lines()? {
            let mut candidates = Vec::new();
            for (_, head) in db.line_heads(&line.id)? {
                if let Some(state) = db.state(&head)? {
                    candidates.push(state);
                }
            }
            candidates.extend(db.line_states(&line.id, crate::model::StateKind::Auto)?);
            candidates.sort_by(|a, b| (b.time, &b.id).cmp(&(a.time, &a.id)));
            let Some(state) = candidates.into_iter().find(|s| attempts.contains_key(&s.id)) else {
                continue;
            };
            let (_, _, tree) = decoder.tree(&state.id, &attempts[&state.id])?;
            let machine = db.state_machine(&state.id)?.context("state signer is missing")?;
            let kind = if state.kind == crate::model::StateKind::Auto { "auto" } else { "state" };
            lines.push(Line { name: line.name, head: state.id, kind: kind.into(), machine, time: state.time, files: tree.files });
        }
        drop(decoder);
        drop(stage);
        Ok(Self { backend, identity, keys: records.keys, lines, pin })
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn read(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let descriptor: Descriptor = serde_json::from_slice(&self.backend.get("store.json")?.context("store.json is missing")?)?;
        if descriptor.format != 2 {
            bail!("unsupported store format {}", descriptor.format);
        }
        self.pin.check(&descriptor)?;
        reader::Reader::new(&self.backend, &self.identity, &self.keys).read(entry)
    }
}

fn paper_identity(backend: &Backend, paper: Option<&str>) -> Result<String> {
    let paper = paper.context("this machine has no store slot; use --paper-key-file <0600-file> or --paper-key-stdin")?;
    let opener = keys::identity_from_paper(paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
    let slot = backend.get("slots/paper.age")?.context("the store has no paper slot")?;
    keys::open_slot(&slot, &opener)
}
