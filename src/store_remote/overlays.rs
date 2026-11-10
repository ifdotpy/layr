//! Overlay epochs reuse the immutable v2 writer and reader with separate age identities.
use super::{backend::Backend, keys, reader, writer, Options, Report, Store};
use crate::{ctx::Ctx, model::Record, oplog, repo::Repo, store::Project};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct LayerAccess {
    pub recipient: String,
    pub identity: Option<String>,
    /// The chunk key of a cache layer whose identity this machine holds (`layr_store::chunks::key`):
    /// its files go up as content-defined chunks. Without it they go up as frames.
    pub chunk_key: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Key {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    pub overlay: String,
    pub name: String,
    pub epoch: String,
    pub parent: String,
    pub store: String,
    pub recipient: String,
    pub readers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation: Option<String>,
    #[serde(default)]
    pub access_mode: layr_store::epochs::Mode,
}

#[derive(Clone)]
pub(crate) struct Published {
    pub key: Key,
    pub record: Record,
    pub line: String,
}

impl Published {
    pub fn prefix(&self) -> String {
        format!("overlays/{}/epochs/{}", self.key.overlay, self.key.epoch)
    }
    fn object(&self) -> String {
        format!(
            "overlays/{}/keys/{}-{}.{}",
            self.key.overlay,
            self.key.epoch,
            self.record.id,
            if self.key.hidden { "age" } else { "json" }
        )
    }
    fn order(&self) -> (i64, &str, u64) {
        (self.record.time, &self.record.machine, self.record.seq)
    }
}

/// The cache states of the cache overlay `orepo` that `line new X <line>` takes for each of `lines`, from the records
/// alone (the states are not here yet): the newest cache of the line's own overlay line, and the newest cache of this
/// platform saved with its head or else with its nearest ancestor that has one (`cache_source`).
fn caches_of_lines(repo: &Repo, orepo: &Repo, lines: &[String]) -> Result<BTreeSet<String>> {
    let mut saved_with: BTreeMap<String, String> = BTreeMap::new();
    for (id, of) in orepo.p.db.cache_states_known()? {
        // A cache of another platform stays on the remote: a newer one (an x86 runner's after an arm runner's) must
        // not hide this platform's cache of the same state.
        if orepo.p.db.state(&id)?.is_some_and(|st| crate::cache_overlay::other_platform(st.cache_platform.as_deref())) {
            continue;
        }
        // A save with no named change saves with an automatic state over the head: it counts for the head.
        if let Some(st) = repo.p.db.state(&of)? {
            if st.kind == crate::model::StateKind::Auto {
                if let Some(parent) = st.parents.first() {
                    saved_with.entry(parent.clone()).or_insert_with(|| id.clone());
                }
            }
        }
        saved_with.entry(of).or_insert(id);
    }
    let mut out = BTreeSet::new();
    for name in lines {
        let line = repo.line(name).with_context(|| format!("--caches-of: no line {name}"))?;
        if let Some(own) = orepo.p.db.line_by_name(&crate::overlay::cache_line_name(&line.id))? {
            out.insert(own.head);
        }
        // Every machine's head of the line: a check runner's own main can be an old head while a server
        // queue moved main on its merge machine, and the runner checks states made from that newer head.
        let mut heads = vec![line.head.clone()];
        for (_, head) in repo.p.db.line_heads(&line.id)? {
            if !heads.contains(&head) {
                heads.push(head);
            }
        }
        for head in heads {
            let mut seen = BTreeSet::new();
            let mut queue = std::collections::VecDeque::from([head]);
            while let Some(id) = queue.pop_front() {
                if seen.len() >= 200 || !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(cache) = saved_with.get(&id) {
                    out.insert(cache.clone());
                    break;
                }
                if let Some(state) = repo.p.db.state(&id)? {
                    queue.extend(state.parents);
                }
            }
        }
    }
    Ok(out)
}

/// The signers of `keys` that `trusted` does not name yet. A signer that it names with another key is an error.
fn not_trusted_yet(
    keys: &BTreeMap<String, VerifyingKey>,
    trusted: &BTreeMap<String, VerifyingKey>,
) -> std::result::Result<BTreeSet<String>, String> {
    let mut waiting = BTreeSet::new();
    for (machine, key) in keys {
        match trusted.get(machine) {
            Some(known) if known == key => {}
            Some(_) => return Err(machine.clone()),
            None => {
                waiting.insert(machine.clone());
            }
        }
    }
    Ok(waiting)
}

pub(crate) fn trusted_keys(repo: &Repo) -> Result<BTreeMap<String, VerifyingKey>> {
    let mut keys = BTreeMap::new();
    keys.insert(repo.p.store.machine.id.clone(), repo.p.store.machine.key.verifying_key());
    let mut query = repo.p.db.conn.prepare("SELECT machine,public_key FROM trusted")?;
    let trusted = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for entry in trusted {
        let (machine, public) = entry?;
        let bytes: [u8; 32] = hex::decode(public)?.try_into().map_err(|_| anyhow::anyhow!("invalid trusted key"))?;
        keys.insert(machine, VerifyingKey::from_bytes(&bytes)?);
    }
    Ok(keys)
}

fn parse(line: &str, parent: &str, recipient: &str, trusted: &BTreeMap<String, VerifyingKey>) -> Result<Published> {
    let record = oplog::parse_line(line, None)?.record;
    let signer = trusted.get(&record.machine).context("overlay key signer is not trusted")?;
    let record = oplog::parse_line(line, Some(signer))?.record;
    crate::model::validate(&record)?;
    if record.op != "overlay.key" {
        bail!("published overlay key is not an overlay.key record");
    }
    let key: Key = serde_json::from_value(record.data.clone()).context("invalid overlay key record")?;
    crate::store::check_name("overlay", &key.name)?;
    if key.parent != parent
        || key.store != recipient
        || key.overlay != record.project
        || !crate::model::is_uuid(&key.overlay)
        || !crate::model::is_uuid(&key.epoch)
        || !keys::valid_recipient(&key.recipient)
    {
        bail!("overlay key does not match its project, store or epoch");
    }
    Ok(Published { key, record, line: line.into() })
}

pub(crate) fn published(
    backend: &Backend,
    parent: &str,
    recipient: &str,
    trusted: &BTreeMap<String, VerifyingKey>,
    authorities: &BTreeMap<String, crate::overlay_managers::Authority>,
) -> Result<(BTreeMap<String, Published>, Vec<String>)> {
    published_objects(backend, parent, recipient, trusted, authorities, backend.list("overlays/")?)
}
fn published_objects(
    backend: &Backend,
    parent: &str,
    recipient: &str,
    trusted: &BTreeMap<String, VerifyingKey>,
    authorities: &BTreeMap<String, crate::overlay_managers::Authority>,
    objects: Vec<super::backend::Object>,
) -> Result<(BTreeMap<String, Published>, Vec<String>)> {
    let mut found: BTreeMap<String, Published> = BTreeMap::new();
    let mut refused = Vec::new();
    for object in objects {
        let parts: Vec<_> = object.key.split('/').collect();
        if parts.len() != 4 || parts[2] != "keys" || !parts[3].ends_with(".json") {
            continue;
        }
        let bytes = backend.get(&object.key)?.context("overlay key disappeared")?;
        let line = std::str::from_utf8(&bytes)?.trim_end_matches('\n');
        let next = parse(line, parent, recipient, trusted)?;
        if next.key.hidden {
            bail!("hidden overlay key was published without encryption");
        }
        if next.object() != object.key {
            bail!("overlay key differs from its object name");
        }
        let authority = authorities.get(&next.key.name).context("overlay key has no creator authority")?;
        if !authority.permits(&next.record, &next.line) {
            refused.push(format!(
                "overlay {}: refused overlay.key signer {} (not a key manager)",
                next.key.name, next.record.machine
            ));
            continue;
        }
        if let Some(old) = found.get(&next.key.name) {
            if old.key.overlay != next.key.overlay {
                bail!("conflicting overlay identities for {}", next.key.name);
            }
            if old.order() >= next.order() {
                continue;
            }
        }
        found.insert(next.key.name.clone(), next);
    }
    Ok((found, refused))
}

/// Discover encrypted manifests only after a caller's reader slot opens.
/// No name or count is returned for objects the caller cannot open.
pub(crate) fn discover_hidden(
    backend: &Backend,
    parent: &str,
    recipient: &str,
    trusted: &BTreeMap<String, VerifyingKey>,
    machine: Option<(&str, &ed25519_dalek::SigningKey)>,
    person: Option<&crate::person::Access>,
    paper: Option<&str>,
    cached: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, (Published, String, crate::overlay_managers::Authority)>> {
    let mut found: BTreeMap<String, (Published, String, crate::overlay_managers::Authority)> = BTreeMap::new();
    let objects = backend.list("overlays/")?;
    for object in objects {
        let parts: Vec<_> = object.key.split('/').collect();
        if parts.len() != 4 || parts[2] != "keys" || !parts[3].ends_with(".age") {
            continue;
        }
        let Some(epoch) = parts[3].get(..36).filter(|_| parts[3].as_bytes().get(36) == Some(&b'-')) else {
            continue;
        };
        if !crate::model::is_uuid(parts[1]) || !crate::model::is_uuid(epoch) {
            continue;
        }
        let prefix = format!("overlays/{}/epochs/{epoch}", parts[1]);
        let scoped = backend.scoped(&prefix)?;
        let mut identity = None;
        if let Some((machine, key)) = machine {
            if let Some(slot) = scoped.get(&format!("slots/machine-{machine}.age"))? {
                identity = keys::open_machine_slot(&slot, key).ok();
            }
        }
        if identity.is_none() {
            if let Some(person) = person {
                let slot_name = layr_store::person::slot_name(&person.login, &keys::recipient_of(&person.identity)?)?;
                if let Some(slot) = scoped.get(&format!("slots/{slot_name}"))? {
                    identity = keys::open_slot(&slot, &person.identity).ok();
                }
            }
        }
        if identity.is_none() {
            if let Some(paper) = paper {
                if let Some(slot) = scoped.get("slots/paper.age")? {
                    let opener = keys::identity_from_paper(paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
                    identity = keys::open_slot(&slot, &opener).ok();
                }
            }
        }
        let cache_key = format!("{parent}/{recipient}/{prefix}");
        let Some(identity) = identity.or_else(|| cached.get(&cache_key).cloned()) else {
            continue;
        };
        let body = backend.get(&object.key)?.context("hidden overlay manifest disappeared")?;
        let line = String::from_utf8(keys::decrypt(&identity, &body)?)?;
        let published = parse(&line, parent, recipient, trusted)?;
        if !published.key.hidden || published.object() != object.key || keys::recipient_of(&identity)? != published.key.recipient
        {
            bail!("hidden overlay manifest differs from its slot or object");
        }
        let records = reader::records(&scoped, &identity, &published.key.overlay)?;
        if !records.lines.contains(&line) {
            bail!("hidden overlay key is absent from its own log");
        }
        let (name, authority) =
            layr_model::authority::from_overlay_lines(parent, &published.key.overlay, &records.lines, trusted)?;
        if name != published.key.name || !authority.permits(&published.record, &line) {
            bail!("hidden overlay key has no creator authority");
        }
        if found.get(&name).is_some_and(|(old, _, _)| old.order() >= published.order()) {
            continue;
        }
        found.insert(name, (published, identity, authority));
    }
    Ok(found)
}

pub(crate) fn open_identity(
    backend: &Backend,
    key: &Key,
    machine: Option<(&str, &ed25519_dalek::SigningKey)>,
    person: Option<&crate::person::Access>,
    paper: Option<&str>,
) -> Result<Option<String>> {
    let checked = |identity: String| -> Result<String> {
        if keys::recipient_of(&identity)? != key.recipient {
            bail!("overlay slot does not match its signed recipient");
        }
        Ok(identity)
    };
    if let Some((machine, signing)) = machine {
        if key.readers.contains_key(&format!("machine:{machine}")) {
            if let Some(slot) = backend.get(&format!("slots/machine-{machine}.age"))? {
                if let Ok(identity) = keys::open_machine_slot(&slot, signing) {
                    return Ok(Some(checked(identity)?));
                }
            }
        }
    }
    if let Some(person) = person {
        let recipient = keys::recipient_of(&person.identity)?;
        if key.readers.contains_key(&format!("person:{}={recipient}", person.login)) {
            let slot = format!("slots/{}", layr_store::person::slot_name(&person.login, &recipient)?);
            if let Some(slot) = backend.get(&slot)? {
                if let Ok(identity) = keys::open_slot(&slot, &person.identity) {
                    return Ok(Some(checked(identity)?));
                }
            }
        }
    }
    if let Some(paper) = paper {
        let opener = keys::identity_from_paper(paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
        if let Some(slot) = backend.get("slots/paper.age")? {
            if let Ok(identity) = keys::open_slot(&slot, &opener) {
                return Ok(Some(checked(identity)?));
            }
        }
    }
    if !key.hidden && key.access_mode == layr_store::epochs::Mode::Public {
        let descriptor = super::Descriptor { format: 2, project: key.overlay.clone(), recipient: key.recipient.clone(), mode: Default::default() };
        let manifest = layr_store::public::Manifest::parse(
            &backend.get("public.json")?.context("public overlay has no manifest")?,
            &descriptor,
        )?;
        let slot = backend.get("slots/public.age")?.context("public overlay has no slot")?;
        return Ok(Some(checked(manifest.open(&slot)?)?));
    }
    Ok(None)
}

fn layer(store: &Store, published: &Published, identity: Option<String>, cache: bool) -> Result<Store> {
    let chunk_key = identity.as_deref().filter(|_| cache).map(|identity| layr_store::chunks::key(identity, &published.key.epoch));
    Ok(Store {
        backend: store.backend.scoped(&published.prefix())?,
        url: store.url.clone(),
        project: published.key.overlay.clone(),
        machine: store.machine.clone(),
        key: store.key.clone(),
        dir: store.dir.join("overlays").join(&published.key.overlay).join(&published.key.epoch),
        paper_key_text: None,
        person: None,
        pin: None,
        aws_profile: store.aws_profile.clone(),
        force_public: false,
        public_clone: false,
        public_reader: std::sync::atomic::AtomicBool::new(false),
        mode: None,
        epoch: None,
        epoch_public: false,
        publishing_epoch: false,
        layer: Some(LayerAccess { recipient: published.key.recipient.clone(), identity, chunk_key }),
    })
}

fn merge(report: &mut Report, next: Report, name: &str, direction: &str) {
    if next.records > 0 || next.states > 0 {
        report.overlays.push(format!("{name}: {} records, {} states", next.records, next.states));
    }
    report.records += next.records;
    report.states += next.states;
    if direction == "push" {
        report.overlay_transfers.push((
            name.into(),
            crate::model::Transfer {
                direction: direction.into(),
                records_sent: next.records,
                states_sent: next.states,
                bytes_sent: next.bytes,
                state_ids_sent: next.state_ids,
                ..Default::default()
            },
        ));
    }
    report.files += next.files;
    report.bytes += next.bytes;
    report.overlaps += next.overlaps;
    report.gaps.extend(next.gaps.into_iter().map(|gap| format!("overlay {name}: {gap}")));
    report.omitted.extend(next.omitted.into_iter().map(|item| format!("overlay {name}: {item}")));
}

fn local_key(store: &Store, id: &str, epoch: &str) -> std::path::PathBuf {
    store.dir.join("overlay-keys").join(id).join(format!("{epoch}.age"))
}

pub(super) fn push(store: &Store, ctx: &Ctx, repo: &Repo, opts: &Options, report: &mut Report) -> Result<()> {
    let descriptor = store.root_descriptor()?;
    let trusted = trusted_keys(repo)?;
    let mut objects = Vec::new();
    for name in repo.visible_overlay_names(ctx)? {
        if let Ok(overlay) = repo.overlay_repo(&name) {
            objects.extend(store.backend.list(&format!("overlays/{}/keys/", overlay.p.id))?);
        }
    }
    let parent_lines: Vec<_> = repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let mut authorities = crate::overlay_managers::from_lines(&repo.p.id, &parent_lines, &trusted)?;
    let listed = key_epochs(&objects);
    let (remote, refused) =
        published_objects(&store.backend, &repo.p.id, &descriptor.recipient, &trusted, &authorities, objects)?;
    report.omitted.extend(refused);
    let paper_recipient = if let Some(paper) = &store.paper_key_text {
        Some(keys::recipient_of(&keys::identity_from_paper(paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?)?)
    } else {
        store.pin.as_ref().and_then(|pin| pin.paper_recipient.clone())
    };
    for name in repo.visible_overlay_names(ctx)? {
        // A machine trusted since the last cache save reads the caches from this push on, not only
        // after the next save.
        if repo.need_overlay(&name)?.cache {
            repo.sync_cache_readers(ctx, &name);
        }
        let orepo = match repo.overlay_repo(&name) {
            Ok(orepo) => orepo,
            Err(_) => {
                report.omitted.push(format!("overlay {name}: no key on this machine"));
                continue;
            }
        };
        // The overlay's project lock covers its key records only. The upload (minutes for the first push of a cache)
        // and the sweep run under the lock of the remote folder, so `line new` and `save --caches` do not wait.
        let lock = orepo.p.lock()?;
        let hidden = repo.need_overlay(&name)?.hidden;
        if !authorities.contains_key(&name) {
            authorities.insert(name.clone(), crate::overlay_managers::of(repo, &name)?);
        }
        let authority = authorities.get(&name).context("overlay has no creator authority")?;
        let overlay_lines: Vec<_> = orepo.p.export_jsonl()?.lines().map(str::to_owned).collect();
        authority.validate(&overlay_lines, &orepo.p.id, &trusted)?;
        let readers: BTreeMap<String, String> = orepo
            .policy()
            .access
            .into_iter()
            .filter(|(_, role)| *role >= crate::access::Role::Reader)
            .map(|(principal, role)| (principal, role.name().into()))
            .collect();
        let mut previous = remote.get(&name).cloned();
        // A record whose publication was interrupted remains the source of its retry.
        for line in orepo.p.effective_jsonl()?.lines() {
            let rec = oplog::parse_line(line, None)?.record;
            if rec.op == "overlay.key" && rec.data["store"].as_str() == Some(&descriptor.recipient) {
                let next = parse(line, &repo.p.id, &descriptor.recipient, &trusted)?;
                if previous.as_ref().is_none_or(|old| next.order() > old.order()) {
                    previous = Some(next);
                }
            }
        }
        let revocation = orepo.p.db.overlay_config("overlay.revocation")?.and_then(|value| value.as_str().map(str::to_owned));
        let access_mode = super::epochs::overlay_mode(&orepo)?;
        if hidden && access_mode != layr_store::epochs::Mode::Private {
            bail!("hidden overlays must remain private");
        }
        let rotate = previous.as_ref().is_none_or(|old| {
            old.key.access_mode > access_mode
                || old.key.revocation != revocation
                || old.key.readers.keys().any(|reader| !readers.contains_key(reader))
        });
        let reader_change = previous.as_ref().is_none_or(|old| old.key.readers != readers);
        if (rotate || reader_change) && !authority.machines.contains(&store.machine) {
            report
                .omitted
                .push(format!("overlay {name}: key change requires a key manager; signer {} is not a manager", store.machine));
            continue;
        }
        let published = if rotate {
            let identity = keys::generate_identity();
            let epoch = crate::ids::new_id();
            let path = local_key(store, &orepo.p.id, &epoch);
            std::fs::create_dir_all(path.parent().context("overlay key parent")?)?;
            crate::fsutil::atomic_write(&path, &keys::machine_slot(&identity, &store.key.verifying_key())?, 0o600)?;
            let key = Key {
                hidden,
                access_mode,
                overlay: orepo.p.id.clone(),
                name: name.clone(),
                epoch,
                parent: repo.p.id.clone(),
                store: descriptor.recipient.clone(),
                recipient: keys::recipient_of(&identity)?,
                readers: readers.clone(),
                revocation,
            };
            let mut record = Record::new("overlay.key");
            record.data = serde_json::to_value(&key)?;
            let record = orepo.p.append(record, &ctx.actor())?;
            let line = orepo.p.db.record_line(&record.machine, record.seq)?.context("overlay key record missing")?;
            Published { key, record, line }
        } else {
            let mut published = previous.context("overlay has no key record")?;
            // Adding a reader does not change the recipient, but its signed reader set must travel.
            if published.key.readers != readers || published.key.hidden != hidden || published.key.access_mode != access_mode {
                published.key.readers = readers.clone();
                published.key.hidden = hidden;
                published.key.access_mode = access_mode;
                let mut record = Record::new("overlay.key");
                record.data = serde_json::to_value(&published.key)?;
                published.record = orepo.p.append(record, &ctx.actor())?;
                published.line = orepo
                    .p
                    .db
                    .record_line(&published.record.machine, published.record.seq)?
                    .context("overlay key record missing")?;
            }
            published
        };
        drop(lock);
        let identity = layer_identity(store, &orepo.p.id, &published)?;
        let scoped = layer(store, &published, identity.clone(), repo.need_overlay(&name)?.cache)?;
        if let Some(identity) = &identity {
            for principal in readers.keys() {
                if let Some(machine) = principal.strip_prefix("machine:") {
                    if let Some(public) = trusted.get(machine) {
                        scoped.backend.put_new(&format!("slots/machine-{machine}.age"), keys::machine_slot(identity, public)?)?;
                    }
                } else if let Some((login, recipient)) = principal.strip_prefix("person:").and_then(|s| s.split_once('=')) {
                    scoped.backend.put_new(
                        &format!("slots/{}", layr_store::person::slot_name(login, recipient)?),
                        keys::seal_slot(identity, recipient)?,
                    )?;
                }
            }
            if let Some(recipient) = &paper_recipient {
                scoped.backend.put_new("slots/paper.age", keys::seal_slot(identity, recipient)?)?;
            } else if !repo.need_overlay(&name)?.cache {
                // A cache is regenerable: the paper key need not open it.
                report.omitted.push(format!("overlay {name}: no paper slot; run once with LAYR_PAPER_KEY_FILE to add it"));
            }
        }
        let push_lock = writer::push_folder_lock(&scoped.dir)?;
        // Test only (tests/t190-overlay-push-lock.sh): a slow upload of the overlay.
        if let Some(ms) = std::env::var("LAYR_TEST_OVERLAY_PUSH_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        merge(report, writer::push_locked(&scoped, ctx, &orepo, opts)?, &name, "push");
        // The push makes the old caches of the overlay dead on the remote too: delete them now.
        if repo.overlay_setting(&name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false) {
            let swept = sweep_layer(repo, &name, &orepo, &scoped, false)?;
            if !swept.trees.is_empty() || !swept.packs.is_empty() {
                eprintln!(
                    "layr: overlay {name}: deleted {} dead cache state(s) and {} part(s) ({} bytes), {} dead chunk(s) in {} pack object(s) ({} bytes) in the remote",
                    swept.trees.len(),
                    swept.parts.len(),
                    swept.bytes,
                    swept.chunks,
                    swept.packs.len(),
                    swept.chunk_bytes
                );
            }
        }
        drop(push_lock);
        if let Some(identity) = &identity {
            if access_mode == layr_store::epochs::Mode::Service {
                let recipient =
                    store.service_recipient(repo)?.context("service overlay requires a configured service recipient")?;
                scoped.backend.put_new("slots/service.age", keys::seal_slot(identity, &recipient)?)?;
            }
            if access_mode == layr_store::epochs::Mode::Public {
                let descriptor = scoped.descriptor()?;
                let manifest = if let Some(bytes) = scoped.backend.get("public.json")? {
                    layr_store::public::Manifest::parse(&bytes, &descriptor)?
                } else {
                    let manifest = layr_store::public::Manifest::new(
                        &descriptor,
                        keys::generate_identity(),
                        store.machine.clone(),
                        &store.key,
                    )?;
                    scoped.backend.put_new("public.json", serde_json::to_vec(&manifest)?)?;
                    manifest
                };
                scoped
                    .backend
                    .put_new("slots/public.age", keys::seal_slot(identity, &keys::recipient_of(&manifest.identity)?)?)?;
            }
        }
        // Publish the key after its epoch is complete. It contains the exact signed record, never an identity.
        let body = if hidden {
            keys::encrypt(&published.key.recipient, published.line.as_bytes())?
        } else {
            published.line.as_bytes().to_vec()
        };
        store.backend.put_new(&published.object(), body)?;
        // The key of this epoch is published: the caches of the older epochs have no reader now.
        if repo.overlay_setting(&name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false) {
            let swept = sweep_old_epochs(store, &orepo.p.id, &published.key.epoch, &listed, false)?;
            if !swept.trees.is_empty() || !swept.parts.is_empty() || !swept.packs.is_empty() {
                eprintln!(
                    "layr: overlay {name}: deleted {} cache state(s), {} part(s) and {} chunk object(s) of older key epochs in the remote",
                    swept.trees.len(),
                    swept.parts.len(),
                    swept.packs.len()
                );
            }
        }
    }
    Ok(())
}

/// The overlay and epoch of each overlay key object in `objects` (`overlays/<overlay>/keys/<epoch>-<record>.<ext>`).
fn key_epochs(objects: &[super::backend::Object]) -> BTreeSet<(String, String)> {
    objects
        .iter()
        .filter_map(|o| {
            let parts: Vec<_> = o.key.split('/').collect();
            let epoch = parts.get(3)?.get(..36).filter(|_| parts[3].as_bytes().get(36) == Some(&b'-'))?;
            (parts.len() == 4 && parts[0] == "overlays" && parts[2] == "keys" && crate::model::is_uuid(epoch))
                .then(|| (parts[1].to_owned(), epoch.to_owned()))
        })
        .collect()
}

/// Delete (with `dry_run`: only list) the cache states of the overlay `overlay` in its key epochs
/// that `listed` names and that are not `current`. A reader takes only the newest published key, so
/// nothing reads these states. `listed` comes from the key listing that chose `current`: an epoch
/// whose key a manager publishes after that listing, or whose key is not published yet (a rotation
/// in flight), is never touched. The records and slots of an old epoch stay; they are small.
fn sweep_old_epochs(
    store: &Store,
    overlay: &str,
    current: &str,
    listed: &BTreeSet<(String, String)>,
    dry_run: bool,
) -> Result<super::sweep::Swept> {
    let mut swept = super::sweep::Swept::default();
    for (_, epoch) in listed.iter().filter(|(o, e)| o == overlay && e != current) {
        let prefix = format!("overlays/{overlay}/epochs/{epoch}/states/");
        let (trees, parts): (Vec<_>, Vec<_>) =
            store.backend.list(&prefix)?.into_iter().map(|o| o.key).partition(|k| k.ends_with(".tree.age"));
        // Parts first, then the trees, as in `sweep::sweep`.
        for key in &parts {
            if !dry_run {
                store.backend.delete(key)?;
            }
        }
        for key in &trees {
            if !dry_run {
                store.backend.delete(key)?;
            }
        }
        swept.parts.extend(parts);
        swept.trees.extend(trees);
        // Chunk packs of the old epoch: nothing names them once its trees are gone. Packs and
        // indexes go after the trees, so a reader never finds a tree without its chunks.
        let chunks: Vec<String> = store
            .backend
            .list(&format!("overlays/{overlay}/epochs/{epoch}/chunks/"))?
            .into_iter()
            .map(|o| o.key)
            .collect();
        for key in &chunks {
            if !dry_run {
                store.backend.delete(key)?;
            }
        }
        swept.packs.extend(chunks);
    }
    Ok(swept)
}

/// The age identity of the layer `published` of the overlay project `overlay`: from its slot in the
/// store, else from this machine's local key file. None: this machine cannot read the layer.
fn layer_identity(store: &Store, overlay: &str, published: &Published) -> Result<Option<String>> {
    let mut identity = if store.backend.can_read() {
        open_identity(
            &store.backend.scoped(&published.prefix())?,
            &published.key,
            Some((&store.machine, &store.key)),
            store.person.as_ref(),
            store.paper_key_text.as_deref(),
        )?
    } else {
        None
    };
    if identity.is_none() {
        let path = local_key(store, overlay, &published.key.epoch);
        if path.exists() {
            let opened = keys::open_machine_slot(&std::fs::read(path)?, &store.key)?;
            if keys::recipient_of(&opened)? != published.key.recipient {
                bail!("local overlay key differs from its signed recipient");
            }
            identity = Some(opened);
        }
    }
    Ok(identity)
}

/// The sweep of the layer `scoped` of cache overlay `name` (`sweep::sweep`) with the live set of the records.
fn sweep_layer(repo: &Repo, name: &str, orepo: &Repo, scoped: &Store, dry_run: bool) -> Result<super::sweep::Swept> {
    let Some(kept) = repo.cache_kept(name)? else { return Ok(Default::default()) };
    let known = |id: &str| orepo.p.db.state(id).ok().flatten().is_some();
    let parent = |id: &str| orepo.p.db.state(id).ok().flatten().and_then(|s| s.parents.first().cloned());
    let local: Vec<String> = orepo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    super::sweep::sweep(scoped, &local, &kept, &known, &parent, dry_run)
}

/// Delete (with `dry_run`: only list) the dead cache states of each cache overlay of `repo` in the
/// store `store`: the dead states of the current epoch of each overlay, and all cache states of its
/// older epochs. The current epoch of an overlay that this machine cannot read is skipped.
pub(super) fn sweep_caches(store: &Store, ctx: &Ctx, repo: &Repo, dry_run: bool) -> Result<Vec<(String, super::sweep::Swept)>> {
    let descriptor = store.root_descriptor()?;
    let trusted = trusted_keys(repo)?;
    let mut objects = Vec::new();
    let mut names = Vec::new();
    for name in repo.visible_overlay_names(ctx)? {
        if !repo.overlay_setting(&name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if let Ok(overlay) = repo.overlay_repo(&name) {
            objects.extend(store.backend.list(&format!("overlays/{}/keys/", overlay.p.id))?);
            names.push(name);
        }
    }
    let parent_lines: Vec<_> = repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let authorities = crate::overlay_managers::from_lines(&repo.p.id, &parent_lines, &trusted)?;
    let listed = key_epochs(&objects);
    let (remote, _) = published_objects(&store.backend, &repo.p.id, &descriptor.recipient, &trusted, &authorities, objects)?;
    let mut out = Vec::new();
    for name in names {
        let Some(published) = remote.get(&name) else { continue };
        let orepo = repo.overlay_repo(&name)?;
        let old = sweep_old_epochs(store, &orepo.p.id, &published.key.epoch, &listed, dry_run)?;
        let Some(identity) = layer_identity(store, &orepo.p.id, published)? else {
            out.push((name.clone(), old));
            continue;
        };
        let scoped = layer(store, published, Some(identity), repo.need_overlay(&name)?.cache)?;
        let _push = writer::push_folder_lock(&scoped.dir)?;
        let mut swept = sweep_layer(repo, &name, &orepo, &scoped, dry_run)?;
        swept.trees.extend(old.trees);
        swept.parts.extend(old.parts);
        swept.packs.extend(old.packs);
        out.push((name.clone(), swept));
    }
    Ok(out)
}

pub(super) fn fetch(store: &Store, ctx: &Ctx, repo: &Repo, opts: &Options, report: &mut Report) -> Result<()> {
    let descriptor = store.root_descriptor()?;
    let trusted = trusted_keys(repo)?;
    let parent_lines: Vec<_> = repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let mut authorities = crate::overlay_managers::from_lines(&repo.p.id, &parent_lines, &trusted)?;
    let (mut published, refused) = published(&store.backend, &repo.p.id, &descriptor.recipient, &trusted, &authorities)?;
    report.omitted.extend(refused);
    let hidden = if store.is_public_reader() {
        BTreeMap::new()
    } else {
        discover_hidden(
            &store.backend,
            &repo.p.id,
            &descriptor.recipient,
            &trusted,
            Some((&store.machine, &store.key)),
            store.person.as_ref(),
            opts.paper_key.as_deref().or(store.paper_key_text.as_deref()),
            &BTreeMap::new(),
        )?
    };
    let mut identities = BTreeMap::new();
    for (name, (key, identity, authority)) in hidden {
        if published.contains_key(&name) {
            continue;
        }
        identities.insert(name.clone(), identity);
        authorities.insert(name.clone(), authority);
        published.insert(name, key);
    }
    for (name, published) in published {
        let is_cache = repo.overlay_info(&name).ok().flatten().is_some_and(|info| info.cache);
        let mut one = || -> Result<()> {
            if store.is_public_reader() && published.key.access_mode != layr_store::epochs::Mode::Public {
                return Ok(());
            }
            if !published.key.hidden && repo.overlay_info(&name)?.is_none() {
                bail!("overlay key names an unknown overlay {name}");
            }
            let backend = store.backend.scoped(&published.prefix())?;
            let identity = open_identity(
                &backend,
                &published.key,
                Some((&store.machine, &store.key)),
                store.person.as_ref(),
                opts.paper_key.as_deref().or(store.paper_key_text.as_deref()),
            )?
            .or_else(|| identities.remove(&name));
            let Some(identity) = identity else {
                if !published.key.hidden {
                    report.omitted.push(format!("overlay {name}: no key on this machine"));
                }
                return Ok(());
            };
            let pname = format!("{}%{name}", repo.p.name);
            let mut op = if crate::store::db_file(&ctx.store.project_dir(&pname)?).exists() {
                Project::open_partial(&ctx.store, &pname)?
            } else {
                Project::create_empty_overlay(&ctx.store, &pname)?
            };
            if !op.id.is_empty() && op.id != published.key.overlay {
                bail!("overlay {name} has another project id");
            }
            // Overlay records must use the same trusted machine keys as the parent. The layer's own fetch below reads
            // the same segments: both skip the ones read and checked before (`seen-records` of the layer).
            let local: Vec<String> = if op.id.is_empty() { Vec::new() } else { op.export_jsonl()?.lines().map(str::to_owned).collect() };
            let seen = reader::seen_file(&store.dir.join("overlays").join(&published.key.overlay).join(&published.key.epoch));
            let records = reader::records_with_local(&backend, &identity, &published.key.overlay, &local, Some(&seen))?;
            let trusted = trusted_keys(repo)?;
            // A machine trusted after this fetch read the project records (a long clone, while a runner that
            // was just trusted pushes) has records here before its trust record: they wait for a later fetch.
            let waiting = not_trusted_yet(&records.keys, &trusted)
                .map_err(|machine| anyhow::anyhow!("overlay {name}: record signer {machine} is trusted here with another key"))?;
            let lines: Vec<String> = records
                .lines
                .iter()
                .filter(|line| oplog::parse_line(line, None).is_ok_and(|s| !waiting.contains(&s.record.machine)))
                .cloned()
                .collect();
            if !waiting.is_empty() {
                let names: Vec<&str> = waiting.iter().map(String::as_str).collect();
                report
                    .omitted
                    .push(format!("overlay {name}: records of machine {} not trusted here yet; a later fetch takes them", names.join(", ")));
            }
            if !lines.contains(&published.line) {
                bail!("overlay {name}: published key is absent from the overlay log");
            }
            authorities.get(&name).context("overlay has no creator authority")?.validate(&lines, &published.key.overlay, &trusted)?;
            // An epoch includes the complete log, so a new reader needs no older layer identity.
            if op.id.is_empty() {
                op.import_lines(&lines, crate::store::Trust::All)?;
                op.reload_id()?;
            }
            let mut orepo = Repo { p: op, ctx: None };
            for (machine, key) in records.keys.iter().filter(|(machine, _)| !waiting.contains(*machine)) {
                crate::cmd::replicate::trust(ctx, &orepo.p, machine, &hex::encode(key.to_bytes()), None)?;
            }
            let mut scoped = layer(store, &published, Some(identity), false)?;
            let mut opts = opts.clone();
            if is_cache && !opts.caches_of.is_empty() {
                opts.only_caches = Some(caches_of_lines(repo, &orepo, &opts.caches_of)?);
            }
            merge(report, reader::fetch(&mut scoped, ctx, &mut orepo, &opts)?, &name, "fetch");
            // Fetched states of another machine move its overlay lines: the history limit applies now, as after a
            // local move, so the states beyond it go in this command.
            repo.after_overlay_move(ctx, &name);
            Ok(())
        };
        match one() {
            // A cache is regenerable: its overlay omits, and the clone or fetch of the rest goes on.
            Err(e) if is_cache => report.omitted.push(format!("overlay {name}: {e:#}")),
            r => r?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signer_not_trusted_yet_waits_and_another_key_fails() {
        let key = |n: u8| ed25519_dalek::SigningKey::from_bytes(&[n; 32]).verifying_key();
        let keys: BTreeMap<String, VerifyingKey> = [("a".to_string(), key(1)), ("r".to_string(), key(2))].into();
        let trusted: BTreeMap<String, VerifyingKey> = [("a".to_string(), key(1))].into();
        assert_eq!(not_trusted_yet(&keys, &trusted), Ok(BTreeSet::from(["r".to_string()])));
        let other: BTreeMap<String, VerifyingKey> = [("a".to_string(), key(1)), ("r".to_string(), key(3))].into();
        assert_eq!(not_trusted_yet(&keys, &other), Err("r".to_string()));
        assert_eq!(not_trusted_yet(&keys, &keys), Ok(BTreeSet::new()));
    }
    #[test]
    #[ignore = "invoked by t35 with its actual store and machine keys"]
    fn overlay_ciphertexts_reject_nonreader_keys() {
        // Only t35 sets these folders and keys.
        let Some(root) = std::env::var_os("LAYR_TEST_OVERLAY_DIR").map(std::path::PathBuf::from) else { return };
        let croot = std::path::PathBuf::from(std::env::var("LAYR_TEST_OVERLAY_C_ROOT").unwrap());
        let aroot = std::path::PathBuf::from(std::env::var("LAYR_TEST_OVERLAY_A_ROOT").unwrap());
        let signing = |root: &std::path::Path| {
            let bytes: [u8; 32] =
                hex::decode(std::fs::read_to_string(root.join("machine/key")).unwrap().trim()).unwrap().try_into().unwrap();
            ed25519_dalek::SigningKey::from_bytes(&bytes)
        };
        let backend = Backend::file(root.clone());
        let a = signing(&aroot);
        let c = signing(&croot);
        let aid = std::fs::read_to_string(aroot.join("machine/id")).unwrap();
        let vault =
            keys::open_machine_slot(&backend.get(&format!("slots/machine-{}.age", aid.trim())).unwrap().unwrap(), &a).unwrap();
        let mut count = 0;
        for object in backend.list("overlays/").unwrap() {
            if !object.key.ends_with(".age") {
                continue;
            }
            let body = backend.get(&object.key).unwrap().unwrap();
            assert!(keys::decrypt(&vault, &body).is_err(), "vault opened {}", object.key);
            assert!(keys::open_machine_slot(&body, &c).is_err(), "C opened {}", object.key);
            count += 1;
        }
        assert!(count >= 4, "no overlay ciphertexts checked");
        // The mount drops the paper opener after its first read. Layer identities stay in memory.
        let mut access = super::super::browse::Access {
            paper: Some(std::fs::read_to_string(root.parent().unwrap().join("paper")).unwrap()),
            ..Default::default()
        };
        let url = format!("file://{}", root.display());
        let browse = super::super::browse::Browse::open(&url, &Default::default(), &access).unwrap();
        access.identity = Some(browse.identity().to_string());
        access.paper = None;
        let mut refreshed = super::super::browse::Browse::open(&url, &Default::default(), &access).unwrap();
        let entry = refreshed.lines.iter().find(|line| line.name == "main").unwrap().files[".env"].clone();
        assert!(refreshed.read(&entry).unwrap().ends_with(b"private-epoch-one\n"));
    }

    #[test]
    #[ignore = "invoked by t35 after it removes reader B"]
    fn a_removed_reader_cannot_substitute_the_layer_recipient() {
        // Only t35 sets these folders and keys.
        let Some(root) = std::env::var_os("LAYR_TEST_OVERLAY_DIR").map(std::path::PathBuf::from) else { return };
        let aroot = std::path::PathBuf::from(std::env::var("LAYR_TEST_OVERLAY_A_ROOT").unwrap());
        let broot = std::path::PathBuf::from(std::env::var("LAYR_TEST_OVERLAY_B_ROOT").unwrap());
        let signing = |root: &std::path::Path| {
            let bytes: [u8; 32] =
                hex::decode(std::fs::read_to_string(root.join("machine/key")).unwrap().trim()).unwrap().try_into().unwrap();
            ed25519_dalek::SigningKey::from_bytes(&bytes)
        };
        let backend = Backend::file(root.clone());
        let mut published: Vec<_> = backend
            .list("overlays/")
            .unwrap()
            .into_iter()
            .filter(|o| o.key.contains("/keys/"))
            .map(|o| {
                let line = String::from_utf8(backend.get(&o.key).unwrap().unwrap()).unwrap();
                oplog::parse_line(&line, None).unwrap().record
            })
            .collect();
        published.sort_by(|a, b| (a.time, &a.machine, a.seq).cmp(&(b.time, &b.machine, b.seq)));
        let original = published.last().unwrap();
        let key: Key = serde_json::from_value(original.data.clone()).unwrap();
        let aid = std::fs::read_to_string(aroot.join("machine/id")).unwrap();
        let bid = std::fs::read_to_string(broot.join("machine/id")).unwrap().trim().to_string();
        let scoped = backend.scoped(&format!("overlays/{}/epochs/{}", key.overlay, key.epoch)).unwrap();
        let identity = keys::open_machine_slot(
            &scoped.get(&format!("slots/machine-{}.age", aid.trim())).unwrap().unwrap(),
            &signing(&aroot),
        )
        .unwrap();
        let malicious_identity = keys::generate_identity();
        let mut forged = original.clone();
        forged.id = crate::ids::new_id();
        forged.machine = bid.clone();
        forged.seq = 1000000;
        forged.time = crate::ids::now_ms() + 1;
        forged.data["recipient"] = serde_json::json!(keys::recipient_of(&malicious_identity).unwrap());
        forged.data["epoch"] = serde_json::json!(crate::ids::new_id());
        let forged_line = oplog::sign_line(&forged, &signing(&broot)).unwrap();
        let path = format!("overlays/{}/keys/{}-{}.json", key.overlay, forged.data["epoch"].as_str().unwrap(), forged.id);
        assert!(backend.put_new(&path, forged_line.into_bytes()).unwrap());
        let output = std::process::Command::new(std::env::var("LAYR_TEST_BINARY").unwrap())
            .args(["--project", "project", "push", "archive", "--json"])
            .env("LAYR_ROOT", &aroot)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(report["omitted"].as_array().unwrap().iter().any(|v| v.as_str().unwrap().contains(&bid)), "{report}");
        let records = reader::records(&scoped, &identity, &key.overlay).unwrap();
        let mut source = reader::Reader::new(&scoped, &identity, &records.keys);
        let mut found = false;
        for (state, attempt) in reader::attempts(&scoped).unwrap() {
            let (_, _, tree) = source.tree(&state, &attempt).unwrap();
            if let Some(entry) = tree.files.get(".env") {
                found |= source.read(entry).unwrap().ends_with(b"private-after-forgery\n");
            }
        }
        assert!(found, "A did not encrypt its new state to the manager's key");
        for object in scoped.list("states/").unwrap() {
            assert!(keys::decrypt(&malicious_identity, &scoped.get(&object.key).unwrap().unwrap()).is_err());
        }
    }

    #[test]
    fn a_key_record_binds_the_parent_store_overlay_and_signer() {
        let signer = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let machine = crate::ids::new_id();
        let parent = crate::ids::new_id();
        let store = keys::recipient_of(&keys::generate_identity()).unwrap();
        let overlay = crate::ids::new_id();
        let mut rec = Record::new("overlay.key");
        rec.id = crate::ids::new_id();
        rec.machine = machine.clone();
        rec.project = overlay.clone();
        rec.seq = 2;
        rec.time = 1;
        rec.data = serde_json::to_value(Key {
            hidden: false,
            overlay,
            name: "private".into(),
            epoch: crate::ids::new_id(),
            parent: parent.clone(),
            store: store.clone(),
            recipient: keys::recipient_of(&keys::generate_identity()).unwrap(),
            readers: BTreeMap::new(),
            revocation: None,
            access_mode: layr_store::epochs::Mode::Private,
        })
        .unwrap();
        let line = oplog::sign_line(&rec, &signer).unwrap();
        let trusted = BTreeMap::from([(machine, signer.verifying_key())]);
        assert!(parse(&line, &parent, &store, &trusted).is_ok());
        assert!(parse(&line, &crate::ids::new_id(), &store, &trusted).is_err());
        assert!(parse(&line, &parent, "age1other", &trusted).is_err());
        assert!(parse(&line, &parent, &store, &BTreeMap::new()).is_err());
        rec.data["recipient"] = serde_json::json!(keys::recipient_of(&keys::generate_identity()).unwrap());
        let changed = line.replace(
            &serde_json::from_str::<serde_json::Value>(&line).unwrap()["r"]["data"]["recipient"].as_str().unwrap().to_string(),
            rec.data["recipient"].as_str().unwrap(),
        );
        assert!(parse(&changed, &parent, &store, &trusted).is_err());
    }
}
