//! Disposable metadata streams for immutable states. No records depend on this cache.

use crate::{fsutil, model, sys};
use layr_hashers::{FastCryptographic, PurposeHasher};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const MAX_ENTRY: u64 = 8 << 20;
const MAX_TOTAL: u64 = 32 << 20;
const MAX_ENTRIES: usize = 256;

pub struct Cache {
    dir: PathBuf,
    name: String,
}

struct Identity {
    filesystem: [u8; 16],
    uuid: [u8; 16],
    generation: u64,
    ctransid: u64,
}

fn cache_name(ids: &[Identity; 2]) -> String {
    let mut hash = FastCryptographic::new();
    hash.update(b"layr-no-data-v2");
    for id in ids {
        hash.update(&id.filesystem);
        hash.update(&id.uuid);
        hash.update(&id.generation.to_le_bytes());
        hash.update(&id.ctransid.to_le_bytes());
    }
    hex::encode(hash.finalize().as_bytes())
}

impl Cache {
    pub fn for_states(parent: &Path, child: &Path, p: &File, c: &File) -> Option<Self> {
        let states = parent.parent()?;
        if child.parent()? != states || states.file_name()? != "states" {
            return None;
        }
        for path in [parent, child] {
            if !model::is_uuid(path.file_name()?.to_str()?) {
                return None;
            }
        }
        let meta = states.parent()?.join("meta");
        let trusted = |path: &Path| -> Option<()> {
            let m = fsutil::open_dir_nofollow(path).ok()?.metadata().ok()?;
            (m.uid() == rustix::process::geteuid().as_raw() && m.mode() & 0o077 == 0).then_some(())
        };
        trusted(&meta)?;
        let dir = meta.join("diff-cache");
        if !dir.exists() {
            use std::os::unix::fs::DirBuilderExt;
            let _ = fs::DirBuilder::new().mode(0o700).create(&dir);
        }
        trusted(&dir)?;
        let identity = |file: &File| -> Option<Identity> {
            if sys::subvol_flags(file.as_fd()).ok()? & sys::SUBVOL_RDONLY == 0 {
                return None;
            }
            let info = sys::subvol_info(file.as_fd()).ok()?;
            Some(Identity { filesystem: sys::filesystem_uuid(file.as_fd()).ok()?, uuid: info.uuid, generation: info.generation, ctransid: info.ctransid })
        };
        Some(Self { dir, name: cache_name(&[identity(p)?, identity(c)?]) })
    }

    pub fn read(&self) -> Option<Vec<u8>> {
        let dir = fsutil::open_dir_nofollow(&self.dir).ok()?;
        let f = fsutil::open_file_at(&dir, &self.name).ok()?;
        let len = f.metadata().ok()?.len();
        if !(32..=MAX_ENTRY).contains(&len) {
            return None;
        }
        let mut data = Vec::new();
        f.take(MAX_ENTRY + 1).read_to_end(&mut data).ok()?;
        if data.len() as u64 != len || <FastCryptographic as PurposeHasher>::hash(&data[32..])[..] != data[..32] {
            return None;
        }
        Some(data.split_off(32))
    }

    pub fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        let size = data.len() as u64 + 32;
        if size > MAX_ENTRY {
            return Ok(());
        }
        // The checksum needs no lock.
        let mut saved = <FastCryptographic as PurposeHasher>::hash(data).to_vec();
        saved.extend_from_slice(data);
        // This cache is disposable. Publish atomically, but do not force a disk transaction.
        // A partial write after a crash fails the checksum and causes a fresh stream. The write
        // happens before the lock; only the eviction and the rename are under it.
        let tmp = self.dir.join(format!(".tmp-{}", crate::ids::new_id()));
        let result = (|| -> anyhow::Result<()> {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(&saved)?;
            drop(f);
            let dir = fsutil::open_dir_nofollow(&self.dir)?;
            // Readers can use an entry while another command replaces or removes its name. The
            // cache is disposable: when another command holds the lock, skip saving.
            match rustix::fs::flock(&dir, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let mut entries = Vec::new();
            let mut total = 0;
            for entry in fs::read_dir(&self.dir)?.flatten() {
                if entry.file_name().to_string_lossy().starts_with(".tmp-") {
                    continue;
                }
                let m = fs::symlink_metadata(entry.path())?;
                if m.is_file() {
                    total += m.len();
                    entries.push((m.modified()?, entry.path(), m.len()));
                }
            }
            entries.sort();
            let mut count = entries.len();
            for (_, path, len) in entries {
                if total + size <= MAX_TOTAL && count < MAX_ENTRIES {
                    break;
                }
                fs::remove_file(path)?;
                total -= len;
                count -= 1;
            }
            fs::rename(&tmp, self.dir.join(&self.name))?;
            Ok(())
        })();
        let _ = fs::remove_file(&tmp);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layr_hashers::{ContentAddress, Digest};

    #[test]
    fn v2_key_does_not_select_v1_entry() {
        let ids = [
            Identity { filesystem: [1; 16], uuid: [2; 16], generation: 3, ctransid: 4 },
            Identity { filesystem: [5; 16], uuid: [6; 16], generation: 7, ctransid: 8 },
        ];
        let mut old = <ContentAddress as Digest>::new();
        Digest::update(&mut old, b"layr-no-data-v1");
        for id in &ids {
            Digest::update(&mut old, id.filesystem);
            Digest::update(&mut old, id.uuid);
            Digest::update(&mut old, id.generation.to_le_bytes());
            Digest::update(&mut old, id.ctransid.to_le_bytes());
        }
        let dir = std::env::temp_dir().join(format!("layr-send-cache-key-{}", crate::ids::new_id()));
        std::fs::create_dir(&dir).unwrap();
        let old_name = hex::encode(old.finalize());
        std::fs::write(dir.join(&old_name), b"old entry").unwrap();
        let cache = Cache { dir: dir.clone(), name: cache_name(&ids) };
        assert_ne!(cache.name, old_name);
        assert!(cache.read().is_none());
        cache.write(b"new entry").unwrap();
        assert_eq!(cache.read().as_deref(), Some(&b"new entry"[..]));
        assert_eq!(std::fs::read(dir.join(old_name)).unwrap(), b"old entry");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checksum_roundtrip_rejects_corruption_and_old_format() {
        let dir = std::env::temp_dir().join(format!("layr-send-cache-{}", crate::ids::new_id()));
        std::fs::create_dir(&dir).unwrap();
        let cache = Cache { dir: dir.clone(), name: "entry".into() };
        cache.write(b"stream").unwrap();
        assert_eq!(cache.read().as_deref(), Some(&b"stream"[..]));
        let mut old = ContentAddress::digest(b"stream").to_vec();
        old.extend_from_slice(b"stream");
        std::fs::write(dir.join("entry"), old).unwrap();
        assert!(cache.read().is_none());
        cache.write(b"stream").unwrap();
        let mut bad = std::fs::read(dir.join("entry")).unwrap();
        *bad.last_mut().unwrap() ^= 1;
        std::fs::write(dir.join("entry"), bad).unwrap();
        assert!(cache.read().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
