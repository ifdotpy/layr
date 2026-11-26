//! Validated status results. Drivers provide evidence; repository code owns comparison.
use crate::repo::Status;
use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;

/// The persisted form of the cache. Version 6 stores each entry as a positional tuple (see `pairs`,
/// `FileStat` and `FileFingerprint`) to keep the file small and fast to decode.
const FORMAT: u32 = 6;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
pub(crate) struct Timestamp {
    pub seconds: i64,
    pub nanos: i64,
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (self.seconds, self.nanos).serialize(s)
    }
}

/// Native observation stamps are compared for equality; shared logic never interprets a
/// filesystem's counter layout. A backing stat binds a merged entry to its physical source.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum ObservationEvidence {
    #[serde(rename = "c")]
    Counter([u64; 3]),
    #[serde(rename = "b")]
    Backing(FileStat),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
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

/// A stat persists as the positional tuple of its fields in declaration order, a timestamp as `[seconds,
/// nanos]` (that order is part of cache format 6); the derived readers accept the arrays.
impl Serialize for FileStat {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (self.dev, self.ino, self.size, self.mode, self.uid, self.gid, self.nlink, &self.mtime, &self.ctime).serialize(s)
    }
}

/// Missing optional acceleration is distinct from evidence that cannot verify this entry.
/// Persisted as `null` (unverifiable) or `[stat, evidence]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FileFingerprint {
    Verified { stat: FileStat, evidence: Option<ObservationEvidence> },
    Unverifiable,
}

impl Serialize for FileFingerprint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Verified { stat, evidence } => s.serialize_some(&(stat, evidence)),
            Self::Unverifiable => s.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for FileFingerprint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = Option::<(FileStat, Option<ObservationEvidence>)>::deserialize(d)?;
        Ok(wire.map_or(Self::Unverifiable, |(stat, evidence)| Self::Verified { stat, evidence }))
    }
}

