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
    index(root).into_iter().chain(project_entries(root)).map(|e| e.path).collect()
}

/// The moved folders of every project file of the store (the authority, see `set_moved_path`).
fn project_entries(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(layout) = crate::volumes::Layout::for_root(root) else { return out };
    for (_, projects) in layout.ready_projects_dirs() {
        for dir in std::fs::read_dir(projects).into_iter().flatten().flatten() {
            let Ok(data) = std::fs::read(paths_file_in(&dir.path().join("meta"))) else { continue };
            let Ok(map) = serde_json::from_slice::<BTreeMap<String, PathBuf>>(&data) else { continue };
            let project = dir.file_name().to_string_lossy().into_owned();
            out.extend(map.into_iter().map(|(line, path)| Entry { path, project: project.clone(), line }));
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

    /// Record (or with `None` forget) the folder of a line. The project file is the source of the
    /// path; the store index is rewritten from it. If the index write fails, the project file goes
    /// back to its earlier bytes, so the caller can move the folder back and both keep the old
    /// path. The caller holds the project lock.
    pub fn set_moved_path(&self, line_id: &str, path: Option<&Path>) -> Result<()> {
        let earlier = match std::fs::read(self.paths_file()) {
            Ok(data) => Some(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let mut map = self.moved_paths();
        match path {
            Some(p) => map.insert(line_id.to_string(), p.to_path_buf()),
            None => map.remove(line_id),
        };
        crate::fsutil::atomic_write(&self.paths_file(), &serde_json::to_vec(&map)?, 0o600)?;
        if let Err(e) = self.write_index(&map) {
            let back = match &earlier {
                Some(data) => crate::fsutil::atomic_write(&self.paths_file(), data, 0o600),
                None => std::fs::remove_file(self.paths_file()).map_err(anyhow::Error::from),
            };
            return Err(match back {
                Ok(()) => e,
                Err(b) => e.context(format!("the project file could not go back: {b:#}")),
            });
        }
        register(&self.store.root);
        Ok(())
    }

    /// Replace the index entries of this project with `map`, under the lock shared by all projects.
    fn write_index(&self, map: &BTreeMap<String, PathBuf>) -> Result<()> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.store.root.join(".line-paths.lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
        let mut all: Vec<Entry> = index(&self.store.root).into_iter().filter(|e| e.project != self.name).collect();
        all.extend(map.iter().map(|(line, path)| Entry { path: path.clone(), project: self.name.clone(), line: line.clone() }));
        crate::fsutil::atomic_write(&index_file(&self.store.root), &serde_json::to_vec(&all)?, 0o644)
    }
}

/// The index entry that holds `path`, with the path below its root. The index is a copy of the
/// project files: when it has no entry for `path` (a crash between the two writes), the project
/// files decide.
pub fn find(store: &Store, path: &Path) -> Option<(Entry, String)> {
    best_match(index(&store.root), path).or_else(|| best_match(project_entries(&store.root), path))
}

fn best_match(entries: Vec<Entry>, path: &Path) -> Option<(Entry, String)> {
    let mut best: Option<(Entry, String)> = None;
    for e in entries {
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
        // The project files are the authority: a store whose index lacks an entry still opens.
        let mut folders = moved_folders(root);
        // The volumes that hold projects are outside the store folder too.
        if let Ok(layout) = crate::volumes::Layout::for_root(root) {
            folders.extend(layout.ready_volume_paths());
        }
        folders.extend(extra.map(Path::to_path_buf));
        crate::storage::set_external(root, folders);
    }
}
