//! Working folders of lines at a chosen path (`layr line move`).
//!
//! The path of a moved line is machine local and never replicates. Two files hold it:
//! `meta/line-paths.json` of the project (`{line id: path}`, the owner) and the index
//! `<store>/line-paths.json` (`[{path, project, line}]`), which lets a command find its project
//! and line from a folder outside the store without opening every project.
use crate::store::{Project, Store};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub project: String,
    pub line: String,
}

fn index_file(root: &Path) -> PathBuf {
    root.join("line-paths.json")
}

/// The project file of moved paths, below the project's `meta` folder.
fn paths_file_in(meta: &Path) -> PathBuf {
    meta.join("line-paths.json")
}

/// The moved folders of the whole store (empty when none).
pub fn index(root: &Path) -> Vec<Entry> {
    std::fs::read(index_file(root)).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

/// Every moved folder the store records: the index and each project's own file.
#[cfg_attr(not(all(target_os = "linux", not(feature = "overlayfs"))), allow(dead_code))]
pub fn moved_folders(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = index(root).into_iter().map(|e| e.path).collect();
    for dir in std::fs::read_dir(root.join("projects")).into_iter().flatten().flatten() {
        if let Ok(data) = std::fs::read(paths_file_in(&dir.path().join("meta"))) {
            if let Ok(map) = serde_json::from_slice::<BTreeMap<String, PathBuf>>(&data) {
                out.extend(map.into_values());
            }
        }
    }
    out
}

impl Project<'_> {
    fn paths_file(&self) -> PathBuf {
        paths_file_in(&self.meta_dir())
    }

    fn moved_paths(&self) -> BTreeMap<String, PathBuf> {
        std::fs::read(self.paths_file()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    /// The recorded folder of a line, when it was moved out of the store.
    pub fn moved_path(&self, line_id: &str) -> Option<PathBuf> {
        self.moved_paths().remove(line_id)
    }

    /// Record (or with `None` forget) the folder of a line. The caller holds the project lock.
    pub fn set_moved_path(&self, line_id: &str, path: Option<&Path>) -> Result<()> {
        let mut map = self.moved_paths();
        match path {
            Some(p) => map.insert(line_id.to_string(), p.to_path_buf()),
            None => map.remove(line_id),
        };
        crate::fsutil::atomic_write(&self.paths_file(), &serde_json::to_vec(&map)?, 0o600)?;
        // The store index: one lock for all projects, entries of this project replaced.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.store.root.join(".line-paths.lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
        let mut all: Vec<Entry> = index(&self.store.root).into_iter().filter(|e| e.project != self.name).collect();
        all.extend(map.into_iter().map(|(line, path)| Entry { path, project: self.name.clone(), line }));
        crate::fsutil::atomic_write(&index_file(&self.store.root), &serde_json::to_vec(&all)?, 0o644)?;
        register(&self.store.root);
        Ok(())
    }
}

/// The index entry that holds `path`, with the path below its root.
pub fn find(store: &Store, path: &Path) -> Option<(Entry, String)> {
    let mut best: Option<(Entry, String)> = None;
    for e in index(&store.root) {
        if let Ok(rel) = path.strip_prefix(&e.path) {
            if best.as_ref().is_none_or(|(b, _)| e.path.components().count() > b.path.components().count()) {
                let sub = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                best = Some((e, sub));
            }
        }
    }
    best
}

/// OverlayFS manages a folder by its registry in the store; tell it the folders outside the store.
/// `extra` is a folder about to join (the target of a move).
pub fn register(root: &Path) {
    register_with(root, None);
}

#[cfg_attr(not(feature = "overlayfs"), allow(unused_variables))]
pub fn register_with(root: &Path, extra: Option<&Path>) {
    #[cfg(feature = "overlayfs")]
    {
        let mut folders: Vec<PathBuf> = index(root).into_iter().map(|e| e.path).collect();
        folders.extend(extra.map(Path::to_path_buf));
        crate::storage::set_external(root, folders);
    }
}