impl FileFingerprint {
    pub fn verifiable(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    pub fn trusted(&self, written: u64) -> bool {
        let Self::Verified { stat, .. } = self else { return false };
        // Use a full second, even on nanosecond filesystems: this also covers timestamps
        // rounded by copies and filesystems with coarse clocks. Future dates stay racy.
        // The timestamp is taken BEFORE scanning/reading, never after the comparison. A stamp in
        // second s can be s.999 and `written` is whole seconds: only s + 2 <= written leaves a
        // full second between them.
        let older = |t: &Timestamp| t.seconds < 0 || u64::try_from(t.seconds).is_ok_and(|s| s.saturating_add(1) < written);
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
    #[serde(with = "pairs")]
    entries: BTreeMap<String, FileFingerprint>,
    pub status: Status,
}

/// A sorted list of `[path, fingerprint]` pairs: no per-entry map syntax, and it decodes with one bulk build.
mod pairs {
    use super::*;

    pub fn serialize<S: Serializer>(entries: &BTreeMap<String, FileFingerprint>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(entries)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, FileFingerprint>, D::Error> {
        Ok(Vec::<(String, FileFingerprint)>::deserialize(d)?.into_iter().collect())
    }
}

impl StatusCache {
    pub fn new(key: String, written: u64, entries: BTreeMap<String, FileFingerprint>, status: Status) -> Self {
        Self { format: FORMAT, version: env!("CARGO_PKG_VERSION").to_string(), key, written, entries, status }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn decode(raw: &[u8], key: &str) -> Option<Self> {
        let cache: Self = serde_json::from_slice(raw).ok()?;
        (cache.format == FORMAT && cache.version == env!("CARGO_PKG_VERSION") && cache.key == key).then_some(cache)
    }

    /// Paths whose entry differs between this cache and `now`, or cannot be trusted: one pass over the two
    /// sorted maps. An entry on one side only has changed; equal entries have not unless they are racy.
    pub fn changed(&self, now: &BTreeMap<String, FileFingerprint>) -> BTreeSet<String> {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let (mut cached, mut current) = (self.entries.iter().peekable(), now.iter().peekable());
        let mut changed = Vec::new();
        loop {
            let path = match (cached.peek(), current.peek()) {
                (None, None) => break,
                (Some(&(p, _)), None) => {
                    cached.next();
                    p
                }
                (None, Some(&(p, _))) => {
                    current.next();
                    p
                }
                (Some(&(p, a)), Some(&(q, b))) => match p.cmp(q) {
                    Less => {
                        cached.next();
                        p
                    }
                    Greater => {
                        current.next();
                        q
                    }
                    Equal => {
                        cached.next();
                        current.next();
                        if a == b && a.trusted(self.written) {
                            continue;
                        }
                        p
                    }
                },
            };
            if !path.is_empty() && !crate::repo::skip_path(path) {
                changed.push(path.clone());
            }
        }
        changed.into_iter().collect()
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
        // 100.456 and a clock of 101 can be a millisecond apart.
        let cache = StatusCache::new("base".into(), 101, scan.clone(), Status::default());
        assert_eq!(cache.changed(&scan), BTreeSet::from(["file".into()]));
        let cache = StatusCache::new("base".into(), 102, scan.clone(), Status::default());
        assert!(cache.changed(&scan).is_empty());
        // A stamp before 1970 (a copied archive) is old, whatever the clock.
        let FileFingerprint::Verified { mut stat, evidence } = entry() else { unreachable!() };
        stat.mtime.seconds = -5;
        stat.ctime.seconds = -5;
        assert!(FileFingerprint::Verified { stat, evidence }.trusted(1));
        let mapped = BTreeMap::from([("file".to_string(), FileFingerprint::Unverifiable)]);
        assert_eq!(cache.changed(&mapped), BTreeSet::from(["file".into()]));
        let cache = StatusCache::new("base".into(), 102, mapped.clone(), Status::default());
        assert_eq!(cache.changed(&mapped), BTreeSet::from(["file".into()]));
        assert_eq!(cache.changed(&scan), BTreeSet::from(["file".into()]));
    }

    fn set(items: &[(&str, FileFingerprint)]) -> BTreeMap<String, FileFingerprint> {
        items.iter().map(|(p, f)| (p.to_string(), f.clone())).collect()
    }

    fn paths(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    /// The per-key predicate the merge join replaced.
    fn changed_by_lookup(cache: &StatusCache, now: &BTreeMap<String, FileFingerprint>) -> BTreeSet<String> {
        cache
            .entries
            .keys()
            .chain(now.keys())
            .filter(|p| {
                !p.is_empty()
                    && !crate::repo::skip_path(p)
                    && match (cache.entries.get(*p), now.get(*p)) {
                        (Some(a), Some(b)) => a != b || !a.trusted(cache.written),
                        _ => true,
                    }
            })
            .cloned()
            .collect()
    }

    #[test]
    fn one_sided_root_and_journal_entries() {
        let journal = format!(".layr-cache-replace-{}.json", "a".repeat(64));
        let journal_tmp = format!("{journal}.tmp");
        let near_miss = format!(".layr-cache-replace-{}.json", "a".repeat(63));
        let old = entry();
        let mut touched = entry();
        let FileFingerprint::Verified { stat, .. } = &mut touched else { unreachable!() };
        stat.size += 1;
        let cache = StatusCache::new(
            "base".into(),
            1000,
            set(&[
                ("", old.clone()),
                (".git/index", old.clone()),
                (&journal, old.clone()),
                (&journal_tmp, old.clone()),
                (&near_miss, old.clone()),
                ("deleted", old.clone()),
                ("kept", old.clone()),
                ("same", old.clone()),
                ("zeta-deleted", old.clone()),
            ]),
            Status::default(),
        );
        let now = set(&[
            ("", touched.clone()),
            (".git/head", old.clone()),
            (&journal, touched.clone()),
            ("added", old.clone()),
            ("kept", touched),
            ("same", old.clone()),
            ("zeta-added", old),
        ]);
        // Entries on one side only have changed, the root and git/journal names are never reported.
        let expected = paths(&["added", "deleted", "kept", "zeta-added", "zeta-deleted", &near_miss]);
        assert_eq!(cache.changed(&now), expected);
        assert_eq!(cache.changed(&now), changed_by_lookup(&cache, &now));
        assert!(cache.changed(&BTreeMap::new()).contains("deleted"));
        let empty = StatusCache::new("base".into(), 1000, BTreeMap::new(), Status::default());
        assert_eq!(empty.changed(&now), paths(&["added", "kept", "same", "zeta-added"]));
        assert!(empty.changed(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn join_equals_the_per_key_predicate_on_mixed_entries() {
        let at = |seconds: i64| {
            let FileFingerprint::Verified { mut stat, evidence } = entry() else { unreachable!() };
            (stat.mtime.seconds, stat.ctime.seconds) = (seconds, seconds);
            FileFingerprint::Verified { stat, evidence }
        };
        let counter = |c: u64| match entry() {
            FileFingerprint::Verified { stat, .. } => {
                FileFingerprint::Verified { stat, evidence: Some(ObservationEvidence::Counter([c, 0, 0])) }
            }
            other => other,
        };
        let cached = set(&[
            ("a-old", at(1)),
            ("b-racy", at(999)),
            ("c-future", at(5000)),
            ("e-counter", counter(1)),
            ("f-both-unverifiable", FileFingerprint::Unverifiable),
            ("g-became-verifiable", FileFingerprint::Unverifiable),
            ("h-became-unverifiable", at(1)),
            ("i-gone", at(1)),
        ]);
        let now = set(&[
            ("a-old", at(1)),
            ("b-racy", at(999)),
            ("c-future", at(5000)),
            ("e-counter", counter(2)),
            ("f-both-unverifiable", FileFingerprint::Unverifiable),
            ("g-became-verifiable", at(1)),
            ("h-became-unverifiable", FileFingerprint::Unverifiable),
            ("j-new", at(1)),
        ]);
        let cache = StatusCache::new("base".into(), 1000, cached, Status::default());
        // Equal entries are rechecked when racy, in the future or unverifiable; stale counters differ.
        assert_eq!(
            cache.changed(&now),
            paths(&[
                "b-racy",
                "c-future",
                "e-counter",
                "f-both-unverifiable",
                "g-became-verifiable",
                "h-became-unverifiable",
                "i-gone",
                "j-new",
            ])
        );
        assert_eq!(cache.changed(&now), changed_by_lookup(&cache, &now));
        assert_eq!(cache.changed(&cache.entries), changed_by_lookup(&cache, &cache.entries));
    }

    #[test]
    fn encoding_round_trips_every_field() {
        let stat = |mtime: i64, ctime: i64| FileStat {
            dev: u64::MAX,
            ino: u64::MAX - 1,
            size: u64::MAX - 2,
            mode: u32::MAX,
            uid: u32::MAX - 1,
            gid: 0,
            nlink: u64::MAX - 3,
            mtime: Timestamp { seconds: mtime, nanos: i64::MIN },
            ctime: Timestamp { seconds: ctime, nanos: i64::MAX },
        };
        let entries = set(&[
            ("", entry()),
            ("é \"quoted\"/\n", FileFingerprint::Unverifiable),
            (
                "backing",
                FileFingerprint::Verified {
                    stat: stat(i64::MIN, -1),
                    evidence: Some(ObservationEvidence::Backing(stat(0, i64::MAX))),
                },
            ),
            (
                "counter",
                FileFingerprint::Verified { stat: stat(1, 2), evidence: Some(ObservationEvidence::Counter([u64::MAX, 0, 7])) },
            ),
            ("none", FileFingerprint::Verified { stat: stat(3, 4), evidence: None }),
        ]);
        let mut status = Status::default();
        status.untracked = vec!["new".into()];
        status.untracked_dirs = vec!["dir/".into()];
        status.ignored = vec!["target".into()];
        let cache = StatusCache::new("base".into(), u64::MAX, entries.clone(), status);
        let back = StatusCache::decode(&cache.encode().unwrap(), "base").unwrap();
        assert_eq!(back.entries, entries);
        assert_eq!(back.written, u64::MAX);
        assert_eq!(serde_json::to_value(&back.status).unwrap(), serde_json::to_value(&cache.status).unwrap());
        // Entries are a sorted list of positional tuples, and an unsorted list still loads sorted.
        let json: serde_json::Value = serde_json::from_slice(&cache.encode().unwrap()).unwrap();
        assert_eq!(json["format"], 6);
        assert_eq!(json["entries"][0][0], "");
        assert_eq!(
            json["entries"][0],
            serde_json::json!(["", [[1, 2, 3, 0o100644, 1000, 1000, 1, [100, 123], [100, 456]], null]])
        );
        assert_eq!(json["entries"][4], serde_json::json!(["é \"quoted\"/\n", null]));
        let mut raw = json.clone();
        raw["entries"].as_array_mut().unwrap().reverse();
        let shuffled = StatusCache::decode(&serde_json::to_vec(&raw).unwrap(), "base").unwrap();
        assert_eq!(shuffled.entries, entries);
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
        // A cache of another format (the previous named-field maps included) is rebuilt, not read.
        let old = br#"{"format":5,"version":"x","key":"base","written":1,"entries":{},"status":{}}"#;
        assert!(StatusCache::decode(old, "base").is_none());
        let mut json: serde_json::Value = serde_json::from_slice(&cache.encode().unwrap()).unwrap();
        json["version"] = env!("CARGO_PKG_VERSION").into();
        assert!(StatusCache::decode(&serde_json::to_vec(&json).unwrap(), "base").is_some());
        json["format"] = 5.into();
        assert!(StatusCache::decode(&serde_json::to_vec(&json).unwrap(), "base").is_none());
    }
}
