//! Volumes: the file systems that hold projects (docs/volumes.md).
//!
//! Machine configuration, never replicated:
//! - `<root>/volumes.json`: `{format, default, volumes: {name: {path}}}`. The store root is the
//!   volume `main` and has no entry.
//! - `<root>/project-volumes.json`: `{format, projects: {project: volume}}`. A project without an
//!   entry lives on `main`.
//! - `<volume path>/layr-volume.json`: the identity of a volume (`store` = machine id, `volume`).
//!   A volume without its identity file is not mounted (or not ours) and is never used.
//!
//! `Layout` is the one resolver for the folder of a project and of a volume. Every other
//! code path asks it; nothing builds `<root>/projects/<name>` by itself.

use crate::fsutil;
use crate::store::check_name;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The store root as a volume.
pub const MAIN: &str = "main";
/// Format of the three volume files. Readers refuse other formats.
pub const FORMAT: u32 = 1;
/// The identity file at the top of every volume but `main`.
pub const IDENTITY: &str = "layr-volume.json";
const VOLUMES_FILE: &str = "volumes.json";
const PROJECTS_FILE: &str = "project-volumes.json";
const LOCK_FILE: &str = ".volumes.lock";

#[derive(Serialize, Deserialize)]
struct VolumesFile {
    format: u32,
    #[serde(default = "main_name")]
    default: String,
    #[serde(default)]
    volumes: BTreeMap<String, VolumeEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct VolumeEntry {
    path: PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct ProjectsFile {
    format: u32,
    #[serde(default)]
    projects: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct Identity {
    format: u32,
    store: String,
    volume: String,
}

fn main_name() -> String {
    MAIN.into()
}

/// The volume configuration of a store, read from its files.
pub struct Layout {
    root: PathBuf,
    /// The machine id of the store: the identity files must name it.
    machine: String,
    default: String,
    /// The volumes other than `main`: name to path.
    volumes: BTreeMap<String, PathBuf>,
    /// The projects that do not live on `main`: project to volume.
    projects: BTreeMap<String, String>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data).with_context(|| format!("invalid {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn check_format(path: &Path, format: u32) -> Result<()> {
    if format != FORMAT {
        bail!("incompatible {}: format {format}; this binary uses format {FORMAT}", path.display());
    }
    Ok(())
}

impl Layout {
    /// Read the volume files of the store at `root`. Missing files mean a store without volumes.
    /// `machine` is the machine id that the identity files must name.
    pub fn load(root: &Path, machine: &str) -> Result<Layout> {
        let volumes = match read_json::<VolumesFile>(&root.join(VOLUMES_FILE))? {
            Some(f) => {
                check_format(&root.join(VOLUMES_FILE), f.format)?;
                f
            }
            None => VolumesFile { format: FORMAT, default: MAIN.into(), volumes: BTreeMap::new() },
        };
        let projects = match read_json::<ProjectsFile>(&root.join(PROJECTS_FILE))? {
            Some(f) => {
                check_format(&root.join(PROJECTS_FILE), f.format)?;
                f.projects
            }
            None => BTreeMap::new(),
        };
        Ok(Layout {
            root: root.to_path_buf(),
            machine: machine.to_string(),
            default: volumes.default,
            volumes: volumes.volumes.into_iter().map(|(name, entry)| (name, entry.path)).collect(),
            projects,
        })
    }

    /// The layout of the store at `root`, with the machine id read from its machine identity.
    /// A store without that identity has no volume but `main` (its identity files cannot match).
    pub fn for_root(root: &Path) -> Result<Layout> {
        let machine = fs::read_to_string(root.join("machine").join("id")).map(|s| s.trim().to_string()).unwrap_or_default();
        Self::load(root, &machine)
    }

    /// The volume that new projects go to when no `--volume` is given.
    pub fn default_volume(&self) -> &str {
        &self.default
    }

    /// `main` first, then the other volumes by name.
    pub fn names(&self) -> Vec<String> {
        std::iter::once(MAIN.to_string()).chain(self.volumes.keys().cloned()).collect()
    }

    pub fn is_volume(&self, name: &str) -> bool {
        name == MAIN || self.volumes.contains_key(name)
    }

    /// The configured path of a volume, without any check (`main`: the store root).
    pub fn path_of(&self, name: &str) -> Option<PathBuf> {
        if name == MAIN {
            Some(self.root.clone())
        } else {
            self.volumes.get(name).cloned()
        }
    }

    /// The volume of a project. An overlay (`<project>%<overlay>`) lives with its project.
    pub fn volume_of(&self, project: &str) -> String {
        let base = project.split('%').next().unwrap_or(project);
        self.projects.get(base).cloned().unwrap_or_else(|| MAIN.to_string())
    }

    /// The path of a volume when it can be used: its folder exists and its identity file names
    /// this store and this volume. `main` is always usable.
    pub fn ready(&self, name: &str) -> Result<PathBuf> {
        if name == MAIN {
            return Ok(self.root.clone());
        }
        let Some(path) = self.volumes.get(name) else {
            bail!("no volume '{name}'; see layr volume list");
        };
        if !path.is_dir() {
            bail!("volume '{name}' at {} is missing: the folder does not exist (is the disk mounted?)", path.display());
        }
        let data = match fs::read(path.join(IDENTITY)) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("volume '{name}' at {} is missing: no {IDENTITY} (is the disk mounted?)", path.display())
            }
            Err(e) => return Err(e).with_context(|| format!("read the identity of volume '{name}' at {}", path.display())),
        };
        let id: Identity =
            serde_json::from_slice(&data).map_err(|_| anyhow!("volume '{name}' at {}: invalid {IDENTITY}", path.display()))?;
        if id.format != FORMAT || id.store != self.machine || id.volume != name {
            bail!("volume '{name}' at {} belongs to another store or volume; it is refused", path.display());
        }
        Ok(path.clone())
    }

    /// The folder of a project (or of an overlay `<project>%<overlay>`). Refused when the volume
    /// of the project cannot be used.
    pub fn project_dir(&self, name: &str) -> Result<PathBuf> {
        let (base, overlay) = match name.split_once('%') {
            Some((p, o)) => {
                check_name("project", p)?;
                check_name("overlay", o)?;
                (p, Some(o))
            }
            None => {
                check_name("project", name)?;
                (name, None)
            }
        };
        let volume = self.volume_of(base);
        let root = self.ready(&volume).map_err(|e| e.context(format!("project '{base}' is on volume '{volume}'")))?;
        let project = root.join("projects").join(base);
        Ok(match overlay {
            Some(o) => project.join("meta").join("overlays").join(o),
            None => project,
        })
    }

    /// The trash folder of a volume: where a delete on that volume renames trees to. The volume
    /// must be ready (the caller opened a project on it).
    pub fn trash_of(&self, volume: &str) -> Result<PathBuf> {
        Ok(self.ready(volume)?.join("trash"))
    }

    /// The `projects` folder of a volume that can be used.
    pub fn projects_folder(&self, volume: &str) -> Result<PathBuf> {
        Ok(self.ready(volume)?.join("projects"))
    }

    /// The `projects` folder of every volume, ready or not, with the volume name.
    pub fn projects_dirs(&self) -> Vec<(String, PathBuf)> {
        self.names().into_iter().filter_map(|v| self.path_of(&v).map(|p| (v, p.join("projects")))).collect()
    }

    /// The `projects` folder of every volume that can be used.
    pub fn ready_projects_dirs(&self) -> Vec<(String, PathBuf)> {
        self.names().into_iter().filter_map(|v| self.ready(&v).ok().map(|p| (v, p.join("projects")))).collect()
    }

    /// The configured paths of the volumes other than `main`, usable or not. The OverlayFS
    /// registry needs every one: a tree under an unusable volume must still find the store home.
    #[cfg_attr(not(feature = "overlayfs"), allow(dead_code))]
    pub fn volume_paths(&self) -> Vec<PathBuf> {
        self.volumes.values().cloned().collect()
    }

    /// The paths of the volumes other than `main` that can be used.
    pub fn ready_volume_paths(&self) -> Vec<PathBuf> {
        self.volumes.keys().filter_map(|v| self.ready(v).ok()).collect()
    }

    /// Projects of the store that live on a volume which cannot be used: (project, volume, reason).
    pub fn missing(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for (project, volume) in &self.projects {
            if let Err(e) = self.ready(volume) {
                out.push((project.clone(), volume.clone(), format!("{e:#}")));
            }
        }
        out
    }

    /// The volumes (other than `main`) and the projects that live on each.
    pub fn projects_on(&self, volume: &str) -> Vec<String> {
        self.projects.iter().filter(|(_, v)| v.as_str() == volume).map(|(p, _)| p.clone()).collect()
    }
}

/// The project that a folder lies in, for a folder below any `projects` folder of the store.
pub fn project_of_path(layout: &Layout, path: &Path) -> Option<String> {
    layout.projects_dirs().iter().find_map(|(_, dir)| {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        path.strip_prefix(&dir).ok()?.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned())
    })
}

fn lock(root: &Path) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(root.join(LOCK_FILE))
        .context("open the volume lock")?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(file)
}

