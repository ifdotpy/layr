//! Validated status results. Drivers provide evidence; repository code owns comparison.
use crate::repo::Status;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Timestamp {
    pub seconds: i64,
    pub nanos: i64,
}

/// Native observation stamps are compared for equality; shared logic never interprets a
/// filesystem's counter layout. A backing stat binds a merged entry to its physical source.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum ObservationEvidence {
    Counter([u64; 3]),
    Backing(FileStat),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct FileStat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
}

impl FileStat {
    pub fn of(m: &std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            // A folder's link count is its entry count on APFS (lstat) but 1 in a bulk scan: only files compare it.
            nlink: if m.is_dir() { 1 } else { m.nlink() },
            mtime: Timestamp { seconds: m.mtime(), nanos: m.mtime_nsec() },
            ctime: Timestamp { seconds: m.ctime(), nanos: m.ctime_nsec() },
        }
    }
}

/// Missing optional acceleration is distinct from evidence that cannot verify this entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum FileFingerprint {
    Verified { stat: FileStat, evidence: Option<ObservationEvidence> },
    Unverifiable,
}

impl FileFingerprint {
    pub fn verifiable(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    pub fn trusted(&self, written: u64) -> bool {
        let Self::Verified { stat, .. } = self else { return false };
        // Use a full second, even on nanosecond filesystems: this also covers timestamps
        // rounded by copies and filesystems with coarse clocks. Future dates stay racy.
        // The timestamp is taken BEFORE scanning/reading, never after the comparison.
        let older = |t: &Timestamp| t.seconds < 0 || u64::try_from(t.seconds).is_ok_and(|s| s < written);
        older(&stat.mtime) && older(&stat.ctime)
    }
}

pub(crate) fn clock() -> Result<u64> {
    Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())
}

#[derive(Serialize, Deserialize)]
pub(crate) struct StatusCache {
    format: u32,
    version: String,
    key: String,
    written: u64,
    entries: BTreeMap<String, FileFingerprint>,
    pub status: Status,
}

impl StatusCache {
    pub fn new(key: String, written: u64, entries: BTreeMap<String, FileFingerprint>, status: Status) -> Self {
        Self { format: 5, version: env!("CARGO_PKG_VERSION").to_string(), key, written, entries, status }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn decode(raw: &[u8], key: &str) -> Option<Self> {
        let cache: Self = serde_json::from_slice(raw).ok()?;
        (cache.format == 5 && cache.version == env!("CARGO_PKG_VERSION") && cache.key == key).then_some(cache)
    }

    pub fn changed(&self, now: &BTreeMap<String, FileFingerprint>) -> BTreeSet<String> {
        self.entries
            .keys()
            .chain(now.keys())
            .filter(|p| {
                !p.is_empty()
                    && !crate::repo::skip_path(p)
                    && match (self.entries.get(*p), now.get(*p)) {
                        (Some(a), Some(b)) => a != b || !a.trusted(self.written),
                        _ => true,
                    }
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> FileFingerprint {
        FileFingerprint::Verified {
            stat: FileStat {
                dev: 1,
                ino: 2,
                size: 3,
                mode: 0o100644,
                uid: 1000,
                gid: 1000,
                nlink: 1,
                mtime: Timestamp { seconds: 100, nanos: 123 },
                ctime: Timestamp { seconds: 100, nanos: 456 },
            },
            evidence: None,
        }
    }

    #[test]
    fn racy_and_unverifiable_entries_are_rechecked() {
        let scan = BTreeMap::from([("file".to_string(), entry())]);
        let cache = StatusCache::new("base".into(), 100, scan.clone(), Status::default());
        assert_eq!(cache.changed(&scan), BTreeSet::from(["file".into()]));
        let cache = StatusCache::new("base".into(), 101, scan.clone(), Status::default());
        assert!(cache.changed(&scan).is_empty());
        let mapped = BTreeMap::from([("file".to_string(), FileFingerprint::Unverifiable)]);
        assert_eq!(cache.changed(&mapped), BTreeSet::from(["file".into()]));
        let cache = StatusCache::new("base".into(), 101, mapped.clone(), Status::default());
        assert_eq!(cache.changed(&mapped), BTreeSet::from(["file".into()]));
        assert_eq!(cache.changed(&scan), BTreeSet::from(["file".into()]));
    }

    #[test]
    fn version_base_and_corrupt_cache_invalidate() {
        let cache = StatusCache::new("base".into(), 101, BTreeMap::new(), Status::default());
        let raw = cache.encode().unwrap();
        assert!(StatusCache::decode(&raw, "base").is_some());
        assert!(StatusCache::decode(&raw, "another base or ignore policy").is_none());
        assert!(StatusCache::decode(&raw[..raw.len() / 2], "base").is_none());
        let mut cache = cache;
        cache.version.push_str("-old");
        assert!(StatusCache::decode(&cache.encode().unwrap(), "base").is_none());
    }
}
