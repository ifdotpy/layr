//! Stream built release files through the existing epoch vault and immutable object transport.
use super::{reader, writer, Options, Store};
use crate::{ctx::Ctx, ids, model::Record, repo::Repo};
use anyhow::{bail, Context, Result};
use layr_hashers::{ContentAddress, Digest};
use layr_model::release::{self, File, Files};
use std::collections::BTreeMap;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    str::FromStr,
};

/// The size limit of a public release file (`--public-files`): the gateway refuses larger ones.
const PUBLIC_MAX_BYTES: u64 = 32 << 20;

/// An unlinked private ciphertext spool bounds memory and leaves no disk artifact after exit.
fn spool() -> Result<fs::File> {
    let path = std::env::temp_dir().join(format!("layr-release-{}", ids::new_id()));
    let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
    fs::remove_file(path)?;
    Ok(file)
}

/// The release file encrypted to the store recipient; a Private or Public store ([`keys::PLAIN`]) keeps it plain.
fn encrypt(mut input: fs::File, recipient: &str, public: bool) -> Result<(fs::File, Option<fs::File>, u64, String)> {
    let mut output = spool()?;
    let mut public_copy = if public { Some(spool()?) } else { None };
    let mut writer = if recipient == layr_store::keys::PLAIN {
        Out::Plain(&mut output)
    } else {
        let recipient = age::x25519::Recipient::from_str(recipient).map_err(|_| anyhow::anyhow!("invalid release recipient"))?;
        let encryptor = age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))?;
        Out::Age(encryptor.wrap_output(&mut output)?)
    };
    let mut hash = ContentAddress::new();
    let mut size = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > release::MAX_FILE_BYTES {
            bail!("release file exceeds 512 MiB");
        }
        if public && size > PUBLIC_MAX_BYTES {
            bail!("a public release file is at most 32 MiB");
        }
        hash.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
        if let Some(copy) = &mut public_copy {
            copy.write_all(&buffer[..n])?;
        }
    }
    writer.finish()?;
    output.seek(SeekFrom::Start(0))?;
    if let Some(copy) = &mut public_copy {
        copy.seek(SeekFrom::Start(0))?;
    }
    Ok((output, public_copy, size, hex::encode(hash.finalize())))
}

/// A release output: an age stream ends with its last chunk; a Private or Public store keeps plain bytes.
enum Out<'a> {
    Plain(&'a mut fs::File),
    Age(age::stream::StreamWriter<&'a mut fs::File>),
}

impl Write for Out<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Out::Plain(file) => file.write(buffer),
            Out::Age(writer) => writer.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Out::Plain(file) => file.flush(),
            Out::Age(writer) => writer.flush(),
        }
    }
}

impl Out<'_> {
    fn finish(self) -> Result<()> {
        if let Out::Age(writer) = self {
            writer.finish()?;
        }
        Ok(())
    }
}

/// What a record changes besides its data, for `layr_store::trust`.
fn payload_of(record: &crate::model::Record) -> layr_store::trust::Payload<'_> {
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

/// Keep full-writer records and merge records accepted against the earlier signed history.
/// Input signatures and contiguous machine prefixes must already be verified.
fn effective_lines(project: &str, lines: &[String], _machine: &str) -> Result<(layr_store::public::Writers, Vec<String>)> {
    let writers = layr_store::public::release_writers_checked(project, lines)?;
    let mut parsed: Vec<(&String, crate::model::Record)> =
        lines.iter().map(|line| crate::oplog::parse_line(line, None).map(|p| (line, p.record))).collect::<Result<_>>()?;
    parsed.sort_by(|a, b| (a.1.time, &a.1.machine, a.1.seq).cmp(&(b.1.time, &b.1.machine, b.1.seq)));
    let mut kept = Vec::with_capacity(lines.len());
    let mut current = Folded::new(ids::now_ms());
    for (line, record) in parsed {
        current.at = record.time;
        if let Some((_, terms)) = writers.merge.get(&record.machine) {
            if let Some(why) =
                terms.refusal_in(record.time, &record.op, &record.data, None, &payload_of(&record), &record.machine, &current)
            {
                if std::env::var_os("LAYR_TRACE").is_some() {
                    let (seq, machine, op) = (record.seq, ids::short(&record.machine), &record.op);
                    eprintln!("trace release: record {seq} of merge machine {machine} ({op}) has no effect: {why}");
                }
                continue;
            }
        } else if !writers.full.contains_key(&record.machine) {
            continue;
        }
        current.apply(&record);
        kept.push(line.clone());
    }
    Ok((writers, kept))
}

