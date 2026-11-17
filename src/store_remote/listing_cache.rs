//! Complete gateway listings, revalidated on every use. Cache failures only cost another full listing.

use layr_hashers::{ContentAddress, Digest};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Entry {
    pub key: String,
    pub modified: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// Keep byte-identical to layr-cloud's listing validator; see the shared golden vector.
pub(super) fn validator(prefix: &str, folders: bool, entries: &[Entry]) -> Option<String> {
    let mode = if folders { "folders" } else { "objects" };
    let mut hash = ContentAddress::new();
    hash.update(format!("layr-listing-v1\n{mode}\n{prefix}\n").as_bytes());
    for entry in entries {
        let etag = entry.etag.as_deref().unwrap_or("");
        if entry.key.contains(['\t', '\r', '\n']) || etag.contains(['\t', '\r', '\n']) {
            return None;
        }
        let size = entry.size.map(|size| size.to_string()).unwrap_or_default();
        hash.update(format!("{}\t{}\t{size}\t{etag}\n", entry.key, entry.modified).as_bytes());
    }
    Some(format!("\"{}\"", hex::encode(hash.finalize())))
}

/// Folder entries have their returned prefix as key, modified=0 and unknown size/ETag.
pub(super) fn folder_validator(prefix: &str, folders: &[String]) -> Option<String> {
    let entries: Vec<_> = folders.iter().map(|key| Entry { key: key.clone(), ..Entry::default() }).collect();
    validator(prefix, true, &entries)
}

#[derive(Deserialize, Serialize)]
pub(super) struct Cached<T> {
    pub etag: String,
    pub listing: T,
}

pub(super) fn path(dir: &Path, request: &str) -> PathBuf {
    dir.join(format!("listing-{}.json", hex::encode(ContentAddress::digest(request.as_bytes()))))
}

fn valid_etag(etag: &str) -> bool {
    etag.len() == 66
        && etag.starts_with('"')
        && etag.ends_with('"')
        && etag[1..etag.len() - 1].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn etag(response: &reqwest::blocking::Response) -> Option<String> {
    response.headers().get(reqwest::header::ETAG)?.to_str().ok().filter(|tag| valid_etag(tag)).map(str::to_owned)
}

pub(super) fn load<T: DeserializeOwned>(path: &Path) -> Option<Cached<T>> {
    let cached: Cached<T> = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    valid_etag(&cached.etag).then_some(cached)
}

pub(super) fn store<T: Serialize>(path: &Path, etag: Option<String>, listing: &T) {
    let Some(etag) = etag else {
        // An old gateway must leave no validator behind from a previously upgraded one.
        let _ = std::fs::remove_file(path);
        return;
    };
    let write = || -> anyhow::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().unwrap())?;
        crate::fsutil::cache_write(path, &serde_json::to_vec(&Cached { etag, listing })?, 0o600)
    };
    let _ = write();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_listing_shared_vector() {
        let entries = vec![
            Entry { key: "records/a.age".into(), modified: 12345, size: Some(3), etag: Some("\"abc\"".into()) },
            Entry { key: "records/z.age".into(), modified: -1, ..Entry::default() },
        ];
        // Shared with layr's listing_cache test; includes quoted ETag and absent metadata.
        let canonical = b"layr-listing-v1\nobjects\nrecords/\nrecords/a.age\t12345\t3\t\"abc\"\nrecords/z.age\t-1\t\t\n";
        let expected = "\"6dc9aacf6a4f6198b17a768edaa4f4d90fc2e13e0bbe0c30866d95eef3b62922\"";
        assert_eq!(format!("\"{}\"", hex::encode(ContentAddress::digest(canonical))), expected);
        assert_eq!(validator("records/", false, &entries).unwrap(), expected);
    }
}
