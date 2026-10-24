//! Read store trees without a local project or snapshot backend.

#[derive(Clone)]
pub struct Entry {
    inner: super::format::Entry,
    layer: Option<usize>,
}
impl std::ops::Deref for Entry {
    type Target = super::format::Entry;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
struct LayerReader {
    backend: Backend,
    identity: String,
    keys: BTreeMap<String, ed25519_dalek::VerifyingKey>,
    attempts: BTreeMap<String, String>,
}
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
    layers: Vec<LayerReader>,
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
        let login = if env.contains_key("LAYR_PERSON_KEY_FILE") {
            match &backend {
                Backend::Gateway(g) => Some(crate::person::gateway_login(g)?),
                _ => None,
            }
        } else {
            None
        };
        let uid = rustix::process::getuid().as_raw();
        let mut person =
            crate::person::load(env, &crate::ctx::Caller::from_uid(uid).home, &std::env::current_dir()?, uid, login.as_deref())?;
        if let (Backend::Gateway(gateway), Some(person)) = (&backend, &mut person) {
            crate::person::gateway_access(gateway, person)?;
        }
        if let Some(person) = &person {
            person.check(&descriptor)?;
        }
        let pin =
            StorePin { project: descriptor.project.clone(), recipient: descriptor.recipient.clone(), paper_recipient: None };
        drop(stage);
        let stage = crate::trace::span("disk slot");
        let identity = if let Some(identity) = &access.identity {
            identity.clone()
        } else if let Some((machine, key)) = &access.machine {
            if let Some(slot) = backend.get(&format!("slots/machine-{machine}.age"))? {
                keys::open_machine_slot(&slot, key)?
            } else {
                if let Some(person) = &person {
                    person.open(&backend, &descriptor)?
                } else {
                    paper_identity(&backend, access.paper.as_deref())?
                }
            }
        } else {
            if let Some(person) = &person {
                person.open(&backend, &descriptor)?
            } else {
                paper_identity(&backend, access.paper.as_deref())?
            }
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
        let mut layers = Vec::new();
        let mut layer_names = BTreeMap::new();
        for (name, published) in super::overlays::published(&backend, &descriptor.project, &descriptor.recipient, &records.keys)?
        {
            let scoped = backend.scoped(&published.prefix())?;
            let machine = access.machine.as_ref().map(|(id, key)| (id.as_str(), key));
            let Some(identity) =
                super::overlays::open_identity(&scoped, &published.key, machine, person.as_ref(), access.paper.as_deref())?
            else {
                continue;
            };
            let overlay_records = reader::records(&scoped, &identity, &published.key.overlay)?;
            if !overlay_records.lines.contains(&published.line) {
                bail!("published overlay key is absent from the overlay log");
            }
            for (machine, key) in &overlay_records.keys {
                if records.keys.get(machine) != Some(key) {
                    bail!("overlay tree signer is not trusted");
                }
            }
            layer_names.insert(name, layers.len());
            let attempts = reader::attempts(&scoped)?;
            layers.push(LayerReader { backend: scoped, identity, keys: overlay_records.keys, attempts });
        }
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
            let mut files: BTreeMap<_, _> =
                tree.files.into_iter().map(|(path, inner)| (path, Entry { inner, layer: None })).collect();
            // Stack settings are per machine. Use the stack of the selected line head's machine.
            let stack = db.config(&format!("stack.{}@{}", line.id, machine))?.or(db.config(&format!("stack.{}", line.id))?);
            let stack: Vec<crate::overlay::Layer> = stack.map(serde_json::from_value).transpose()?.unwrap_or_default();
            for layer in stack {
                let Some(&index) = layer_names.get(&layer.overlay) else { continue };
                let source = &layers[index];
                let Some(attempt) = source.attempts.get(&layer.state) else { continue };
                let (_, _, tree) =
                    reader::Reader::new(&source.backend, &source.identity, &source.keys).tree(&layer.state, attempt)?;
                for (path, inner) in tree.files {
                    // A higher layer replaces a lower file or directory at the same path.
                    files.retain(|old, _| !old.starts_with(&format!("{path}/")) && !path.starts_with(&format!("{old}/")));
                    files.insert(path, Entry { inner, layer: Some(index) });
                }
            }
            lines.push(Line { name: line.name, head: state.id, kind: kind.into(), machine, time: state.time, files });
        }
        drop(decoder);
        drop(stage);
        Ok(Self { backend, identity, keys: records.keys, lines, pin, layers })
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
        if let Some(index) = entry.layer {
            let layer = self.layers.get(index).context("unknown overlay source")?;
            reader::Reader::new(&layer.backend, &layer.identity, &layer.keys).read(&entry.inner)
        } else {
            reader::Reader::new(&self.backend, &self.identity, &self.keys).read(&entry.inner)
        }
    }
}

fn paper_identity(backend: &Backend, paper: Option<&str>) -> Result<String> {
    let paper = paper.context("this machine has no store slot; use --paper-key-file <0600-file> or --paper-key-stdin")?;
    let opener = keys::identity_from_paper(paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
    let slot = backend.get("slots/paper.age")?.context("the store has no paper slot")?;
    keys::open_slot(&slot, &opener)
}
