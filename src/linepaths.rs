//! Working folders of lines at a chosen path (`layr line move`), or kept after a rename.
//!
//! The path of a moved line is machine local and never replicates. Two files hold it:
//! `meta/line-paths.json` of the project (`{line id: path}`, the owner) and the index
//! `<store>/line-paths.json` (`[{path, project, line}]`), which lets a command find its project
//! and line from a folder outside the store without opening every project.
//!
//! A relative path in the project file is a folder below the project's `lines/`: `line rename`
//! keeps the folder of the old name, so programs that work in it keep their folder. It moves with
//! the project, so only the project file holds it, never the index.
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
pub(crate) fn paths_file_in(meta: &Path) -> PathBuf {
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
            out.extend(map.into_iter().filter(|(_, path)| path.is_absolute()).map(|(line, path)| Entry {
                path,
                project: project.clone(),
                line,
            }));
        }
    }
    out
}

impl Project<'_> {
    fn paths_file(&self) -> PathBuf {
        paths_file_in(&self.meta_dir())
    }

    /// The project file as written: line id to an absolute folder (moved out of the store) or to a
    /// folder name below `lines/` (kept after a rename).
    fn recorded_paths(&self) -> BTreeMap<String, PathBuf> {
        std::fs::read(self.paths_file()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    /// The folders of this project outside the store: line id to folder.
    pub(crate) fn moved_paths(&self) -> BTreeMap<String, PathBuf> {
        self.recorded_paths().into_iter().filter(|(_, p)| p.is_absolute()).collect()
    }

    /// The recorded folder of a line: a path outside the store (`line move`), or a folder in
    /// `lines/` that is not `lines/<name>` (kept by `line rename`). None: `lines/<name>`.
    pub fn moved_path(&self, line_id: &str) -> Option<PathBuf> {
        self.recorded_paths().remove(line_id).map(|p| self.resolve_folder(p))
    }

    fn resolve_folder(&self, p: PathBuf) -> PathBuf {
        if p.is_absolute() {
            return p;
        }
        // A line name only, so an edited file cannot point out of `lines/`.
        self.default_line_path(&p.to_string_lossy())
    }

    /// The working folder of line `line_id` named `name`: its recorded folder, else `lines/<name>`. When another
    /// line keeps `lines/<name>` (a folder that `line rename` kept), the line has no folder there: the path is a
    /// folder of its id below `meta/`, which no command makes, so that no command reads, writes or deletes the
    /// folder of the other line as this line's.
    pub fn line_folder(&self, line_id: &str, name: &str) -> PathBuf {
        let map = self.recorded_paths();
        if let Some(p) = map.get(line_id) {
            return self.resolve_folder(p.clone());
        }
        if map.iter().any(|(id, p)| id != line_id && p.is_relative() && p == Path::new(name)) {
            return self.meta_dir().join("no-folder").join(line_id);
        }
        self.default_line_path(name)
    }

    /// The line id that keeps the folder `lines/<folder>` after a rename.
    pub fn kept_folder_line(&self, folder: &str) -> Option<String> {
        kept_folder_line_in(&self.meta_dir(), folder)
    }

    /// After `line rename` (or its undo) to `new`: the folder stays where it is. A folder in
    /// `lines/` is recorded by its name, and forgotten when it is `lines/<new>`; a folder outside
    /// the store keeps its path. Returns the entry before, for `restore_folder`. The caller holds
    /// the project lock.
    pub fn keep_folder(&self, line_id: &str, old: &str, new: &str) -> Result<Option<PathBuf>> {
        let before = self.recorded_paths().remove(line_id);
        let folder = match &before {
            Some(p) if p.is_absolute() => return Ok(before),
            Some(p) => p.clone(),
            // A line without a folder on this machine has nothing to keep.
            None if self.default_line_path(old).symlink_metadata().is_err() => return Ok(before),
            None => PathBuf::from(old),
        };
        self.set_moved_path(line_id, (folder != Path::new(new)).then_some(folder.as_path()))?;
        Ok(before)
    }

    /// Put back the entry that `keep_folder` returned (a rename whose record failed).
    pub fn restore_folder(&self, line_id: &str, before: Option<&Path>) -> Result<()> {
        if self.recorded_paths().get(line_id).map(PathBuf::as_path) == before {
            return Ok(());
        }
        self.set_moved_path(line_id, before)
    }

    /// Record (or with `None` forget) the folder of a line: an absolute path outside the store, or a
    /// folder name below `lines/` (see `keep_folder`). The project file is the source of the
    /// path; the store index is rewritten from it. If the index write fails, the project file goes
    /// back to its earlier bytes, so the caller can move the folder back and both keep the old
    /// path. The caller holds the project lock.
    pub fn set_moved_path(&self, line_id: &str, path: Option<&Path>) -> Result<()> {
        let earlier = match std::fs::read(self.paths_file()) {
            Ok(data) => Some(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let mut map = self.recorded_paths();
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
        all.extend(map.iter().filter(|(_, path)| path.is_absolute()).map(|(line, path)| Entry {
            path: path.clone(),
            project: self.name.clone(),
            line: line.clone(),
        }));
        crate::fsutil::atomic_write(&index_file(&self.store.root), &serde_json::to_vec(&all)?, 0o644)
    }
}

/// The line id that keeps the folder `lines/<folder>` of the project with the meta folder `meta`
/// (a folder kept by `line rename`), from the project file alone.
pub fn kept_folder_line_in(meta: &Path, folder: &str) -> Option<String> {
    let data = std::fs::read(paths_file_in(meta)).ok()?;
    let map: BTreeMap<String, PathBuf> = serde_json::from_slice(&data).ok()?;
    map.into_iter().find(|(_, p)| p.is_relative() && p == Path::new(folder)).map(|(id, _)| id)
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
        // A view move can stop after the kernel moved its mount but before the
        // path index changed. Accept both journal paths during registry recovery.
        folders.extend(crate::views::pending_paths(root));
        // The volumes that hold projects are outside the store folder too. All of them, usable
        // or not: an unusable volume (unmounted, wrong identity) must not break the registry.
        if let Ok(layout) = crate::volumes::Layout::for_root(root) {
            folders.extend(layout.volume_paths());
        }
        folders.extend(extra.map(Path::to_path_buf));
        crate::storage::set_external(root, folders);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_folder_is_found_by_its_relative_entry() {
        let root = std::env::temp_dir().join(format!("layr-linepaths-{}", crate::ids::new_id()));
        let meta = root.join("projects").join("p").join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        let map: BTreeMap<String, PathBuf> =
            [("a".to_string(), PathBuf::from("feat")), ("b".to_string(), PathBuf::from("/elsewhere/work"))].into();
        std::fs::write(paths_file_in(&meta), serde_json::to_vec(&map).unwrap()).unwrap();
        assert_eq!(kept_folder_line_in(&meta, "feat").as_deref(), Some("a"));
        assert_eq!(kept_folder_line_in(&meta, "work"), None, "an absolute entry is no kept folder");
        assert_eq!(kept_folder_line_in(&meta, "other"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
