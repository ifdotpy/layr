//! Direct reads of a portable state. Native paths are made only for callers that
//! require a file descriptor; the private cache is not a saved state.
use crate::fsutil::{FType, Meta};
use anyhow::{bail, Context, Result};
use layr_objects::{Id, Node, StateRef, Store};
use std::collections::BTreeMap;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct ObjectTree {
    pub store: Store,
    pub refs: StateRef,
    cache: PathBuf,
    materialized: Mutex<BTreeMap<String, PathBuf>>,
    links: BTreeMap<String, (String, u64, u64)>,
}
impl std::fmt::Debug for ObjectTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectTree").field("cache", &self.cache).finish()
    }
}
#[derive(Clone)]
pub struct Entry {
    pub entry: layr_objects::tree::Entry,
    pub ns: i64,
    folder_times: Option<Id>,
}
impl ObjectTree {
    pub fn new(store: Store, refs: StateRef, parent: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(parent)?;
        let cache = parent.join(crate::store::temp_name());
        let mut links = BTreeMap::new();
        for (index, group) in store.get_tree(&refs.tree)?.links.iter().enumerate() {
            let first = std::str::from_utf8(&group[0])?.to_string();
            for path in group {
                let path = std::str::from_utf8(path)?.to_string();
                anyhow::ensure!(
                    links.insert(path, (first.clone(), index as u64 + 1, group.len() as u64)).is_none(),
                    "duplicate hard link path"
                );
            }
        }
        std::fs::DirBuilder::new().mode(0o700).create(&cache)?;
        Ok(Arc::new(Self { store, refs, cache, links, materialized: Mutex::new(BTreeMap::new()) }))
    }
    pub fn root(&self) -> &Path {
        &self.cache
    }
    pub fn entry(&self, rel: &str) -> Result<Option<Entry>> {
        crate::fsutil::check_rel(rel)?;
        let mut tree = self.refs.tree;
        let mut times = self.refs.times;
        if rel.is_empty() {
            return Ok(Some(Entry {
                entry: layr_objects::tree::Entry { name: Vec::new(), mode: 0o755, node: Node::Dir { tree }, xattrs: Vec::new() },
                ns: self.store.get_times(&times)?.own,
                folder_times: Some(times),
            }));
        }
        let parts: Vec<_> = rel.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            let folder = self.store.get_tree(&tree)?;
            let tm = self.store.get_times(&times)?;
            anyhow::ensure!(folder.entries.len() == tm.records.len(), "tree and times count differ");
            let Some(index) = folder.entries.iter().position(|e| e.name == part.as_bytes()) else { return Ok(None) };
            let entry = folder.entries[index].clone();
            let ns = match (&entry.node, tm.records[index]) {
                (Node::Dir { tree: t }, layr_objects::times::Record::Folder(ts)) => {
                    tree = *t;
                    times = ts;
                    self.store.get_times(&ts)?.own
                }
                (Node::Dir { .. }, _) | (_, layr_objects::times::Record::Folder(_)) => bail!("tree and times kinds differ"),
                (_, layr_objects::times::Record::Time(ns)) => ns,
            };
            if i + 1 == parts.len() {
                let folder_times = matches!(entry.node, Node::Dir { .. }).then_some(times);
                return Ok(Some(Entry { entry, ns, folder_times }));
            }
            if !matches!(entry.node, Node::Dir { .. }) {
                return Ok(None);
            }
        }
        Ok(None)
    }
    pub fn meta(&self, rel: &str) -> Result<Option<Meta>> {
        Ok(self.entry(rel)?.map(|e| e.meta()))
    }
    pub fn read(&self, rel: &str) -> Result<Vec<u8>> {
        match self.entry(rel)?.context("object path does not exist")?.entry.node {
            Node::File { blob, size } => self.file_bytes(&blob, size),
            Node::Symlink { target } => Ok(target),
            Node::Dir { .. } => bail!("object path is a directory"),
        }
    }
    fn file_bytes(&self, blob: &Id, size: u64) -> Result<Vec<u8>> {
        let bytes = self.store.read_blob(blob)?;
        anyhow::ensure!(bytes.len() as u64 == size, "object file size does not match its blob");
        Ok(bytes)
    }
    pub fn walk(&self, rel: &str, f: &mut dyn FnMut(&str, &Meta) -> Result<bool>) -> Result<()> {
        let Some(entry) = self.entry(rel)? else { return Ok(()) };
        let Node::Dir { tree } = entry.entry.node else { return Ok(()) };
        self.walk_tree(rel, tree, entry.folder_times.context("missing directory times")?, f)
    }
    fn walk_tree(&self, rel: &str, tree: Id, times: Id, f: &mut dyn FnMut(&str, &Meta) -> Result<bool>) -> Result<()> {
        let folder = self.store.get_tree(&tree)?;
        let times = self.store.get_times(&times)?;
        anyhow::ensure!(folder.entries.len() == times.records.len(), "tree and times count differ");
        for (entry, record) in folder.entries.into_iter().zip(times.records) {
            let name = std::str::from_utf8(&entry.name).context("a layr path is not UTF-8")?;
            let path = if rel.is_empty() { name.to_owned() } else { format!("{rel}/{name}") };
            let (ns, folder_times) = match (&entry.node, record) {
                (Node::Dir { .. }, layr_objects::times::Record::Folder(times)) => {
                    (self.store.get_times(&times)?.own, Some(times))
                }
                (Node::Dir { .. }, _) | (_, layr_objects::times::Record::Folder(_)) => bail!("tree and times kinds differ"),
                (_, layr_objects::times::Record::Time(ns)) => (ns, None),
            };
            let entry = Entry { entry, ns, folder_times };
            if f(&path, &entry.meta())? {
                if let Node::Dir { tree } = entry.entry.node {
                    self.walk_tree(&path, tree, folder_times.context("missing directory times")?, f)?;
                }
            }
        }
        Ok(())
    }
    pub fn entries(&self, rel: &str) -> Result<BTreeMap<String, Meta>> {
        let mut out = BTreeMap::new();
        self.walk(rel, &mut |p, m| {
            out.insert(p.to_owned(), m.clone());
            Ok(true)
        })?;
        Ok(out)
    }
    /// Decode one entry for a filesystem-only consumer. Never expand its children.
    pub fn path(&self, rel: &str) -> Result<PathBuf> {
        crate::fsutil::check_rel(rel)?;
        let mut paths = self.materialized.lock().map_err(|_| anyhow::anyhow!("object cache lock poisoned"))?;
        let path = self.path_locked(rel, &mut paths)?;
        if let Some((first, _, _)) = self.links.get(rel) {
            for (other, (group, _, _)) in &self.links {
                if group == first {
                    self.path_locked(other, &mut paths)?;
                }
            }
        }
        Ok(path)
    }
    fn path_locked(&self, rel: &str, paths: &mut BTreeMap<String, PathBuf>) -> Result<PathBuf> {
        let next = paths.len();
        let dst = paths
            .entry(rel.to_owned())
            .or_insert_with(|| self.cache.join(format!("entry-{next}")).join(if rel.is_empty() { "root" } else { rel }))
            .clone();
        let Some(e) = self.entry(rel)? else { return Ok(dst) };
        if dst.symlink_metadata().is_ok() {
            return Ok(dst);
        }
        let bytes = match &e.entry.node {
            Node::File { blob, size } => Some(self.file_bytes(blob, *size)?),
            _ => None,
        };
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some((first, _, _)) = self.links.get(rel) {
            anyhow::ensure!(matches!(e.entry.node, Node::File { .. }), "hard link path is not a file");
            if first != rel {
                let source = self.path_locked(first, paths)?;
                std::fs::hard_link(source, &dst)?;
                return Ok(dst);
            }
        }
        let result = (|| -> Result<()> {
            match &e.entry.node {
                Node::Dir { .. } => {
                    std::fs::create_dir(&dst)?;
                    layr_objects::fsx::set_xattrs(&dst, &e.entry.xattrs)?;
                    layr_objects::times::set(&dst, e.ns)?;
                    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(e.entry.mode))?;
                }
                Node::Symlink { target } => {
                    use std::os::unix::ffi::OsStrExt;
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), &dst)?;
                    layr_objects::fsx::set_xattrs(&dst, &e.entry.xattrs)?;
                    layr_objects::times::set(&dst, e.ns)?;
                }
                Node::File { .. } => {
                    use std::io::Write;
                    let bytes = bytes.as_ref().expect("file bytes");
                    let temp = self.cache.join(crate::store::temp_name());
                    let result = (|| -> Result<()> {
                        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
                        f.write_all(&bytes)?;
                        crate::fsutil::set_mtime(&f, e.ns.div_euclid(1_000_000_000), e.ns.rem_euclid(1_000_000_000))?;
                        layr_objects::fsx::set_xattrs(&temp, &e.entry.xattrs)?;
                        f.set_permissions(std::fs::Permissions::from_mode(e.entry.mode))?;
                        std::fs::rename(&temp, &dst)?;
                        Ok(())
                    })();
                    if result.is_err() {
                        let _ = std::fs::remove_file(&temp);
                    }
                    result?;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&dst);
            let _ = std::fs::remove_dir(&dst);
        }
        result?;
        Ok(dst)
    }
}
impl Entry {
    fn meta(&self) -> Meta {
        let (ftype, size) = match &self.entry.node {
            Node::File { size, .. } => (FType::File, *size),
            Node::Dir { .. } => (FType::Dir, 0),
            Node::Symlink { target } => (FType::Symlink, target.len() as u64),
        };
        Meta { ftype, size, mode: self.entry.mode, ino: 0, uid: 0, gid: 0, mtime: self.ns.div_euclid(1_000_000_000) }
    }
}
impl Drop for ObjectTree {
    fn drop(&mut self) {
        if let Ok(paths) = self.materialized.get_mut() {
            for path in paths.values() {
                if path.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.cache);
    }
}
use std::os::unix::fs::DirBuilderExt;

#[cfg(target_os = "macos")]
impl ObjectTree {
    pub fn scan_entry(&self, rel: &str) -> Result<Option<crate::storage::ScanEntry>> {
        Ok(self.entry(rel)?.map(|e| {
            let meta = e.meta();
            crate::storage::ScanEntry {
                fingerprint: (
                    u64::MAX,
                    self.links.get(rel).map(|(_, inode, _)| *inode).unwrap_or(0),
                    meta.mode,
                    meta.size,
                    meta.mtime,
                    e.ns.rem_euclid(1_000_000_000),
                    0,
                    0,
                ),
                meta,
                data: None,
                nlink: self.links.get(rel).map(|(_, _, count)| *count).unwrap_or(1),
                birth: (0, 0),
                flags: 0,
            }
        }))
    }
    pub fn scan(&self) -> Result<BTreeMap<String, crate::storage::ScanEntry>> {
        let mut map = BTreeMap::new();
        map.insert(String::new(), self.scan_entry("")?.context("missing root")?);
        self.walk("", &mut |p, _| {
            map.insert(p.to_owned(), self.scan_entry(p)?.context("missing object entry")?);
            Ok(true)
        })?;
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn fixture() -> Result<(PathBuf, Arc<ObjectTree>)> {
        let tmp = std::env::temp_dir().join(format!("layr-object-read-{}", crate::ids::new_id()));
        let source = tmp.join("source");
        std::fs::create_dir_all(source.join("nested"))?;
        std::fs::write(source.join("file"), b"content")?;
        std::fs::hard_link(source.join("file"), source.join("nested/link"))?;
        std::os::unix::fs::symlink("/outside", source.join("symlink"))?;
        std::fs::set_permissions(source.join("nested"), std::fs::Permissions::from_mode(0o750))?;
        layr_objects::times::set(&source.join("nested"), 1_700_000_000_123_456_789)?;
        let store = Store::open(&tmp.join("objects"), layr_objects::Keying::Plain)?;
        let written = layr_objects::write_tree(
            &store,
            &source,
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        let tree =
            ObjectTree::new(store, StateRef { tree: written.tree.unwrap(), times: written.times.unwrap() }, &tmp.join("reads"))?;
        std::fs::remove_dir_all(source)?;
        Ok((tmp, tree))
    }

    #[test]
    fn direct_read_and_pruned_walk_need_no_native_tree() -> Result<()> {
        let (tmp, tree) = fixture()?;
        assert_eq!(tree.read("file")?, b"content");
        assert_eq!(tree.read("symlink")?, b"/outside");
        assert!(tree.entry("symlink/child")?.is_none());
        assert!(tree.entry("../outside").is_err());
        let mut paths = Vec::new();
        tree.walk("", &mut |path, _| {
            paths.push(path.to_string());
            Ok(false)
        })?;
        assert_eq!(paths, ["file", "nested", "symlink"]);
        assert_eq!(std::fs::read_dir(tree.root())?.count(), 0);
        drop(tree);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }

    #[test]
    fn decoded_entries_keep_metadata_and_cross_directory_links() -> Result<()> {
        let (tmp, tree) = fixture()?;
        let file = tree.path("file")?;
        assert_eq!(std::fs::metadata(&file)?.nlink(), 2);
        assert_eq!(std::fs::metadata(&file)?.ino(), std::fs::metadata(tree.path("nested/link")?)?.ino());
        let dir = std::fs::metadata(tree.path("nested")?)?;
        assert_eq!(dir.mode() & 0o777, 0o750);
        assert_eq!(dir.mtime_nsec(), 123_456_789);
        assert_eq!(std::fs::read_link(tree.path("symlink")?)?, Path::new("/outside"));
        assert!(!tree.path("symlink/child")?.exists());
        drop(tree);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }

    #[test]
    fn writing_an_object_source_keeps_tree_times_and_link_groups() -> Result<()> {
        let (tmp, tree) = fixture()?;
        #[cfg(target_os = "macos")]
        let source = crate::thin::Layered::objects(tree.clone());
        #[cfg(not(target_os = "macos"))]
        let source = crate::source::Source::Objects(tree.clone());
        let copied = Store::open(&tmp.join("copy"), layr_objects::Keying::Plain)?;
        let report = layr_objects::write::write_tree_from(
            &copied,
            &crate::objstore::SnapshotSource(&source),
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        assert_eq!(report.tree, Some(tree.refs.tree));
        assert_eq!(report.times, Some(tree.refs.times));
        drop(source);
        drop(tree);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }

    #[test]
    fn corrupt_blob_fails_without_publishing_a_partial_entry() -> Result<()> {
        let (tmp, tree) = fixture()?;
        let Node::File { blob, .. } = tree.entry("file")?.unwrap().entry.node else { unreachable!() };
        std::fs::write(tree.store.hot_path(&blob).unwrap(), b"corrupt")?;
        assert!(tree.read("file").is_err());
        assert!(tree.path("file").is_err());
        assert_eq!(std::fs::read_dir(tree.root())?.count(), 0);
        #[cfg(target_os = "macos")]
        {
            let upper = tmp.join("upper");
            std::fs::create_dir(&upper)?;
            let source = crate::thin::Layered::over(&upper, crate::thin::Layered::objects(tree.clone()))?;
            let source = crate::objstore::SnapshotSource(&source);
            assert!(
                layr_objects::write::Source::names(&source, b"").is_err(),
                "a corrupt base entry must not disappear from a save"
            );
        }
        drop(tree);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }
}
