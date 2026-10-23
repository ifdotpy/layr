//! The host config and atomic updates of token hashes. No store is opened here.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const CONFIG_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub reader_role_arn: String,
    pub writer_role_arn: String,
    pub gc_role_arn: String,
    pub recipient: String,
    #[serde(default)]
    pub write_token_hashes: Vec<String>,
    #[serde(default)]
    pub gc_token_hashes: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Project {
    pub fn key(&self, name: &str) -> String {
        format!("{}/{}", self.prefix.trim_end_matches('/'), name)
    }

    pub fn binding(&self, name: &str) -> String {
        serde_json::json!([name, self.bucket, self.region, self.prefix, self.reader_role_arn, self.recipient]).to_string()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Config {
    pub host: String,
    pub listen: SocketAddr,
    pub projects: BTreeMap<String, Project>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub fn project_name(name: &str) -> bool {
    (1..=44).contains(&name.len()) && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.host.is_empty() || self.host.len() > 253
            || !self.host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        { bail!("gateway host must be a DNS name"); }
        if self.projects.len() > 1024 { bail!("too many gateway projects"); }
        let role = regex::Regex::new(r"^arn:(aws|aws-cn|aws-us-gov):iam::[0-9]{12}:role/[A-Za-z0-9+=,.@_/-]{1,512}$")?;
        for (name, p) in &self.projects {
            if !project_name(name) { bail!("invalid gateway project name"); }
            if !(3..=63).contains(&p.bucket.len())
                || !p.bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
                || !(3..=63).contains(&p.region.len())
                || !p.region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            { bail!("invalid bucket or region for project {name}"); }
            let prefix = p.prefix.trim_end_matches('/');
            if prefix.is_empty() || prefix.len() > 512 || prefix.starts_with('/')
                || !prefix.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
                || prefix.split('/').any(|s| s.is_empty() || matches!(s, "." | ".."))
            { bail!("invalid store prefix for project {name}"); }
            if [&p.reader_role_arn, &p.writer_role_arn, &p.gc_role_arn].iter().any(|arn| !role.is_match(arn)) {
                bail!("invalid project role ARN for {name}");
            }
            p.recipient.parse::<age::x25519::Recipient>().map_err(|_| anyhow!("invalid store recipient for {name}"))?;
            for hashes in [&p.write_token_hashes, &p.gc_token_hashes] {
                if hashes.len() > 4096 || hashes.iter().any(|h| h.len() != 64
                    || !h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                { bail!("invalid token hashes for project {name}"); }
            }
        }
        Ok(())
    }
}

pub fn read(path: &Path) -> Result<Config> {
    let file = File::open(path)?;
    if file.metadata()?.mode() & 0o022 != 0 { bail!("gateway config must not be writable by group or others"); }
    let mut data = Vec::new();
    file.take(CONFIG_LIMIT + 1).read_to_end(&mut data)?;
    if data.len() as u64 > CONFIG_LIMIT { bail!("gateway config is too large"); }
    let config: Config = serde_json::from_slice(&data).context("invalid gateway config JSON")?;
    config.validate()?;
    Ok(config)
}

type Stamp = (u64, u64, i64, i64);
pub struct ConfigFile {
    path: PathBuf,
    loaded: Mutex<Option<(Stamp, Arc<Config>)>>,
}

impl ConfigFile {
    pub fn new(path: PathBuf) -> Self { Self { path, loaded: Mutex::new(None) } }
    pub fn get(&self) -> Result<Arc<Config>> {
        let meta = fs::metadata(&self.path)?;
        if meta.mode() & 0o022 != 0 { bail!("gateway config must not be writable by group or others"); }
        let stamp = (meta.ino(), meta.len(), meta.mtime(), meta.mtime_nsec());
        let mut loaded = self.loaded.lock().map_err(|_| anyhow!("gateway config lock failed"))?;
        if let Some((old, config)) = loaded.as_ref() {
            if *old == stamp { return Ok(config.clone()); }
        }
        let config = Arc::new(read(&self.path)?);
        *loaded = Some((stamp, config.clone()));
        Ok(config)
    }
}

pub fn add_hash(path: &Path, project: &str, hash: String, gc: bool) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().context("config path needs a file name")?.to_string_lossy();
    let lock = OpenOptions::new().write(true).create(true).truncate(false)
        .custom_flags(libc::O_NOFOLLOW).mode(0o600).open(parent.join(format!(".{name}.lock")))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() { bail!("gateway config must be a regular file"); }
    let mut config = read(path)?;
    let p = config.projects.get_mut(project).context("unknown gateway project")?;
    let hashes = if gc { &mut p.gc_token_hashes } else { &mut p.write_token_hashes };
    if hashes.len() >= 4096 { bail!("too many token hashes"); }
    hashes.push(hash);
    let data = serde_json::to_vec_pretty(&config)?;
    if data.len() as u64 > CONFIG_LIMIT { bail!("gateway config is too large"); }
    let temporary = parent.join(format!(".{name}.token-{:016x}", rand::random::<u64>()));
    let mut created = false;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        created = true;
        file.write_all(&data)?;
        file.write_all(b"\n")?;
        // Preserve readability for the dynamic service user, even under umask 077.
        file.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o777))?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() && created { let _ = fs::remove_file(temporary); }
    result
}

#[cfg(test)]
pub fn sample() -> Config {
    let role = |kind| format!("arn:aws:iam::123456789012:role/layr-store-{kind}-example");
    Config { host: "localhost".into(), listen: "127.0.0.1:18095".parse().unwrap(), extra: BTreeMap::new(),
        projects: [("example".into(), Project { bucket: "layr-example".into(), region: "eu-central-1".into(),
            prefix: "example/".into(), reader_role_arn: role("reader"), writer_role_arn: role("writer"),
            gc_role_arn: role("gc"), recipient: age::x25519::Identity::generate().to_public().to_string(),
            write_token_hashes: Vec::new(), gc_token_hashes: Vec::new(), extra: BTreeMap::new() })].into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_updates_are_atomic_and_preserve_hashes_and_extensions() {
        let home = std::env::temp_dir().join(format!("layr-gateway-config-{:016x}", rand::random::<u64>()));
        fs::create_dir(&home).unwrap();
        let path = home.join("config.json");
        let mut config = sample();
        config.extra.insert("future".into(), serde_json::json!(true));
        fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let file = ConfigFile::new(path.clone());
        assert!(file.get().unwrap().projects["example"].write_token_hashes.is_empty());
        std::thread::scope(|scope| {
            for n in 0..8 { let path = &path; scope.spawn(move || add_hash(path, "example", format!("{n:064x}"), false).unwrap()); }
        });
        let updated = file.get().unwrap();
        assert_eq!(updated.projects["example"].write_token_hashes.len(), 8);
        assert!(updated.projects["example"].gc_token_hashes.is_empty());
        assert_eq!(updated.extra["future"], true);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        fs::write(&path, b"not json").unwrap();
        assert!(file.get().is_err());
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn unsafe_prefixes_roles_and_plain_tokens_are_refused() {
        for prefix in ["", "../other", "/other", "example//other", "example/../other"] {
            let mut config = sample();
            config.projects.get_mut("example").unwrap().prefix = prefix.into();
            assert!(config.validate().is_err(), "{prefix}");
        }
        let mut config = sample();
        config.projects.get_mut("example").unwrap().writer_role_arn = "*".into();
        assert!(config.validate().is_err());
        let mut config = sample();
        config.projects.get_mut("example").unwrap().write_token_hashes.push("plain token".into());
        assert!(config.validate().is_err());
    }
}
