//! Bounded ciphertext replay for one verification operation and one direct retry.
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

const WINDOW: usize = 512 << 20;
pub fn cacheable(key: &str) -> bool {
    let (epoch, key) = if let Some(rest) = key.strip_prefix("project/epochs/") {
        let Some((epoch, inner)) = rest.split_once('/') else {
            return false;
        };
        if !super::format::is_uuid(epoch) {
            return false;
        }
        (true, inner)
    } else {
        (false, key)
    };
    super::format::parse_tree_name(key).is_some()
        || super::format::parse_data_name(key).is_some()
        || super::format::parse_segment_name(key).is_some()
        || super::format::release_file(key)
        || matches!(key.split('/').collect::<Vec<_>>().as_slice(), ["index", "paths" | "graph", name]
            if name.strip_suffix(".age").is_some_and(super::format::is_uuid))
        || (epoch
            && key
                .strip_prefix("history/")
                .and_then(|s| s.strip_suffix(".age"))
                .is_some_and(|id| id == "root" || super::format::is_uuid(id)))
}
#[derive(Clone)]
pub struct Download {
    pub bytes: Option<Vec<u8>>,
    pub cdn: bool,
}
#[derive(Default)]
pub struct Replay {
    direct: AtomicBool,
    seen_cdn: AtomicBool,
    entries: Mutex<(BTreeMap<String, Download>, usize)>,
}
#[derive(Debug)]
pub struct DirectFailure;
impl std::fmt::Display for DirectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("verification failed after the direct read")
    }
}
impl std::error::Error for DirectFailure {}

impl Replay {
    pub fn cached(&self, key: &str) -> bool {
        self.entries.lock().expect("CDN replay lock").0.contains_key(key)
    }
    pub fn is_direct(&self) -> bool {
        self.direct.load(Ordering::SeqCst)
    }
    pub fn retry(&self, cached_cdn: bool) -> bool {
        (cached_cdn || self.seen_cdn.load(Ordering::SeqCst))
            && self.direct.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }
    pub fn source_is_cdn(&self, key: &str) -> bool {
        self.entries.lock().expect("CDN replay lock").0.get(key).is_some_and(|entry| entry.cdn)
    }
    pub fn get(&self, key: &str, request: impl FnOnce(bool) -> Result<Download>) -> Result<Option<Vec<u8>>> {
        let direct = self.is_direct();
        {
            let entries = self.entries.lock().expect("CDN replay lock");
            if let Some(entry) = entries.0.get(key) {
                if !direct || !entry.cdn {
                    return Ok(entry.bytes.clone());
                }
            }
        }
        let download = request(direct)?;
        if download.cdn {
            self.seen_cdn.store(true, Ordering::SeqCst);
        }
        let mut entries = self.entries.lock().expect("CDN replay lock");
        // During the retry, keep first-attempt direct replies and replace only CDN replies.
        // No new retry entries are retained, so a large direct verification stays streaming.
        if !direct || entries.0.contains_key(key) {
            let size = download.bytes.as_ref().map_or(0, Vec::len);
            if let Some(old) = entries.0.insert(key.into(), download.clone()) {
                entries.1 -= old.bytes.as_ref().map_or(0, Vec::len);
            }
            entries.1 += size;
            // One object may cross the window. Objects are independently limited to 32 MiB.
            // Keep that reply so it is never fetched directly twice during the retry.
            if !direct && entries.1 > WINDOW {
                bail!("CDN verification replay window exceeded");
            }
        }
        Ok(download.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_reads_each_cdn_object_directly_once_and_reuses_direct_replies() {
        let replay = Replay::default();
        let body = replay
            .get("cdn", |direct| {
                assert!(!direct);
                Ok(Download { bytes: Some(b"bad".to_vec()), cdn: true })
            })
            .unwrap();
        assert_eq!(body.unwrap(), b"bad");
        replay.get("direct", |_| Ok(Download { bytes: Some(b"trusted".to_vec()), cdn: false })).unwrap();
        assert!(replay.retry(false));
        assert_eq!(
            replay
                .get("cdn", |direct| {
                    assert!(direct);
                    Ok(Download { bytes: Some(b"good".to_vec()), cdn: false })
                })
                .unwrap()
                .unwrap(),
            b"good"
        );
        assert_eq!(replay.get("direct", |_| panic!("direct reply must not be fetched twice")).unwrap().unwrap(), b"trusted");
        replay.get("cdn", |_| panic!("retry reply must not be fetched twice")).unwrap();
        assert!(!replay.retry(false));
    }
    #[test]
    fn a_direct_failure_never_retries_and_a_cached_cdn_part_can_retry_once() {
        let replay = Replay::default();
        replay.get("missing", |_| Ok(Download { bytes: None, cdn: false })).unwrap();
        assert!(!replay.retry(false));
        assert!(replay.retry(true));
        assert!(!replay.retry(true));
    }
}

#[derive(Default)]
pub struct Signing {
    active: Mutex<usize>,
    changed: std::sync::Condvar,
}
pub struct SigningLease<'a>(&'a Signing);
impl Signing {
    pub fn acquire(&self) -> SigningLease<'_> {
        let mut active = self.active.lock().expect("signing admission lock");
        while *active >= 4 {
            active = self.changed.wait(active).expect("signing admission lock");
        }
        *active += 1;
        SigningLease(self)
    }
}
impl Drop for SigningLease<'_> {
    fn drop(&mut self) {
        *self.0.active.lock().expect("signing admission lock") -= 1;
        self.0.changed.notify_one();
    }
}
#[derive(Debug)]
pub struct BusyFailure;
impl std::fmt::Display for BusyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("gateway is busy after bounded retries")
    }
}
impl std::error::Error for BusyFailure {}

#[derive(Debug)]
pub struct BatchDeadlineFailure;
impl std::fmt::Display for BatchDeadlineFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("gateway signing deadline expired for one key after reducing the batch")
    }
}
impl std::error::Error for BatchDeadlineFailure {}
