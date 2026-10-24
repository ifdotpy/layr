//! Read the golden fixture through the shared store reader and the pure model.
use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_model::{probes, Model, Record, StateKind, Subject};
use layr_store::{format, keys, reader, record};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
struct Request {
    line: String,
    change: String,
    #[serde(rename = "ref")]
    reference: String,
    attestation_state: String,
    probe_base: String,
    probe_head: String,
}

type Objects = BTreeMap<String, Vec<u8>>;
type DecodedTree = (Vec<u8>, format::Tree);

fn objects(root: &Path) -> Result<Objects> {
    fn visit(root: &Path, dir: &Path, out: &mut Objects) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(root, &entry.path(), out)?;
            } else {
                ensure!(kind.is_file(), "store contains a non-file object");
                let path = entry.path();
                let name = path.strip_prefix(root)?.to_str().context("non-UTF-8 store path")?.replace('\\', "/");
                out.insert(name, fs::read(path)?);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

fn get<'a>(objects: &'a Objects, name: &str) -> Result<&'a [u8]> {
    objects.get(name).map(Vec::as_slice).with_context(|| format!("missing store object {name}"))
}

/// Check segment addresses, signatures, sequence continuity, and project bindings.
fn records(
    objects: &Objects,
    identity: &str,
    project: &str,
    trusted: Option<&BTreeMap<String, VerifyingKey>>,
) -> Result<(Vec<String>, BTreeMap<String, VerifyingKey>)> {
    let mut machines: BTreeMap<String, BTreeMap<u64, String>> = BTreeMap::new();
    for (name, bytes) in objects.iter().filter(|(name, _)| name.starts_with("records/")) {
        let (machine, first, last) = format::parse_segment_name(name).context("invalid segment name")?;
        let plain = String::from_utf8(keys::decrypt(identity, bytes)?)?;
        let mut expected = first;
        for line in plain.lines() {
            let raw = record::parse_line(line, None)?;
            let rec: Record = serde_json::from_str(raw.get())?;
            ensure!(rec.machine == machine && rec.seq == expected && expected <= last, "invalid segment sequence");
            ensure!(rec.project == project, "record belongs to another project");
            let by_seq = machines.entry(machine.clone()).or_default();
            if let Some(old) = by_seq.insert(expected, line.into()) {
                ensure!(old == line, "conflicting record segment");
            }
            expected = expected.checked_add(1).context("record sequence overflow")?;
        }
        ensure!(expected.checked_sub(1) == Some(last), "incomplete segment");
    }
    ensure!(!machines.is_empty(), "store has no records");
    let mut verified = Vec::new();
    let mut public_keys = BTreeMap::new();
    for (machine, by_seq) in machines {
        let first = by_seq.get(&1).context("missing machine announcement")?;
        let raw = record::parse_line(first, None)?;
        let rec: Record = serde_json::from_str(raw.get())?;
        ensure!(rec.op == "machine", "first record is not a machine announcement");
        let key = record::machine_key(raw)?;
        if let Some(trusted) = trusted {
            ensure!(trusted.get(&machine) == Some(&key), "overlay machine is not trusted");
        }
        for (index, (seq, line)) in by_seq.into_iter().enumerate() {
            ensure!(seq == index as u64 + 1, "record history has a gap");
            record::parse_line(&line, Some(&key))?;
            verified.push(line);
        }
        public_keys.insert(machine, key);
    }
    Ok((verified, public_keys))
}

struct Store {
    objects: Objects,
    identity: String,
    keys: BTreeMap<String, VerifyingKey>,
    trees: BTreeMap<(String, String), DecodedTree>,
    pending: BTreeSet<(String, String)>,
}

impl Store {
    fn tree(&mut self, state: &str, attempt: &str) -> Result<DecodedTree> {
        let address = (state.to_string(), attempt.to_string());
        if let Some(tree) = self.trees.get(&address) {
            return Ok(tree.clone());
        }
        ensure!(self.pending.len() < format::FULL_TREE_EVERY, "tree chain exceeds the store limit");
        ensure!(self.pending.insert(address.clone()), "tree chain has a cycle");
        let object = get(&self.objects, &format::tree_name(state, attempt))?.to_vec();
        let plain = keys::decrypt(&self.identity, &object)?;
        let (header, _) = format::split_tree(&plain)?;
        ensure!(header.state == state && header.attempt == attempt, "tree address differs from its header");
        let base = match &header.base {
            Some((state, attempt)) => Some(self.tree(state, attempt)?.0),
            None => None,
        };
        let key = self.keys.get(&header.machine).context("unknown tree signer")?;
        let (_, canonical, tree) = reader::tree(&self.identity, &object, base.as_deref(), key)?;
        self.pending.remove(&address);
        self.trees.insert(address, (canonical.clone(), tree.clone()));
        Ok((canonical, tree))
    }