fn save_volumes(root: &Path, file: &VolumesFile) -> Result<()> {
    fsutil::atomic_write(&root.join(VOLUMES_FILE), &serde_json::to_vec_pretty(file)?, 0o600)
}

fn save_projects(root: &Path, file: &ProjectsFile) -> Result<()> {
    fsutil::atomic_write(&root.join(PROJECTS_FILE), &serde_json::to_vec_pretty(file)?, 0o600)
}

/// The volume configuration as it is in the files (read under the lock by the writers below).
fn volumes_file(layout: &Layout) -> VolumesFile {
    VolumesFile {
        format: FORMAT,
        default: layout.default.clone(),
        volumes: layout.volumes.iter().map(|(n, p)| (n.clone(), VolumeEntry { path: p.clone() })).collect(),
    }
}

/// Choose the volume of a new project. Returns the volume and whether this call recorded the
/// project in `project-volumes.json` (the caller removes the record when its create fails).
/// An existing project keeps its volume; a name that exists on any usable volume is refused.
pub fn assign(root: &Path, machine: &str, project: &str, requested: Option<&str>) -> Result<(String, bool)> {
    let _lock = lock(root)?;
    let layout = Layout::load(root, machine)?;
    if let Some(volume) = layout.projects.get(project) {
        if requested.is_some_and(|r| r != volume) {
            bail!("project '{project}' is on volume '{volume}'; it cannot be placed on '{}'", requested.unwrap_or(""));
        }
        return Ok((volume.clone(), false));
    }
    for (volume, projects) in layout.ready_projects_dirs() {
        if projects.join(project).exists() {
            bail!("{} already exists (volume '{volume}'); nothing was changed", projects.join(project).display());
        }
    }
    let volume = requested.unwrap_or(&layout.default).to_string();
    if !layout.is_volume(&volume) {
        bail!("no volume '{volume}'; see layr volume list");
    }
    layout.ready(&volume)?;
    if volume == MAIN {
        return Ok((volume, false));
    }
    let mut file = match read_json::<ProjectsFile>(&root.join(PROJECTS_FILE))? {
        Some(f) => f,
        None => ProjectsFile { format: FORMAT, projects: BTreeMap::new() },
    };
    file.projects.insert(project.to_string(), volume.clone());
    file.format = FORMAT;
    save_projects(root, &file)?;
    Ok((volume, true))
}

