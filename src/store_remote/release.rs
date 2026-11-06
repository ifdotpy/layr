//! Stream built release files through the existing epoch vault and immutable object transport.
use super::{reader, writer, Options, Store};
use crate::{ctx::Ctx, ids, model::Record, repo::Repo};
use anyhow::{bail, Context, Result};
use layr_model::release::{self, File, Files};
use layr_hashers::{ContentAddress, Digest};
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
fn encrypt(mut input: fs::File, recipient: &str) -> Result<(fs::File, u64, String)> {
    let mut output = spool()?;
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
        hash.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
    }
    writer.finish()?;
    output.seek(SeekFrom::Start(0))?;
    Ok((output, size, hex::encode(hash.finalize())))
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

pub(super) fn model(project: &str, records: &reader::Records, machine: &str) -> Result<layr_model::Project> {
    if !records.gaps.is_empty() {
        bail!("remote record gaps prevent reading release files");
    }
    let trusted = layr_store::public::trusted_writers_checked(project, &records.lines)?;
    let lines = records
        .lines
        .iter()
        .filter(|line| crate::oplog::parse_line(line, None).is_ok_and(|r| trusted.contains_key(&r.record.machine)))
        .map(String::as_str);
    let model = layr_model::Model::from_verified_lines(lines, machine)?;
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
        let trusted = layr_store::public::trusted_writers_checked(&descriptor.project, trust_source)?;
        if trusted.get(&self.machine) != Some(&self.key.verifying_key()) {
            bail!("release attach requires a trusted writer machine");
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
        for (name, file) in prepared {
            // The same open file, read again from its start for the public copy (`--public-files`).
            let again = if public { Some(file.try_clone()?) } else { None };
            let (ciphertext, size, sha256) = encrypt(file, &recipient)?;
            let object = format!("files/{}.age", ids::new_id());
            if !self.backend.put_file_new(&object, ciphertext)? {
                bail!("release object id already exists; retry attach");
            }
            if let Some(again) = again {
                plain.push((name.clone(), again));
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
        let list: Vec<serde_json::Value> = files
            .files
            .iter()
            .map(|f| serde_json::json!({"name": f.name, "size": f.size, "sha256": f.sha256}))
            .collect();
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
        let (mut output, size, sha256) = encrypt(file_with(&bytes), layr_store::keys::PLAIN).unwrap();
        output.read_to_string(&mut plain).unwrap();
        assert_eq!((plain.as_bytes(), size), (&bytes[..], bytes.len() as u64));
        assert_eq!(sha256, hex::encode(ContentAddress::digest(&bytes)));
        let identity = layr_store::keys::generate_identity();
        let recipient = layr_store::keys::recipient_of(&identity).unwrap();
        let (mut output, _, same) = encrypt(file_with(&bytes), &recipient).unwrap();
        let mut sealed = Vec::new();
        output.read_to_end(&mut sealed).unwrap();
        assert!(sealed.starts_with(b"age-encryption.org/"));
        assert_eq!(layr_store::keys::decrypt(&identity, &sealed).unwrap(), bytes);
        assert_eq!(same, sha256);
    }
}
