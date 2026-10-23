//! Immutable objects in a folder or an S3 bucket.

use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Object {
    pub key: String,
    pub modified: i64,
}

pub enum Backend {
    File(PathBuf, u32, u32),
    S3(super::s3::S3),
}

impl Backend {
    pub fn file(root: PathBuf) -> Self {
        Self::File(root, rustix::process::geteuid().as_raw(), rustix::process::getegid().as_raw())
    }
    pub fn as_caller(&mut self, ctx: &crate::ctx::Ctx) {
        if let Self::File(_, uid, gid) = self {
            *uid = ctx.caller.uid;
            *gid = ctx.caller.gid;
        }
    }

    pub fn open(url: &str) -> Result<Self> {
        if url.starts_with("file://") {
            let parsed = reqwest::Url::parse(url).context("invalid file remote URL")?;
            if parsed.query().is_some() || parsed.fragment().is_some() {
                bail!("a file remote cannot have a query or fragment");
            }
            let path = parsed.to_file_path().map_err(|_| anyhow::anyhow!("a file remote needs an absolute local path"))?;
            return Ok(Self::file(path));
        }
        if let Some(path) = url.strip_prefix("s3://") {
            let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
            if bucket.is_empty() || bucket.contains(['@', ':', '?', '#']) || prefix.contains(['?', '#']) {
                bail!("invalid S3 bucket");
            }
            return Ok(Self::S3(super::s3::S3::with_env(bucket, prefix, &std::collections::HashMap::new())?));
        }
        bail!("store URL must start with file:// or s3://")
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        match self {
            Self::S3(s) => s.get(key),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                let path = crate::fsutil::safe_join(root, key)?;
                let mut f = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e).with_context(|| format!("read remote object {key}")),
                };
                if !f.metadata()?.is_file() {
                    bail!("remote object is not a file: {key}");
                }
                let mut data = Vec::new();
                f.read_to_end(&mut data)?;
                Ok(Some(data))
            }),
        }
    }

    /// Publish a whole object or leave its destination absent. Never replace an object.
    pub fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        check_key(key)?;
        match self {
            Self::S3(s) => s.put_new(key, body),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                fs::create_dir_all(root)?;
                let path = crate::fsutil::safe_join(root, key)?;
                let parent = path.parent().expect("object parent");
                fs::create_dir_all(parent)?;
                crate::fsutil::safe_join(root, key)?;
                let temporary = parent.join(format!(".put-{}", crate::ids::new_id()));
                let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                let result = (|| -> Result<bool> {
                    f.write_all(&body)?;
                    f.sync_all()?;
                    match fs::hard_link(&temporary, &path) {
                        Ok(()) => {
                            fs::File::open(parent)?.sync_all()?;
                            Ok(true)
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                        Err(e) => Err(e.into()),
                    }
                })();
                let _ = fs::remove_file(temporary);
                result
            }),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        if !prefix.is_empty() {
            check_key(prefix.trim_end_matches('/'))?;
        }
        match self {
            Self::S3(s) => s.list(prefix),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                let mut out = Vec::new();
                if root.exists() {
                    walk(root, root, prefix, &mut out)?;
                }
                out.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(out)
            }),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match self {
            Self::S3(s) => s.delete(key),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                let path = crate::fsutil::safe_join(root, key)?;
                match fs::remove_file(path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }),
        }
    }
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<Object>) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let path = e.path();
        let name = path.strip_prefix(root)?.to_str().context("non-UTF-8 remote object")?.to_string();
        let m = fs::symlink_metadata(&path)?;
        if m.file_type().is_symlink() {
            bail!("symbolic link in remote store: {name}");
        }
        if m.is_dir() {
            if prefix.starts_with(&format!("{name}/")) || name.starts_with(prefix) {
                walk(root, &path, prefix, out)?;
            }
        } else if name.starts_with(prefix) && !e.file_name().to_string_lossy().starts_with(".put-") {
            let modified = m.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
            out.push(Object { key: name, modified });
        }
    }
    Ok(())
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.split('/').any(|p| p.is_empty() || p == "." || p == "..") || key.contains(['\\', '\0']) {
        bail!("invalid remote object key: {key:?}");
    }
    crate::fsutil::check_rel(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_is_exclusive_and_never_changes_bytes() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let s = Backend::file(root.clone());
        assert!(s.put_new("states/a", b"first".to_vec()).unwrap());
        assert!(!s.put_new("states/a", b"second".to_vec()).unwrap());
        assert_eq!(s.get("states/a").unwrap().unwrap(), b"first");
        assert_eq!(s.list("states/").unwrap().len(), 1);
        assert!(s.put_new("../bad", vec![]).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