pub(super) fn model(project: &str, records: &reader::Records, machine: &str) -> Result<layr_model::Project> {
    if !records.gaps.is_empty() {
        bail!("remote record gaps prevent reading release files");
    }
    let (_, lines) = effective_lines(project, &records.lines, machine)?;
    let model = layr_model::Model::from_verified_lines(lines.iter().map(String::as_str), machine)?;
    model.projects.get(project).cloned().context("project is absent from remote history; push it first")
}

impl Store {
    fn release_model(&self) -> Result<layr_model::Project> {
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        model(&descriptor.project, &reader::records(&self.backend, &identity, &descriptor.project)?, &self.machine)
    }

    pub fn release_files(&mut self, ctx: &Ctx, name: &str) -> Result<Vec<File>> {
        self.connect(ctx)?;
        let model = self.release_model()?;
        if !model.ref_list(name, ids::now_ms()).iter().any(|r| r.name == name) {
            bail!("release ref '{name}' does not exist");
        }
        model.release_files(name, ids::now_ms())
    }

    /// Read the creator's main and any compatible, signed queue merge on the configured machine.
    fn main_snapshot(&mut self, ctx: &Ctx) -> Result<(layr_model::Project, String)> {
        self.connect(ctx)?;
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        let history = reader::records(&self.backend, &identity, &descriptor.project)?;
        if !history.gaps.is_empty() {
            bail!("remote record gaps prevent verifying main");
        }
        let (writers, lines) = effective_lines(&descriptor.project, &history.lines, &self.machine)?;
        let records: Vec<_> =
            lines.iter().filter_map(|line| crate::oplog::parse_line(line, None).ok()).map(|parsed| parsed.record).collect();
        let creator = layr_model::project_perspective(&descriptor.project, &records)?;
        let project = model(&descriptor.project, &history, &creator)?;
        let mains: Vec<_> = project.line_list().into_iter().filter(|line| line.name == "main").collect();
        if mains.len() != 1 {
            bail!("remote must have exactly one main line");
        }
        let main = &mains[0];
        if project.state(&main.head).is_none() {
            bail!("remote main state is missing");
        }
        let Some(value) = project.config.get("queue.merge-machine") else {
            return Ok((project, main.head.clone()));
        };
        let machine = value.as_str().context("invalid queue.merge-machine")?;
        let view = project.lines.get(&main.id).context("remote main line is missing")?;
        let Some(merged) = view.heads.get(machine) else { return Ok((project, main.head.clone())) };
        if !crate::model::is_uuid(machine) || writers.key(machine, ids::now_ms()).is_none() {
            bail!("queue.merge-machine is not a trusted machine");
        }
        if project.ancestors(&main.head).iter().any(|state| state == merged) {
            return Ok((project, main.head.clone()));
        }
        if project.state(merged).is_none() || !project.ancestors(merged).iter().any(|state| state == &main.head) {
            bail!("merge machine main does not extend creator main");
        }
        let completed = records.iter().any(|record| {
            let after = &record.data["queue_change"]["after"];
            record.machine == machine
                && record.op == "queue.result"
                && after["status"] == "done"
                && after["target_line"] == main.id
                && after["result"] == *merged
                && record.lines.get(&main.id).and_then(|change| change.after.as_ref()).is_some_and(|line| line.head == *merged)
        });
        if !completed {
            bail!("merge machine main head has no matching queue completion");
        }
        let merged = merged.clone();
        Ok((project, merged))
    }

    /// Read the accepted main head directly from signed remote history.
    pub fn verified_main_head(&mut self, ctx: &Ctx) -> Result<String> {
        Ok(self.main_snapshot(ctx)?.1)
    }

