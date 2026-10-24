//! Encrypted immutable remotes, independent of the local snapshot backend.

pub(crate) mod backend;
pub(crate) mod browse;
mod gateway;
mod legacy;
pub(crate) mod reader;
mod writer;

pub(crate) use layr_store::{canonical, format, keys, s3, zst};

use crate::ctx::Ctx;
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use backend::Backend;
use format::Descriptor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Clone, Default)]
pub struct Options {
    pub auto: bool,
    pub caches: bool,
    pub paper_key: Option<String>,
    pub desk_prefix: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub records: usize,
    pub states: usize,
    pub files: usize,
    pub bytes: u64,
    pub omitted: Vec<String>,
    pub overlaps: usize,
    pub gaps: Vec<String>,
    pub missing_lines: Vec<String>,
}

#[derive(Default)]
pub struct ConfigureOptions {
    pub paper_key_out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Configured {
    pub created: bool,
    pub recovery_key_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StorePin {
    pub project: String,
    pub recipient: String,
}

impl StorePin {
    fn check(&self, descriptor: &Descriptor) -> Result<()> {
        if self.project != descriptor.project {
            bail!("store project changed: pinned {}, store.json {}", self.project, descriptor.project);
        }
        if self.recipient != descriptor.recipient {
            bail!(
                "store recipient changed: pinned {}, store.json {}; use layr remote trust-store <name> --recipient <age1…>",
                self.recipient,
                descriptor.recipient
            );
        }
        Ok(())
    }
}

pub struct Store {
    backend: Backend,
    url: String,
    project: String,
    machine: String,
    key: ed25519_dalek::SigningKey,
    dir: PathBuf,
    paper_key_text: Option<String>,
    pin: Option<StorePin>,
}

/// Opening a store performs no remote I/O. Configure it with the caller's environment next.
pub fn open(url: &str, repo: &Repo) -> Result<Store> {
    let mut pin: Option<StorePin> = None;
    for (key, _) in repo.p.db.config_all()? {
        let Some(name) = key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")) else { continue };
        if repo.p.config_value(&key)?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() != Some(url) {
            continue;
        }
        if let Some(value) = repo.p.config_value(&format!("cloud.remote.{name}.store"))? {
            let stored: StorePin = serde_json::from_value(value).context("invalid store pin")?;
            if pin.as_ref().is_some_and(|pin| pin != &stored) {
                bail!("conflicting local pins for store {url}");
            }
            pin = Some(stored);
        }
    }
    open_with_pin(url, repo, pin)
}

pub fn open_named(name: &str, repo: &Repo) -> Result<Store> {
    let url = crate::cmd::remote::remote_url(repo, name)?;
    let pin = repo
        .p
        .config_value(&format!("cloud.remote.{name}.store"))?
        .map(serde_json::from_value)
        .transpose()
        .context("invalid store pin")?;
    open_with_pin(&url, repo, pin)
}

fn open_with_pin(url: &str, repo: &Repo, pin: Option<StorePin>) -> Result<Store> {
    let hash = hex::encode(Sha256::digest(url.as_bytes()));
    Ok(Store {
        backend: Backend::open(url)?,
        url: url.to_string(),
        project: repo.p.id.clone(),
        machine: repo.p.store.machine.id.clone(),
        key: repo.p.store.machine.key.clone(),
        dir: repo.p.meta_dir().join("remote-stores").join(hash),
        paper_key_text: None,
        pin,
    })
}

impl Store {
    fn connect(&mut self, ctx: &Ctx) -> Result<()> {
        self.backend.as_caller(ctx);
        if let Backend::Gateway(gateway) = &mut self.backend {
            gateway.connect(ctx)?;
        }
        self.paper_key_text = ctx
            .env("LAYR_PAPER_KEY_FILE")
            .map(|path| {
                crate::privs::as_caller_fs(ctx, || {
                    use std::io::Read;
                    let mut text = String::new();
                    std::fs::File::open(ctx.cwd.join(path))?.take(4097).read_to_string(&mut text)?;
                    if text.len() > 4096 {
                        bail!("invalid paper key file");
                    }
                    Ok(text)
                })
            })
            .transpose()?;
        if let Some(path) = self.url.strip_prefix("s3://") {
            let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
            self.backend = Backend::S3(s3::S3::with_env(bucket, prefix, &ctx.env)?);
        }
        Ok(())
    }

    pub fn configure(&mut self, ctx: &Ctx, opts: &ConfigureOptions) -> Result<Configured> {
        self.connect(ctx)?;
        if self.backend.get("store.json")?.is_some() {
            self.pin_descriptor()?;
            return Ok(Configured { created: false, recovery_key_file: None });
        }
        if self.backend.get("vault.json")?.is_some() {
            let vault: serde_json::Value =
                serde_json::from_slice(&self.backend.get("vault.json")?.context("missing vault.json")?)?;
            if vault["format"].as_u64() != Some(1) {
                bail!("unsupported Desk vault format");
            }
            return Ok(Configured { created: false, recovery_key_file: None });
        }
        if self.project.is_empty() {
            bail!("the remote store is empty");
        }
        let path = opts.paper_key_out.as_ref().context("a new store requires --paper-key-out <path>")?;
        let file = writer::initialize(self, ctx, path)?;
        self.pin_descriptor()?;
        Ok(Configured { created: true, recovery_key_file: Some(file) })
    }

    fn pin_descriptor(&mut self) -> Result<()> {
        let descriptor = self.descriptor()?;
        self.pin = Some(StorePin { project: descriptor.project, recipient: descriptor.recipient });
        Ok(())
    }

    /// Caller holds the project lock. Config records carry the machine signature.
    pub fn save_pin(&self, ctx: &Ctx, repo: &Repo, name: &str) -> Result<()> {
        if let Some(pin) = &self.pin {
            let key = format!("cloud.remote.{name}.store");
            let value = serde_json::to_value(pin)?;
            if repo.p.config_value(&key)?.as_ref() != Some(&value) {
                crate::cmd::admin::set_config(ctx, repo, &key, value)?;
            }
        }
        Ok(())
    }

    pub fn trust_store(&mut self, ctx: &Ctx, repo: &Repo, name: &str, recipient: &str) -> Result<()> {
        recipient.parse::<age::x25519::Recipient>().map_err(|e| anyhow::anyhow!("invalid store recipient: {e}"))?;
        self.connect(ctx)?;
        let previous = self.pin.take();
        let descriptor = self.descriptor();
        self.pin = previous;
        let descriptor = descriptor?;
        if let Some(pin) = &self.pin {
            if pin.project != descriptor.project {
                bail!("store project changed: pinned {}, store.json {}", pin.project, descriptor.project);
            }
        }
        if descriptor.recipient != recipient {
            bail!("store recipient differs: requested {}, store.json {}", recipient, descriptor.recipient);
        }
        self.pin = Some(StorePin { project: descriptor.project, recipient: descriptor.recipient });
        self.save_pin(ctx, repo, name)
    }

    fn descriptor(&self) -> Result<Descriptor> {
        let data = self
            .backend
            .get("store.json")?
            .context("remote store is not initialized; use layr remote add with --paper-key-out")?;
        let descriptor: Descriptor = serde_json::from_slice(&data)?;
        if descriptor.format != 2 {
            bail!("unsupported store format {}", descriptor.format);
        }
        if !crate::model::is_uuid(&descriptor.project) {
            bail!("store has an invalid project id");
        }
        if let Some(pin) = &self.pin {
            pin.check(&descriptor)?;
        }
        if !self.project.is_empty() && descriptor.project != self.project {
            bail!("store project differs: local {}, store.json {}", self.project, descriptor.project);
        }
        descriptor.recipient.parse::<age::x25519::Recipient>().map_err(|e| anyhow::anyhow!("invalid store recipient: {e}"))?;
        Ok(descriptor)
    }

    fn identity(&self, descriptor: &Descriptor, paper: Option<&str>) -> Result<String> {
        let machine = format!("slots/machine-{}.age", self.machine);
        if let Some(slot) = self.backend.get(&machine)? {
            let identity = keys::open_machine_slot(&slot, &self.key)?;
            if keys::recipient_of(&identity)? != descriptor.recipient {
                bail!("machine slot does not match store recipient");
            }
            if let Backend::Gateway(gateway) = &self.backend {
                gateway.set_identity(&identity)?;
            }
            return Ok(identity);
        }
        let paper = paper.map(str::to_string).or_else(|| self.paper_key_text.clone());
        if let Some(paper) = paper {
            let opener = keys::identity_from_paper(&paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
            let slot = self.backend.get("slots/paper.age")?.context("remote has no paper slot")?;
            let identity = keys::open_slot(&slot, &opener)?;
            if keys::recipient_of(&identity)? != descriptor.recipient {
                bail!("paper slot does not match store recipient");
            }
            if let Backend::Gateway(gateway) = &self.backend {
                gateway.set_identity(&identity)?;
            }
            return Ok(identity);
        }
        bail!("this machine has no remote slot; set LAYR_PAPER_KEY_FILE or use layr remote trust")
    }

    fn private_dir(&self) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    pub fn push(&mut self, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
        self.connect(ctx)?;
        repo.require(ctx, "sync")?;
        if repo.p.is_overlay() {
            bail!("private overlay projects cannot be pushed to a store");
        }
        if self.backend.get("store.json")?.is_none() && self.backend.get("vault.json")?.is_some() {
            bail!("Desk v1 stores are read only; clone into a local project and add a new v2 remote");
        }
        writer::push(self, ctx, repo, opts)
    }

    pub fn fetch(&mut self, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
        self.connect(ctx)?;
        if !repo.p.id.is_empty() {
            repo.require(ctx, "sync")?;
        }
        if self.backend.get("vault.json")?.is_some() && self.backend.get("store.json")?.is_none() {
            bail!("Desk v1 is read only; use layr clone with LAYR_PAPER_KEY_FILE");
        }
        let mut report = reader::fetch(self, ctx, repo, opts)?;
        let pending = self.dir.join("pending-lines.json");
        if pending.exists() {
            let ids: std::collections::BTreeSet<String> = serde_json::from_slice(&std::fs::read(pending)?)?;
            report.missing_lines = self.materialize_clone_lines(ctx, repo, Some(&ids))?;
        }
        Ok(report)
    }

    pub fn clone_into(&mut self, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
        self.connect(ctx)?;
        if !repo.p.id.is_empty() {
            bail!("clone needs an empty project");
        }
        let mut report = if self.backend.get("store.json")?.is_none() && self.backend.get("vault.json")?.is_some() {
            legacy::import(self, ctx, repo, opts)?
        } else {
            reader::fetch(self, ctx, repo, opts)?
        };
        report.missing_lines = self.materialize_clone_lines(ctx, repo, None)?;
        Ok(report)
    }

    fn materialize_clone_lines(
        &self,
        ctx: &Ctx,
        repo: &mut Repo,
        only: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<Vec<String>> {
        self.private_dir()?;
        let _lock = repo.p.lock()?;
        let lines = repo.p.db.lines()?;
        let mut pending = Vec::new();
        let mut missing = Vec::new();
        for line in lines {
            if only.is_some_and(|ids| !ids.contains(&line.id)) {
                continue;
            }
            if !repo.p.has_state(&line.head) {
                pending.push(line.id.clone());
                missing.push(format!(
                    "line {}: head state {} is not in the store yet; run `layr fetch <remote>` later",
                    line.name, line.head
                ));
                continue;
            }
            let local = crate::model::Line { machine: repo.p.store.machine.id.clone(), owner: ctx.caller.uid, ..line.clone() };
            if !repo.p.line_path(&local.name).exists() {
                repo.materialize(&local, &local.head, &repo.p.line_path(&local.name))?;
                let mut record = crate::model::Record::new("line.adopt");
                record.lines.insert(
                    local.id.clone(),
                    crate::model::LineChange {
                        before: Some(repo.line_state(&line, &line.head)),
                        after: Some(repo.line_state(&local, &local.head)),
                        working_before: None,
                        working_after: None,
                    },
                );
                repo.commit_record(ctx, record)?;
            }
        }
        crate::fsutil::atomic_write(&self.dir.join("pending-lines.json"), &serde_json::to_vec(&pending)?, 0o600)?;
        Ok(missing)
    }

    pub fn verify(&self, files: bool) -> Result<Report> {
        reader::verify(self, files)
    }
    pub fn log(&self) -> Result<Vec<String>> {
        reader::log(self)
    }
    pub fn gc(&self, dry_run: bool) -> Result<Vec<String>> {
        reader::gc(self, dry_run)
    }

    pub fn trust(&self, repo: &Repo, machine: &str, public_key: Option<&str>) -> Result<()> {
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        let id = if crate::model::is_uuid(machine) {
            machine.to_string()
        } else {
            repo.p.db.machine_by_name(machine)?.context("unknown machine")?
        };
        let first_key = repo.p.db.machine_key(&id)?;
        let public = public_key
            .map(str::to_string)
            .or_else(|| first_key.clone())
            .context("machine has no key; give --public-key <key>")?;
        if first_key.as_ref().is_some_and(|first| first != &public) {
            bail!("the public key differs from the machine's first record");
        }
        if repo.p.db.trusted(&id)?.as_ref().is_some_and(|trusted| trusted != &public) {
            bail!("the public key differs from the trusted machine key");
        }
        let bytes: [u8; 32] = hex::decode(public)?.try_into().map_err(|_| anyhow::anyhow!("invalid machine key"))?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&bytes)?;
        let slot = keys::machine_slot(&identity, &key)?;
        let name = format!("slots/machine-{id}.age");
        // The signed trust path binds this machine id to one key. A retry keeps its slot.
        self.backend.put_new(&name, slot)?;
        Ok(())
    }
}

/// The CLI commands use the same named-remote configuration as push and fetch.
pub fn command(ctx: &Ctx, operation: &str, args: &[String]) -> Result<i32> {
    let spec = crate::args::Spec::new().value("--remote").flag("--files").flag("--json").flag("-n|--dry-run").flag("-q|--quiet");
    let a = crate::args::parse(&spec, args)?;
    let name = a.get("--remote").context("name a remote with --remote <name>")?;
    let h = crate::cmd::here(ctx)?;
    h.repo.require(ctx, if operation == "gc" { "gc" } else { "fsck" })?;
    let mut store = open_named(name, &h.repo)?;
    store.connect(ctx)?;
    match operation {
        "fsck" => {
            let report = store.verify(a.has("--files"))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&report)?);
            } else {
                outln!(
                    ctx,
                    "remote {name}: {} records, {} states, {} verified files, {} overlaps",
                    report.records,
                    report.states,
                    report.files,
                    report.overlaps
                );
                for gap in &report.gaps {
                    outln!(ctx, "gap: {gap}");
                }
            }
            if !report.gaps.is_empty() {
                return Ok(1);
            }
        }
        "log" => {
            for line in store.log()? {
                if a.has("--json") {
                    outln!(ctx, "{line}");
                } else {
                    let record = crate::oplog::parse_line(&line, None)?.record;
                    for state in record.states {
                        outln!(ctx, "{} {} {}", crate::ids::short(&state.id), state.time, state.message);
                    }
                }
            }
        }
        "gc" => {
            let deleted = store.gc(a.has("-n"))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&deleted)?);
            } else if !a.has("-q") {
                outln!(ctx, "{} {} orphan remote parts", if a.has("-n") { "would delete" } else { "deleted" }, deleted.len());
            }
        }
        _ => bail!("unknown remote store operation"),
    }
    Ok(0)
}

