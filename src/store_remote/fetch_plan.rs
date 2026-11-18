//! Best-effort operation cache; downloads and decoding remain in the reader.

use super::super::{
    format::{Entry, Tree},
    keys,
};
use std::{collections::BTreeMap, path::PathBuf};

/// Live ciphertext budget. Oversized objects and failed writes bypass this cache.
pub(super) const MATERIAL_BYTES: u64 = 2 << 30;

pub(super) struct Material {
    dir: PathBuf,
    identity: String,
    recipient: age::x25519::Recipient,
    pub entries: BTreeMap<String, u64>,
    pub bytes: u64,
    limit: u64,
}

impl Material {
    /// `dir` is a pid-named path supplied by the local replica's swept work folder.
    pub fn new(dir: PathBuf) -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).ok()?.parse().ok()?;
        std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
        Some(Self { dir, identity, recipient, entries: BTreeMap::new(), bytes: 0, limit: MATERIAL_BYTES })
    }

    fn path(&self, key: &str) -> PathBuf {
        use layr_hashers::{ContentAddress, Digest};
        self.dir.join(hex::encode(ContentAddress::digest(key.as_bytes())))
    }

    pub fn remove(&mut self, key: &str) {
        if let Some(&length) = self.entries.get(key) {
            match std::fs::remove_file(self.path(key)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return,
            }
            self.entries.remove(key);
            self.bytes -= length;
        }
    }

    pub fn put(&mut self, key: &str, body: &[u8]) {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        self.remove(key);
        let bound = keys::ciphertext_len(body.len()) as u64;
        if bound > self.limit || self.entries.contains_key(key) {
            return;
        }
        while self.bytes + bound > self.limit {
            let Some(old) = self.entries.keys().next().cloned() else { return };
            self.remove(&old);
            if self.entries.contains_key(&old) {
                return;
            }
        }
        let temporary = self.dir.join(".part");
        let written = (|| -> anyhow::Result<u64> {
            let file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
            let encryptor = age::Encryptor::with_recipients(std::iter::once(&self.recipient as &dyn age::Recipient))?;
            let mut writer = encryptor.wrap_output(file)?;
            writer.write_all(body)?;
            let length = writer.finish()?.metadata()?.len();
            std::fs::rename(&temporary, self.path(key))?;
            Ok(length)
        })();
        match written {
            Ok(length) => {
                self.entries.insert(key.into(), length);
                self.bytes += length;
            }
            Err(_) => {
                let _ = std::fs::remove_file(temporary);
            }
        }
    }

    pub fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let length = *self.entries.get(key)?;
        let path = self.path(key);
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() == length) {
            if let Ok(body) = std::fs::read(&path) {
                if body.len() as u64 == length {
                    if let Ok(plain) = keys::decrypt(&self.identity, &body) {
                        return Some(plain);
                    }
                }
            }
        }
        self.remove(key);
        None
    }
}
impl Drop for Material {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(super) fn unchanged(tree: &Tree, base: Option<&Tree>, path: &str, entry: &Entry) -> bool {
    base.and_then(|b| b.files.get(path))
        .is_some_and(|old| (old.0.as_str(), old.1, old.2.as_str()) == (entry.0.as_str(), entry.1, entry.2.as_str()))
        && base.and_then(|b| b.times.get(path)) == tree.times.get(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn material_evicts_and_reclaims_live_bytes_without_failing_the_operation() {
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        let mut material = Material::new(dir.clone()).unwrap();
        material.limit = 4096;
        for n in 0..20 {
            material.put(&format!("part-{n}"), &[1; 1024]);
            assert!(material.bytes <= material.limit);
        }
        assert!(!material.entries.contains_key("part-0"));
        assert!(material.get("part-19").is_some());
        material.put("part-19", b"replacement");
        assert_eq!(material.bytes, material.entries.values().sum::<u64>());
        let path = material.path("part-19");
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(material.get("part-19").is_none());
        assert!(!path.exists());
        assert_eq!(material.bytes, material.entries.values().sum::<u64>());
        material.put("oversized", &[0; 8192]);
        assert!(!material.entries.contains_key("oversized"));
        let keys = material.entries.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            material.remove(&key);
        }
        assert_eq!(material.bytes, 0);
        std::fs::remove_dir_all(&dir).unwrap();
        material.put("io-error", b"cache errors are optional");
        assert!(material.get("io-error").is_none());
    }
}