    /// Check the signed remote release ref against the remote main history.
    pub fn verify_release_main(&mut self, ctx: &Ctx, name: &str, expected: &str) -> Result<()> {
        let (project, head) = self.main_snapshot(ctx)?;
        let reference = project
            .ref_list(name, ids::now_ms())
            .into_iter()
            .find(|r| r.name == name)
            .context("release ref does not exist on the remote")?;
        if reference.target != expected {
            bail!("remote release ref does not match the expected state");
        }
        if project.state(expected).is_none() || !project.ancestors(&head).iter().any(|state| state == expected) {
            bail!("release state is not in remote main history");
        }
        Ok(())
    }

    /// Restore public files from the signed remote attachment, without writing project records.
    pub fn publish_release(&mut self, ctx: &Ctx, repo: &Repo, name: &str, expect: Option<&str>) -> Result<Vec<File>> {
        if repo.p.is_overlay() {
            bail!("release files belong to the parent project");
        }
        let before = repo.p.db.ref_value(name, ids::now_ms())?.context("release ref does not exist")?;
        if expect.is_some_and(|expected| expected != before.target) {
            bail!("release ref '{name}' changed: --expect does not match");
        }
        crate::cmd::refs::authorize_update(ctx, repo, name, Some(&before), Some(&before), true)?;
        repo.require(ctx, "push")?;
        self.connect(ctx)?;
        self.require_private_access()?;
        if let Some(person) = &self.person {
            crate::cmd::refs::authorize_person_update(repo, &person.login, name, Some(&before), Some(&before))?;
        }
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        let history = reader::records(&self.backend, &identity, &descriptor.project)?;
        let writers = layr_store::public::release_writers_checked(&descriptor.project, &history.lines)?;
        if writers.key(&self.machine, ids::now_ms()) != Some(&self.key.verifying_key()) {
            bail!("release publication requires a trusted writer or merge machine");
        }
        let initial = model(&descriptor.project, &history, &self.machine)?;
        if initial.ref_list(name, ids::now_ms()).iter().find(|r| r.name == name).map(|r| &r.target) != Some(&before.target) {
            bail!("release ref '{name}' changed on the remote; fetch before publishing");
        }
        let files =
            Files { reference: name.into(), state: before.target.clone(), files: initial.release_files(name, ids::now_ms())? };
        files.validate()?;
        if files.files.is_empty() {
            bail!("release has no signed files to publish");
        }
        if files.files.iter().any(|file| file.size > PUBLIC_MAX_BYTES) {
            bail!("a public release file is at most 32 MiB");
        }
        let mut plain = Vec::new();
        for file in &files.files {
            let (stream, identity) = self.backend.release_stream(&file.object, &identity)?;
            let stream = stream.take(release::MAX_OBJECT_BYTES + 1);
            let mut input: Box<dyn Read> = if identity == layr_store::keys::PLAIN {
                Box::new(stream)
            } else {
                let identity =
                    age::x25519::Identity::from_str(&identity).map_err(|_| anyhow::anyhow!("invalid release identity"))?;
                let decryptor = age::Decryptor::new(stream)?;
                Box::new(decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?)
            };
            let mut output = spool()?;
            let mut hash = ContentAddress::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > file.size {
                    bail!("release file size mismatch");
                }
                hash.update(&buffer[..n]);
                output.write_all(&buffer[..n])?;
            }
            if size != file.size || hex::encode(hash.finalize()) != file.sha256 {
                bail!("release file SHA-256 or size mismatch");
            }
            output.seek(SeekFrom::Start(0))?;
            plain.push((file.name.clone(), output));
        }
        if repo.p.db.ref_value(name, ids::now_ms())?.as_ref() != Some(&before) {
            bail!("release ref '{name}' changed during publication");
        }
        let history = reader::records(&self.backend, &identity, &descriptor.project)?;
        let writers = layr_store::public::release_writers_checked(&descriptor.project, &history.lines)?;
        if writers.key(&self.machine, ids::now_ms()) != Some(&self.key.verifying_key()) {
            bail!("release publication requires a trusted writer or merge machine");
        }
        let current = model(&descriptor.project, &history, &self.machine)?;
        if current.ref_list(name, ids::now_ms()).iter().find(|r| r.name == name).map(|r| &r.target) != Some(&before.target) {
            bail!("release ref '{name}' changed on the remote during publication");
        }
        if current.release_files(name, ids::now_ms())? != files.files {
            bail!("release files changed on the remote during publication");
        }
        super::epochs::check_current(self)?;
        crate::cmd::refs::authorize_update(ctx, repo, name, Some(&before), Some(&before), true)?;
        if let Some(person) = &self.person {
            crate::cmd::refs::authorize_person_update(repo, &person.login, name, Some(&before), Some(&before))?;
        }
        self.publish_plain(&files, plain)?;
        Ok(files.files)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn attach_release(
        &mut self,
        ctx: &Ctx,
        repo: &mut Repo,
        name: &str,
        paths: &[String],
        rename: Option<&str>,
        expect: Option<&str>,
        public: bool,
    ) -> Result<Files> {
        if paths.is_empty() || paths.len() > release::MAX_FILES {
            bail!("attach needs 1 to 64 files");
        }
        if rename.is_some() && paths.len() != 1 {
            bail!("--name needs exactly one file");
        }
        let before = repo.p.db.ref_value(name, ids::now_ms())?.context("release ref does not exist")?;
        if expect.is_some_and(|expected| expected != before.target) {
            bail!("release ref '{name}' changed: --expect does not match");
        }
        crate::cmd::refs::authorize_update(ctx, repo, name, Some(&before), Some(&before), true)?;
        repo.require(ctx, "push")?;
        if repo.p.is_overlay() {
            bail!("release files belong to the parent project");
        }
        self.connect(ctx)?;
        self.require_private_access()?;
        let authorize = || -> Result<()> {
            crate::cmd::refs::authorize_update(ctx, repo, name, Some(&before), Some(&before), true)?;
            if let Some(person) = &self.person {
                crate::cmd::refs::authorize_person_update(repo, &person.login, name, Some(&before), Some(&before))?;
            }
            Ok(())
        };
        authorize()?;
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        let history = reader::records(&self.backend, &identity, &descriptor.project)?;
        let local = repo.p.db.all_lines()?;
        let trust_source =
            if history.lines.iter().any(|l| crate::oplog::parse_line(l, None).is_ok_and(|r| r.record.op == "project")) {
                &history.lines
            } else {
                &local
            };
        let writers = layr_store::public::release_writers_checked(&descriptor.project, trust_source)?;
        if writers.key(&self.machine, ids::now_ms()) != Some(&self.key.verifying_key()) {
            bail!("release attach requires a trusted writer or merge machine");
        }
        if history.lines.iter().any(|l| crate::oplog::parse_line(l, None).is_ok_and(|r| r.record.op == "project")) {
            check_ref(&model(&descriptor.project, &history, &self.machine)?, name, &before.target)?;
        } else if !history.gaps.is_empty()
            || history.lines.iter().any(|l| crate::oplog::parse_line(l, None).is_ok_and(|r| r.record.op != "machine"))
        {
            bail!("remote history has no project creator");
        }
        let mut prepared = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for path in paths {
            let path = ctx.cwd.join(path);
            let filename = rename
                .map(str::to_owned)
                .or_else(|| path.file_name()?.to_str().map(str::to_owned))
                .context("file has no UTF-8 name")?;
            if !release::filename(&filename) || !names.insert(filename.to_lowercase()) {
                bail!("invalid or duplicate release filename: {filename}");
            }
            let file = crate::privs::as_caller_fs(ctx, || {
                let f = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path)?;
                if !f.metadata()?.is_file() || f.metadata()?.len() > release::MAX_FILE_BYTES {
                    bail!("release files must be regular files of at most 512 MiB");
                }
                Ok(f)
            })?;
            // The gateway serves public files up to 32 MiB: refuse before anything is written.
            if public && file.metadata()?.len() > PUBLIC_MAX_BYTES {
                bail!("{filename}: a public release file is at most 32 MiB");
            }
            prepared.push((filename, file));
        }
        let descriptor = self.descriptor()?;
        // Ensure the release ref and its state have a remote caller before publishing its files.
        writer::push(self, ctx, repo, &Options::default())?;
        let recipient = self.descriptor()?.recipient;
        if descriptor.recipient != recipient {
            bail!("project epoch changed; retry attach");
        }
        let mut files = Vec::new();
        let mut plain = Vec::new();
        let public_version =
            if public { Some(name.strip_prefix("release/").context("release ref must start with release/")?) } else { None };
        for (name, file) in prepared {
            // Capture public bytes during hashing so later source changes cannot alter the release.
            let (ciphertext, public_copy, size, sha256) = encrypt(file, &recipient, public)?;
            if let Some(version) = public_version {
                let key = format!("public/release/{version}/{name}");
                if let Some(existing) = self.backend.get(&key)? {
                    if existing.len() as u64 != size || hex::encode(ContentAddress::digest(&existing)) != sha256 {
                        bail!("{key} already holds other bytes");
                    }
                }
            }
            let object = format!("files/{}.age", ids::new_id());
            if !self.backend.put_file_new(&object, ciphertext)? {
                bail!("release object id already exists; retry attach");
            }
            if let Some(copy) = public_copy {
                plain.push((name.clone(), copy));
            }
            files.push(File { name, size, sha256, object });
        }
        let lock = repo.p.lock()?;
        if repo.p.db.ref_value(name, ids::now_ms())?.as_ref().map(|r| &r.target) != Some(&before.target) {
            bail!("release ref '{name}' changed during attach");
        }
        check_ref(&self.release_model()?, name, &before.target)?;
        super::epochs::check_current(self)?;
        crate::cmd::refs::authorize_update(ctx, repo, name, Some(&before), Some(&before), true)?;
        if let Some(person) = &self.person {
            crate::cmd::refs::authorize_person_update(repo, &person.login, name, Some(&before), Some(&before))?;
        }
        let files = Files { reference: name.into(), state: before.target, files };
        files.validate()?;
        if public {
            let version = files.reference.strip_prefix("release/").context("release ref must start with release/")?;
            let list: Vec<serde_json::Value> =
                files.files.iter().map(|f| serde_json::json!({"name": f.name, "size": f.size, "sha256": f.sha256})).collect();
            let body = serde_json::to_vec(&serde_json::json!({"version": version, "state": files.state, "files": list}))?;
            let key = format!("public/release/{version}/files.json");
            if self.backend.get(&key)?.is_some_and(|existing| existing != body) {
                bail!("{key} already lists other files");
            }
        }
        let mut record = Record::new("release.files");
        record.data = serde_json::to_value(&files)?;
        repo.commit_record(ctx, record)?;
        writer::push_locked(self, ctx, repo, &Options::default())?;
        // The public copies are plain remote objects: their upload (minutes for large files) holds no project lock.
        drop(lock);
        check_ref(&self.release_model()?, name, &files.state)?;
        if public {
            self.publish_plain(&files, plain)?;
        }
        Ok(files)
    }

    /// `--public-files`: each file as plaintext at `public/release/<version>/<name>`, then the list of names,
    /// sizes and SHA-256 at `public/release/<version>/files.json`, all create-only, the list last. The gateway serves
    /// them without sign-in (`/p/<project>/release/<version>/<name>`, `.../latest`); the signed `release.files`
    /// record binds the same digests. A retry after a partial publish finds the same bytes and goes on.
    fn publish_plain(&self, files: &Files, plain: Vec<(String, fs::File)>) -> Result<()> {
        let version = files.reference.strip_prefix("release/").context("release ref must start with release/")?;
        for (name, mut file) in plain {
            let entry = files.files.iter().find(|f| f.name == name).context("public file is not in the release list")?;
            file.seek(SeekFrom::Start(0))?;
            let key = format!("public/release/{version}/{name}");
            if !self.backend.put_file_new(&key, file)? {
                let body = self.backend.get(&key)?.with_context(|| format!("{key} exists but cannot be read"))?;
                if hex::encode(ContentAddress::digest(&body)) != entry.sha256 {
                    bail!("{key} already holds other bytes");
                }
            }
        }
        let list: Vec<serde_json::Value> =
            files.files.iter().map(|f| serde_json::json!({"name": f.name, "size": f.size, "sha256": f.sha256})).collect();
        let body = serde_json::to_vec(&serde_json::json!({"version": version, "state": files.state, "files": list}))?;
        let key = format!("public/release/{version}/files.json");
        if !self.backend.put_new(&key, body.clone())? && self.backend.get(&key)?.as_deref() != Some(body.as_slice()) {
            bail!("{key} already lists other files");
        }
        Ok(())
    }

    pub fn get_release_file(&mut self, ctx: &Ctx, file: &File, directory: &Path) -> Result<()> {
        self.connect(ctx)?;
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        let (stream, identity) = self.backend.release_stream(&file.object, &identity)?;
        let stream = stream.take(release::MAX_OBJECT_BYTES + 1);
        let mut plain: Box<dyn Read> = if identity == layr_store::keys::PLAIN {
            Box::new(stream)
        } else {
            let identity = age::x25519::Identity::from_str(&identity).map_err(|_| anyhow::anyhow!("invalid release identity"))?;
            let decryptor = age::Decryptor::new(stream)?;
            Box::new(decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?)
        };
        crate::privs::as_caller_fs(ctx, || {
            fs::create_dir_all(directory)?;
            let directory = fs::canonicalize(directory)?;
            let output = directory.join(&file.name);
            let temporary = directory.join(format!(".layr-release-{}", ids::new_id()));
            let result = (|| -> Result<()> {
                let mut out = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                let mut hash = ContentAddress::new();
                let mut size = 0;
                let mut buffer = [0u8; 65536];
                loop {
                    let n = plain.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    size += n as u64;
                    if size > file.size || size > release::MAX_FILE_BYTES {
                        bail!("release file size mismatch");
                    }
                    hash.update(&buffer[..n]);
                    out.write_all(&buffer[..n])?;
                }
                if size != file.size || hex::encode(hash.finalize()) != file.sha256 {
                    bail!("release file SHA-256 or size mismatch");
                }
                out.sync_all()?;
                fs::hard_link(&temporary, &output)
                    .with_context(|| format!("save {} (existing files are preserved)", output.display()))?;
                fs::File::open(&directory)?.sync_all()?;
                Ok(())
            })();
            let _ = fs::remove_file(&temporary);
            result
        })
    }
}