/// Remove the record of a project placed by `assign` (its create failed).
pub fn unassign(root: &Path, project: &str) -> Result<()> {
    let _lock = lock(root)?;
    let Some(mut file) = read_json::<ProjectsFile>(&root.join(PROJECTS_FILE))? else { return Ok(()) };
    if file.projects.remove(project).is_some() {
        save_projects(root, &file)?;
    }
    Ok(())
}

/// Register a new volume at `path` (which must exist). `backend` checks the file system of the
/// build. Creates the identity file and the `projects` and `trash` folders. Returns the canonical path.
pub fn add(root: &Path, machine: &str, name: &str, path: &Path, backend: impl FnOnce(&Path) -> Result<()>) -> Result<PathBuf> {
    check_name("volume", name)?;
    if name == MAIN {
        bail!("'main' is the store root and cannot be added");
    }
    let _lock = lock(root)?;
    let layout = Layout::load(root, machine)?;
    if layout.volumes.contains_key(name) {
        bail!("volume '{name}' already exists");
    }
    let path = fs::canonicalize(path)
        .with_context(|| format!("volume path {} does not exist (create the mount first)", path.display()))?;
    if !path.is_dir() {
        bail!("volume path {} is not a folder", path.display());
    }
    let store = fs::canonicalize(root)?;
    for (other, other_path) in layout.names().into_iter().filter_map(|v| layout.path_of(&v).map(|p| (v, p))) {
        let other_path = fs::canonicalize(&other_path).unwrap_or(other_path);
        if path.starts_with(&other_path) || other_path.starts_with(&path) {
            bail!("volume path {} overlaps volume '{other}' at {}", path.display(), other_path.display());
        }
    }
    if path.starts_with(&store) || store.starts_with(&path) {
        bail!("volume path {} overlaps the store root {}", path.display(), store.display());
    }
    free_folder(&path)?;
    backend(&path)?;
    let id_path = path.join(IDENTITY);
    match fs::read(&id_path) {
        Ok(data) => {
            let id: Identity = serde_json::from_slice(&data).map_err(|_| anyhow!("invalid {}", id_path.display()))?;
            if id.format != FORMAT || id.store != machine || id.volume != name {
                bail!("{} belongs to another store or volume; it is refused", path.display());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = Identity { format: FORMAT, store: machine.to_string(), volume: name.to_string() };
            fsutil::atomic_write(&id_path, &serde_json::to_vec(&id)?, 0o600)?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::create_dir_all(path.join("projects")).context("create the projects folder of the volume")?;
    fs::create_dir_all(path.join("trash")).context("create the trash folder of the volume")?;
    let mut file = volumes_file(&layout);
    file.volumes.insert(name.to_string(), VolumeEntry { path: path.clone() });
    save_volumes(root, &file)?;
    Ok(path)
}

/// A folder that a volume can take: empty, apart from hidden entries, `lost+found`, the
/// identity file and empty `projects` and `trash` folders (a volume added again after a remove).
fn free_folder(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "lost+found" || name == IDENTITY {
            continue;
        }
        if name == "projects" || name == "trash" {
            if fs::read_dir(entry.path())?.next().is_some() {
                bail!("{}/{name} is not empty; a volume needs an empty folder", path.display());
            }
            continue;
        }
        bail!("{} is not empty (it has {name}); a volume needs an empty folder", path.display());
    }
    Ok(())
}

/// Remove a volume from the configuration. It must hold no project and must not be the default.
/// Its trash is emptied when the volume can be used. Its files stay on the disk.
pub fn remove(root: &Path, machine: &str, name: &str) -> Result<()> {
    let _lock = lock(root)?;
    let layout = Layout::load(root, machine)?;
    if name == MAIN {
        bail!("'main' is the store root and cannot be removed");
    }
    if !layout.volumes.contains_key(name) {
        bail!("no volume '{name}'; see layr volume list");
    }
    let holders = layout.projects_on(name);
    if !holders.is_empty() {
        bail!("volume '{name}' holds {} project(s): {}; move or delete them first", holders.len(), holders.join(", "));
    }
    if layout.default == name {
        bail!("volume '{name}' is the default volume; run layr volume default <other> first");
    }
    if let Ok(path) = layout.ready(name) {
        crate::store::empty_all(&path.join("trash"));
    }
    let mut file = volumes_file(&layout);
    file.volumes.remove(name);
    save_volumes(root, &file)
}

/// Make a volume the default for new projects.
pub fn set_default(root: &Path, machine: &str, name: &str) -> Result<()> {
    let _lock = lock(root)?;
    let layout = Layout::load(root, machine)?;
    if !layout.is_volume(name) {
        bail!("no volume '{name}'; see layr volume list");
    }
    layout.ready(name)?;
    let mut file = volumes_file(&layout);
    file.default = name.to_string();
    save_volumes(root, &file)
}

/// Space of a volume's file system: (used, free) bytes.
pub fn space(path: &Path) -> Result<(u64, u64)> {
    let st = rustix::fs::statvfs(path)?;
    let unit = st.f_frsize;
    Ok(((st.f_blocks - st.f_bfree) * unit, st.f_bavail * unit))
}

/// The file system type of a volume folder.
pub fn fs_type(path: &Path) -> String {
    crate::sys::fs_type_name(path).unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("layr-volumes-{name}-{}", crate::ids::new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_store_without_files_has_only_main() {
        let root = temp("none");
        let layout = Layout::load(&root, "m1").unwrap();
        assert_eq!(layout.names(), vec![MAIN.to_string()]);
        assert_eq!(layout.volume_of("app%keys"), MAIN);
        assert_eq!(layout.project_dir("app").unwrap(), root.join("projects/app"));
        assert_eq!(layout.project_dir("app%keys").unwrap(), root.join("projects/app/meta/overlays/keys"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_volume_needs_its_identity() {
        let root = temp("store");
        let vol = temp("vol");
        let layout = Layout::load(&root, "m1").unwrap();
        assert!(layout.ready(MAIN).is_ok());
        // Not a volume yet: the folder has no identity file.
        let mut file = volumes_file(&layout);
        file.volumes.insert("fast".into(), VolumeEntry { path: vol.clone() });
        save_volumes(&root, &file).unwrap();
        let layout = Layout::load(&root, "m1").unwrap();
        assert!(layout.ready("fast").unwrap_err().to_string().contains("no layr-volume.json"));
        // Another store's identity is refused.
        let id = Identity { format: FORMAT, store: "other".into(), volume: "fast".into() };
        fs::write(vol.join(IDENTITY), serde_json::to_vec(&id).unwrap()).unwrap();
        assert!(layout.ready("fast").unwrap_err().to_string().contains("another store or volume"));
        // This store's identity is accepted.
        let id = Identity { format: FORMAT, store: "m1".into(), volume: "fast".into() };
        fs::write(vol.join(IDENTITY), serde_json::to_vec(&id).unwrap()).unwrap();
        assert_eq!(layout.ready("fast").unwrap(), vol);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&vol);
    }

    #[test]
    fn a_project_on_a_missing_volume_is_refused_with_its_volume() {
        let root = temp("missing");
        let gone = root.join("not-mounted");
        let mut file = volumes_file(&Layout::load(&root, "m1").unwrap());
        file.volumes.insert("slow".into(), VolumeEntry { path: gone.clone() });
        save_volumes(&root, &file).unwrap();
        save_projects(&root, &ProjectsFile { format: FORMAT, projects: BTreeMap::from([("app".into(), "slow".into())]) })
            .unwrap();
        let layout = Layout::load(&root, "m1").unwrap();
        let err = format!("{:#}", layout.project_dir("app").unwrap_err());
        assert!(err.contains("volume 'slow'") && err.contains("not-mounted"), "{err}");
        assert!(layout.project_dir("other").is_ok());
        assert_eq!(layout.missing().len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_newer_format_is_refused() {
        let root = temp("format");
        fs::write(root.join(PROJECTS_FILE), br#"{"format": 2, "projects": {}}"#).unwrap();
        assert!(Layout::load(&root, "m1").is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
