//! Adapt logical object metadata to the native sparse-copy scan without exporting paths.
use crate::{apfs::ScanEntry, object_tree::ObjectTree};
use anyhow::{Context, Result};
use layr_fs_core::{RelDir, TreeRead};
use std::collections::BTreeMap;
pub(crate) fn entry(objects: &ObjectTree, path: &str) -> Result<Option<ScanEntry>> {
    let Some(metadata) = objects.metadata(&RelDir::from_bytes(path.as_bytes())?)? else { return Ok(None) };
    let meta = layr_fs_common::observation(&metadata);
    Ok(Some(ScanEntry {
        fingerprint: (
            u64::MAX,
            metadata.link.map_or(0, |link| link.inode),
            meta.mode,
            meta.size,
            meta.mtime,
            metadata.attributes.modified_ns.rem_euclid(1_000_000_000),
            0,
            0,
        ),
        meta,
        data: None,
        nlink: metadata.link.map_or(1, |link| link.count),
        birth: (0, 0),
        flags: 0,
    }))
}
pub(crate) fn scan(objects: &ObjectTree) -> Result<BTreeMap<String, ScanEntry>> {
    let mut entries = BTreeMap::from([(String::new(), entry(objects, "")?.context("object root absent")?)]);
    objects.walk("", &mut |path, _| {
        entries.insert(path.to_owned(), entry(objects, path)?.context("object entry disappeared")?);
        Ok(true)
    })?;
    Ok(entries)
}