fn check_ref(remote: &layr_model::Project, name: &str, expected: &str) -> Result<()> {
    if !remote.ref_log(name).is_empty()
        && remote.ref_list(name, ids::now_ms()).iter().find(|r| r.name == name).map(|r| r.target.as_str()) != Some(expected)
    {
        bail!("release ref '{name}' changed on the remote; fetch before attaching");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with(bytes: &[u8]) -> fs::File {
        let mut file = spool().unwrap();
        file.write_all(bytes).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    #[test]
    fn a_plain_store_keeps_release_bytes_and_a_secret_store_encrypts_them() {
        let bytes = b"release archive bytes".repeat(1000);
        let mut plain = String::new();
        let (mut output, _, size, sha256) = encrypt(file_with(&bytes), layr_store::keys::PLAIN, false).unwrap();
        output.read_to_string(&mut plain).unwrap();
        assert_eq!((plain.as_bytes(), size), (&bytes[..], bytes.len() as u64));
        assert_eq!(sha256, hex::encode(ContentAddress::digest(&bytes)));
        let identity = layr_store::keys::generate_identity();
        let recipient = layr_store::keys::recipient_of(&identity).unwrap();
        let (mut output, _, _, same) = encrypt(file_with(&bytes), &recipient, false).unwrap();
        let mut sealed = Vec::new();
        output.read_to_end(&mut sealed).unwrap();
        assert!(sealed.starts_with(b"age-encryption.org/"));
        assert_eq!(layr_store::keys::decrypt(&identity, &sealed).unwrap(), bytes);
        assert_eq!(same, sha256);
    }

    #[test]
    fn public_copy_keeps_the_bytes_that_were_hashed_when_source_changes() {
        let source = file_with(b"original release");
        let mut changed = source.try_clone().unwrap();
        let (_, copy, size, digest) = encrypt(source, layr_store::keys::PLAIN, true).unwrap();
        changed.seek(SeekFrom::Start(0)).unwrap();
        changed.write_all(b"modified release").unwrap();
        let mut bytes = Vec::new();
        copy.unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original release");
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(digest, hex::encode(ContentAddress::digest(&bytes)));
    }

    #[test]
    fn public_copy_rechecks_the_limit_as_it_reads() {
        let source = spool().unwrap();
        source.set_len(PUBLIC_MAX_BYTES + 1).unwrap();
        let error = encrypt(source, layr_store::keys::PLAIN, true).err().unwrap();
        assert!(error.to_string().contains("32 MiB"));
    }
}

#[cfg(test)]
#[path = "release_authority_tests.rs"]
mod authority_tests;