/// Validate the gateway URL before a remote is written to project configuration.
pub(crate) fn validate_gateway_url(url: &str) -> Result<()> {
    gateway::Gateway::open(url).map(|_| ())
}

#[cfg(test)]
mod pin_tests {
    use super::*;
    use crate::ctx::{Caller, Ctx};
    use crate::model::Record;

    #[test]
    fn changed_file_descriptor_stops_writer_fetch_clone_and_mount_before_encryption_or_decryption() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = Ctx {
            store: crate::store::Store::open(&root.join("local")).unwrap(),
            caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.clone(),
            env: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        let dir = ctx.store.projects_dir().join("p");
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        let project_id = crate::ids::new_id();
        let mut repo = Repo {
            p: crate::store::Project {
                store: &ctx.store,
                name: "p".into(),
                dir: dir.clone(),
                id: project_id.clone(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            },
        };
        let mut record = Record::new("project");
        record.data = serde_json::json!({"id":project_id,"name":"p","config":{}});
        repo.p.append(record, &ctx.actor()).unwrap();
        let url = format!("file://{}", root.join("remote").display());
        let mut store = open(&url, &repo).unwrap();
        store.configure(&ctx, &ConfigureOptions { paper_key_out: Some(root.join("paper")) }).unwrap();
        crate::cmd::admin::set_config(&ctx, &repo, "cloud.remote.archive.url", serde_json::json!(url)).unwrap();
        store.save_pin(&ctx, &repo, "archive").unwrap();
        let mut store = open(&url, &repo).unwrap();
        let pin = store.pin.clone().unwrap();
        let attacker = keys::generate_identity();
        let recipient = keys::recipient_of(&attacker).unwrap();
        std::fs::write(
            root.join("remote/store.json"),
            serde_json::to_vec(&Descriptor { format: 2, project: project_id.clone(), recipient: recipient.clone() }).unwrap(),
        )
        .unwrap();
        // No slot: this machine can only write to the public recipient.
        std::fs::remove_dir_all(root.join("remote/slots")).unwrap();
        let before = store.backend.list("").unwrap().len();
        let error = writer::push(&store, &ctx, &repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        assert_eq!(store.backend.list("").unwrap().len(), before);
        let error = reader::fetch(&mut store, &ctx, &mut repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        // The descriptor must win over the missing slot error in clone and mount too.
        repo.p.id.clear();
        store.project.clear();
        let error = store.clone_into(&ctx, &mut repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        let access = browse::Access { pin: Some(pin.clone()), ..Default::default() };
        let error = browse::Browse::open(&url, &Default::default(), &access).err().unwrap().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        let other_project = crate::ids::new_id();
        std::fs::write(
            root.join("remote/store.json"),
            serde_json::to_vec(&Descriptor { format: 2, project: other_project.clone(), recipient: pin.recipient.clone() })
                .unwrap(),
        )
        .unwrap();
        let error = store.descriptor().err().unwrap().to_string();
        assert!(error.contains(&project_id) && error.contains(&other_project), "{error}");
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
}