    fn states(&mut self) -> Result<BTreeMap<String, BTreeMap<String, Vec<u8>>>> {
        let mut attempts = BTreeMap::new();
        for name in self.objects.keys().filter(|name| name.starts_with("states/") && name.ends_with(".tree.age")) {
            let (state, attempt) = format::parse_tree_name(name).context("invalid tree name")?;
            attempts.entry(state).or_insert(attempt);
        }
        let mut states = BTreeMap::new();
        for (state, attempt) in attempts {
            let (_, tree) = self.tree(&state, &attempt)?;
            let mut files = BTreeMap::new();
            for (path, entry) in tree.files {
                files.insert(path, reader::file(&self.identity, &entry, &self.objects)?);
            }
            states.insert(state, files);
        }
        Ok(states)
    }
}

fn store(objects: Objects, identity: String, keys: BTreeMap<String, VerifyingKey>) -> Store {
    Store { objects, identity, keys, trees: BTreeMap::new(), pending: BTreeSet::new() }
}

fn write(out: &Path, name: &str, value: impl serde::Serialize) -> Result<()> {
    fs::write(out.join(format!("{name}.json")), serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    ensure!(args.len() == 3, "usage: store_golden <store dir> <paper key file> <out dir>");
    let request: Request = serde_json::from_slice(&fs::read(args[2].join("request.json"))?)?;
    let objects = objects(&args[0])?;
    let descriptor: format::Descriptor = serde_json::from_slice(get(&objects, "store.json")?)?;
    ensure!(descriptor.format == 2 && format::is_uuid(&descriptor.project), "invalid store descriptor");
    let paper = keys::identity_from_paper(&fs::read_to_string(&args[1])?)?;
    let identity = keys::open_slot(get(&objects, "slots/paper.age")?, &paper)?;
    ensure!(keys::recipient_of(&identity)? == descriptor.recipient, "paper slot differs from the store recipient");
    let (mut lines, public_keys) = records(&objects, &identity, &descriptor.project, None)?;
    ensure!(public_keys.len() == 1, "the golden fixture must use one machine");
    let machine = public_keys.keys().next().context("no machine")?.clone();

    // The fixture has one overlay epoch and one creator. Verify that creator's key.
    let overlay_keys: Vec<_> = objects
        .keys()
        .filter(|name| {
            let parts: Vec<_> = name.split('/').collect();
            parts.len() == 4 && parts[0] == "overlays" && parts[2] == "keys" && name.ends_with(".json")
        })
        .cloned()
        .collect();
    ensure!(overlay_keys.len() == 1, "the fixture must publish one overlay key");
    let mut overlay_files = BTreeMap::new();
    let mut overlay_project = String::new();
    for name in overlay_keys {
        let key_line = std::str::from_utf8(get(&objects, &name)?)?.trim_end_matches('\n');
        let raw = record::parse_line(key_line, None)?;
        let key_record: Record = serde_json::from_str(raw.get())?;
        let key = public_keys.get(&key_record.machine).context("unknown overlay key signer")?;
        record::parse_line(key_line, Some(key))?;
        ensure!(key_record.op == "overlay.key", "published key is not an overlay key");
        let data = &key_record.data;
        let overlay = data["overlay"].as_str().context("overlay key has no project")?;
        let epoch = data["epoch"].as_str().context("overlay key has no epoch")?;
        let layer_name = data["name"].as_str().context("overlay key has no name")?;
        ensure!(format::is_uuid(overlay) && format::is_uuid(epoch), "invalid overlay identity");
        ensure!(
            key_record.project == overlay && data["parent"] == descriptor.project && data["store"] == descriptor.recipient,
            "overlay key has wrong store bindings"
        );
        ensure!(name == format!("overlays/{overlay}/keys/{epoch}-{}.json", key_record.id), "overlay key has wrong object name");
        let created = lines
            .iter()
            .map(|line| {
                let raw = record::parse_line(line, None)?;
                Ok(serde_json::from_str::<Record>(raw.get())?)
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .any(|rec| {
                rec.op == "overlay.create"
                    && rec.machine == key_record.machine
                    && rec.data["name"] == layer_name
                    && rec.data["config"][format!("overlay.{layer_name}")]["id"] == overlay
            });
        ensure!(created, "overlay key signer does not match its creator");
        let prefix = format!("overlays/{overlay}/epochs/{epoch}/");
        let scoped: Objects = objects
            .iter()
            .filter_map(|(name, bytes)| name.strip_prefix(&prefix).map(|name| (name.into(), bytes.clone())))
            .collect();
        let layer_identity = keys::open_slot(get(&scoped, "slots/paper.age")?, &paper)?;
        ensure!(
            keys::recipient_of(&layer_identity)? == data["recipient"].as_str().context("no overlay recipient")?,
            "overlay slot has wrong recipient"
        );
        let (layer_lines, layer_keys) = records(&scoped, &layer_identity, overlay, Some(&public_keys))?;
        ensure!(layer_lines.iter().any(|line| line == key_line), "overlay log does not contain its key");
        overlay_files = store(scoped, layer_identity, layer_keys).states()?;
        overlay_project = overlay.into();
        lines.extend(layer_lines);
    }

    let model = Model::from_verified_lines(lines.iter().map(String::as_str), &machine)?;
    let project = model.projects.get(&descriptor.project).context("store project is absent from the model")?;
    let overlay = model.projects.get(&overlay_project).context("overlay is absent from the model")?;
    ensure!(
        overlay
            .overlay_setting("access")
            .and_then(Value::as_object)
            .is_some_and(|access| access.values().any(|role| role == "reader")),
        "overlay model has no reader"
    );
    ensure!(
        overlay_files.values().any(|files| files.get("private.txt").is_some_and(|bytes| bytes == b"private bytes\n")),
        "overlay file was not decrypted"
    );

    let files = store(objects, identity, public_keys).states()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?;
    let mut context = probes::Context { now, ..Default::default() };
    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        probe: BTreeMap<String, probes::Probe>,
    }
    for (id, state) in &project.states {
        if let Some(files) = files.get(id) {
            context.sources.insert(id.clone(), "available".into());
            if state.state.kind == StateKind::Derived {
                context.outputs.insert(id.clone(), files.clone());
            }
            if let Some(manifest) = files.get("layr.toml") {
                let manifest: Manifest = toml::from_str(std::str::from_utf8(manifest)?)?;
                context.definitions.insert(id.clone(), manifest.probe);
            }
        } else {
            context.sources.insert(id.clone(), "source unavailable".into());
        }
    }
    for id in files.keys() {
        ensure!(project.states.contains_key(id), "store tree has no state record");
    }
    let line = project.line_list().into_iter().find(|line| line.name == request.line).context("requested line is absent")?;
    ensure!(project.change_versions(&request.change).len() == 3, "fixture change must have three versions");
    let subject = |id: String| Subject { kind: "state".into(), id };
    fs::create_dir_all(&args[2])?;
    write(&args[2], "lines", project.line_list())?;
    write(&args[2], "log", project.log(&[line.head.clone()], None, usize::MAX))?;
    write(&args[2], "log-auto", project.log(&[line.head.clone()], Some(&line.id), usize::MAX))?;
    write(&args[2], "change", project.change_show(&request.change, &context)?)?;
    write(&args[2], "refs", project.ref_list("", now))?;
    write(&args[2], "ref-log", project.ref_log(&request.reference))?;
    write(&args[2], "queue", project.queue_list(None))?;
    write(&args[2], "attestations", project.attestations(&request.attestation_state, None))?;
    write(&args[2], "operations", project.op_log(Some(&line.id), 30))?;
    write(&args[2], "probe-diff", probes::diff(project, &subject(request.probe_base), &subject(request.probe_head), &context)?)?;
    if project.line_list().len() != 2 {
        bail!("fixture must have two lines");
    }
    Ok(())
}
