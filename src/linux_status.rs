//! Metadata enumeration shared by Linux drivers. Validity evidence comes from the driver.
use crate::status_cache::{FileFingerprint, FileStat};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub(crate) fn native_fingerprint(path: &Path, observed: &std::fs::Metadata) -> FileFingerprint {
    let stat = FileStat::of(observed);
    if observed.is_file() {
        match crate::sys::leased_metadata(path) {
            Ok(Some(leased)) if FileStat::of(&leased) == stat => {}
            _ => return FileFingerprint::Unverifiable,
        }
    }
    FileFingerprint::Verified { stat, evidence: None }
}

pub(crate) fn scan(
    root: &Path,
    paths: Option<&BTreeSet<String>>,
    evidence: &mut dyn FnMut(&str, &std::fs::Metadata) -> FileFingerprint,
) -> Result<BTreeMap<String, FileFingerprint>> {
    let root_meta = std::fs::symlink_metadata(root)?;
    let mut out = BTreeMap::new();
    let mut pending: Vec<String> = paths.map_or_else(|| vec![String::new()], |p| p.iter().cloned().collect());
    while let Some(rel) = pending.pop() {
        let path = root.join(&rel);
        let m = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if paths.is_some() && matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
                continue
            }
            Err(e) => return Err(e.into()),
        };
        if !rel.is_empty()
            && (crate::repo::skip_path(&rel)
                || m.dev() != root_meta.dev()
                || m.is_dir() && crate::fs::host().namespace.managed(&path))
        {
            continue;
        }
        out.insert(rel.clone(), evidence(&rel, &m));
        if paths.is_none() && m.is_dir() {
            for child in std::fs::read_dir(path)? {
                let name = child?.file_name().into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 file name"))?;
                pending.push(if rel.is_empty() { name } else { format!("{rel}/{name}") });
            }
        }
    }
    Ok(out)
}

pub(crate) fn status_stage_identity(root: &Path) -> Result<Vec<u8>> {
    let m = std::fs::symlink_metadata(root)?;
    Ok(format!("{}:{}", m.dev(), m.ino()).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn leases_reject_writable_unreadable_and_replaced_observations() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-status-lease-{}", crate::ids::new_id()));
        std::fs::create_dir(&dir)?;
        let path = dir.join("file");
        std::fs::write(&path, b"original")?;
        let observed = std::fs::symlink_metadata(&path)?;
        assert!(native_fingerprint(&path, &observed).verifiable());
        let writer = std::fs::OpenOptions::new().write(true).open(&path)?;
        assert!(!native_fingerprint(&path, &observed).verifiable());
        drop(writer);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))?;
        assert!(!native_fingerprint(&path, &std::fs::symlink_metadata(&path)?).verifiable());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        std::fs::rename(&path, dir.join("old"))?;
        std::fs::write(&path, b"replaced")?;
        assert!(!native_fingerprint(&path, &observed).verifiable());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
