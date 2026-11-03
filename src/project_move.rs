//! `layr project move <project> <volume>`: move a project to another volume (docs/volumes.md).
//!
//! Steps, under the move lock `project-moves/.lock` and the project lock of the source:
//! 1. The journal `project-moves/<project>.json` names the source and the target volume.
//! 2. `project_copy::copy_project` copies the folder to `<target>/projects/<project>` and verifies
//!    the copy. The git objects of the copied lines are pointed at the new folder (not live yet).
//! 3. The commit: `volumes::reassign` writes the project -> volume record in one atomic write.
//! 4. Finish: the moved line folders outside the project get the new git objects path, the old
//!    folder moves to the trash of its volume, and the journal goes.
//!
//! Recovery runs at every store open (`recover`). A journal whose move lock is free belongs to a
//! dead process. When the record names the target, step 4 runs. Otherwise the target copy is
//! removed (rollback) and a rerun of the move copies again.

use crate::project_copy::{self, CopyReport};
use crate::store::{db_file, kick_trash, Project, Store};
use crate::volumes::{self, Layout};
use crate::{fsutil, ids, linepaths, storage};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const DIR: &str = "project-moves";
const LOCK: &str = ".lock";
const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Journal {
    format: u32,
    project: String,
    from: String,
    to: String,
}

/// The result of a move.
pub struct Moved {
    pub from: String,
    pub to: String,
    pub copy: CopyReport,
}

fn journal_of(root: &Path, project: &str) -> PathBuf {
    root.join(DIR).join(format!("{project}.json"))
}

/// The move lock of the store. `None` when another process holds it and `wait` is false.
fn move_lock(root: &Path, wait: bool) -> Result<Option<File>> {
    let dir = root.join(DIR);
    fs::create_dir_all(&dir).context("create the folder of the project moves")?;
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(LOCK))?;
    let how = if wait {
        rustix::fs::FlockOperation::LockExclusive
    } else {
        rustix::fs::FlockOperation::NonBlockingLockExclusive
    };
    match rustix::fs::flock(&file, how) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The git objects folder of a project folder, as the line folders name it in `alternates`.
fn objects_of(project_dir: &Path) -> String {
    project_dir.join("git").join("objects").display().to_string()
}

/// Point the git objects of one line folder from `from` to `to` (`.git/objects/info/alternates`).
/// Only a file that names exactly `from` changes.
fn rewrite_alternates(working: &Path, from: &str, to: &str) -> Result<()> {
    let file = working.join(".git/objects/info/alternates");
    let data = match fs::read_to_string(&file) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if data.trim_end() != from {
        return Ok(());
    }
    // The `.git` folder of a line is sealed (read only): open it for the write, then seal it again.
    fsutil::git_writable(working, true);
    let result = fsutil::atomic_write(&file, format!("{to}\n").as_bytes(), 0o644);
    fsutil::git_writable(working, false);
    result.with_context(|| format!("point {} at the new folder", working.display()))
}

/// The line folders below `project_dir/lines`.
fn rewrite_lines(project_dir: &Path, from: &str, to: &str) -> Result<()> {
    let Ok(entries) = fs::read_dir(project_dir.join("lines")) else { return Ok(()) };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            rewrite_alternates(&entry.path(), from, to)?;
        }
    }
    Ok(())
}

/// The moved line folders of a project folder (`meta/line-paths.json`).
fn moved_folders(project_dir: &Path) -> Vec<PathBuf> {
    fs::read(linepaths::paths_file_in(&project_dir.join("meta")))
        .ok()
        .and_then(|d| serde_json::from_slice::<std::collections::BTreeMap<String, PathBuf>>(&d).ok())
        .map(|m| m.into_values().collect())
        .unwrap_or_default()
}

/// Move a folder to another place on its file system. Each backend moves a tree its own way:
/// sealed trees on APFS, managed mounts on OverlayFS.
fn move_folder(from: &Path, to: &Path) -> Result<()> {
    #[cfg(feature = "overlayfs")]
    return storage::rename_tree(from, to);
    #[cfg(not(feature = "overlayfs"))]
    storage::move_tree(from, to)
}

/// Remove a folder of a volume through that volume's trash (a rename on the same file system).
fn to_trash(volume_root: &Path, folder: &Path) -> Result<()> {
    let trash = volume_root.join("trash");
    fs::create_dir_all(&trash).context("create the trash of the volume")?;
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    move_folder(folder, &trash.join(format!("{name}.{}", ids::new_id())))?;
    kick_trash(&trash);
    Ok(())
}

/// The lock file of a project folder, taken and held by the caller (`Project::lock` for a
/// folder without a `Project` object).
fn lock_folder(project_dir: &Path) -> Result<File> {
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(project_dir.join("meta").join("lock"))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(file)
}

/// Working folders of processes: a process whose current folder lies in one of `roots`.
/// The mover itself is not counted.
fn in_use(roots: &[PathBuf]) -> Result<Option<(u32, PathBuf)>> {
    let me = std::process::id();
    let roots: Vec<PathBuf> = roots.iter().map(|r| fs::canonicalize(r).unwrap_or_else(|_| r.clone())).collect();
    let inside = |p: &Path| roots.iter().any(|r| p.starts_with(r));
    #[cfg(target_os = "linux")]
    {
        for entry in fs::read_dir("/proc").context("list the processes")?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if pid == me {
                continue;
            }
            if let Ok(cwd) = fs::read_link(entry.path().join("cwd")) {
                if inside(&cwd) {
                    return Ok(Some((pid, cwd)));
                }
            }
        }
        Ok(None)
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("lsof")
            .args(["-a", "-d", "cwd", "-F", "pn"])
            .output()
            .context("list the working folders of the processes (lsof)")?;
        // lsof exits with 1 when it finds nothing.
        ensure!(matches!(out.status.code(), Some(0) | Some(1)), "lsof failed: {}", String::from_utf8_lossy(&out.stderr));
        let mut pid = 0u32;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(p) = line.strip_prefix('p') {
                pid = p.parse().unwrap_or(0);
            } else if let Some(path) = line.strip_prefix('n') {
                let path = PathBuf::from(path);
                if pid != me && inside(&path) {
                    return Ok(Some((pid, path)));
                }
            }
        }
        Ok(None)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (me, inside);
        Ok(None)
    }
}

/// Test hook: `LAYR_TEST_MOVE_EXIT_AFTER=copy|switch` exits the process (status 3) at that step,
/// as a crash would. The journal and the folders stay as they are.
fn hook(stage: &str) {
    if std::env::var("LAYR_TEST_MOVE_EXIT_AFTER").is_ok_and(|v| v == stage) {
        eprintln!("test hook: exit after {stage}");
        std::process::exit(3);
    }
}

/// Remove the target copy of a move that did not commit: a rename into the target trash.
fn discard_copy(target_root: &Path, dst: &Path) -> Result<()> {
    if fs::symlink_metadata(dst).is_err() {
        return Ok(());
    }
    to_trash(target_root, dst)
}

/// Move a project to `volume`. Refused: an unknown project, volume or overlay, a volume that
/// cannot be used, a project already there, a folder in use, and a moved line on another file
/// system. A failure before the commit leaves the source as the project.
pub fn move_project(store: &Store, project: &str, volume: &str) -> Result<Moved> {
    if project.contains('%') {
        bail!("'{project}' is an overlay; it moves with its project");
    }
    let root = store.root.as_path();
    let machine = store.machine.id.as_str();
    let layout = store.layout()?;
    if !layout.is_volume(volume) {
        bail!("no volume '{volume}'; see layr volume list");
    }
    let src = store.project_dir(project)?;
    if !db_file(&src).is_file() {
        bail!("project '{project}' not found");
    }
    let from = layout.volume_of(project);
    if from == volume {
        bail!("project '{project}' is already on volume '{volume}'");
    }
    let target_root = layout.ready(volume)?;
    let dst = target_root.join("projects").join(project);
    if fs::symlink_metadata(&dst).is_ok() {
        bail!("{} already exists; nothing was changed", dst.display());
    }

    let _t = crate::trace::begin(format!("project move {project}: move lock"));
    let _moves = move_lock(root, true)?;
    drop(_t);
    let _t = crate::trace::begin(format!("project move {project}: open the project"));
    let handle = Project::open(store, project)?;
    drop(_t);
    let _t = crate::trace::begin(format!("project move {project}: project lock"));
    let _lock = handle.lock()?;
    drop(_t);
    let moved = handle.moved_paths();
    let mut watch = vec![src.clone()];
    watch.extend(moved.values().cloned());
    let _t = crate::trace::begin(format!("project move {project}: working folders in use"));
    if let Some((pid, path)) = in_use(&watch)? {
        bail!("project '{project}' is in use: process {pid} works in {}; stop it and run the move again", path.display());
    }
    let target_dev = fs::metadata(&target_root)?.dev();
    let off_volume: Vec<String> = moved
        .iter()
        .filter(|(_, p)| fs::metadata(p).is_ok_and(|m| m.dev() != target_dev))
        .map(|(line, p)| format!("{line} at {}", p.display()))
        .collect();
    if !off_volume.is_empty() {
        bail!(
            "moved line folders are not on volume '{volume}': {}; move them there (layr line move) first",
            off_volume.join(", ")
        );
    }

    let journal = Journal { format: FORMAT, project: project.into(), from: from.clone(), to: volume.into() };
    fsutil::atomic_write(&journal_of(root, project), &serde_json::to_vec(&journal)?, 0o600)?;

    let copied = (|| -> Result<CopyReport> {
        let _t = crate::trace::begin(format!("project move {project}: copy and verify"));
        let copy = project_copy::copy_project(&src, &dst, |_| {})?;
        let _t = crate::trace::begin(format!("project move {project}: git objects of the copy"));
        rewrite_lines(&dst, &objects_of(&src), &objects_of(&dst))?;
        Ok(copy)
    })();
    let copy = match copied {
        Ok(copy) => copy,
        Err(e) => {
            discard_copy(&target_root, &dst).context("remove the partial copy")?;
            fs::remove_file(journal_of(root, project))?;
            return Err(e.context(format!("copy project '{project}' to volume '{volume}'")));
        }
    };
    hook("copy");

    let _t = crate::trace::begin(format!("project move {project}: switch record"));
    if let Err(e) = volumes::reassign(root, machine, project, volume) {
        // The commit may have happened before the error: only a record that still names the source rolls back.
        if Layout::load(root, machine)?.volume_of(project) != volume {
            discard_copy(&target_root, &dst)?;
            fs::remove_file(journal_of(root, project))?;
            return Err(e.context("switch the project to the new volume"));
        }
    }
    hook("switch");

    // The project is on `volume` from here on. A failed finish is retried by the next command.
    if let Err(e) = finish(root, machine, project, &from, volume, &src, &dst) {
        eprintln!("layr: project '{project}' is on volume '{volume}'; the old folder is removed later ({e:#})");
    }
    Ok(Moved { from, to: volume.into(), copy })
}

/// Step 4: the project lives on `to`. Points the git objects of the moved lines at the new folder,
/// removes the old folder through its volume's trash, and removes the journal. Idempotent.
fn finish(root: &Path, machine: &str, project: &str, from: &str, to: &str, src: &Path, dst: &Path) -> Result<()> {
    let layout = Layout::load(root, machine)?;
    // The target must be usable: the project lives there now.
    layout.ready(to)?;
    // The target folder is live now: its project lock covers the rewrite.
    let _t = crate::trace::begin(format!("project move {project}: target project lock"));
    let _lock = lock_folder(dst)?;
    drop(_t);
    let _t = crate::trace::begin(format!("project move {project}: git objects of moved lines"));
    let (old, new) = (objects_of(src), objects_of(dst));
    rewrite_lines(dst, &old, &new)?;
    for line in moved_folders(dst) {
        rewrite_alternates(&line, &old, &new)?;
    }
    let _t = crate::trace::begin(format!("project move {project}: remove the old folder"));
    if fs::symlink_metadata(src).is_ok() {
        let from_root = layout.ready(from).with_context(|| format!("the old folder of '{project}' stays until its volume is ready"))?;
        to_trash(&from_root, src)?;
    }
    match fs::remove_file(journal_of(root, project)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Finish or roll back the moves that stopped (called by `Store::open`). Errors are reported and
/// do not stop the command: the project is on the volume its record names either way.
pub fn recover(store: &Store) {
    if let Err(e) = recover_all(store) {
        eprintln!("layr: project move recovery: {e:#}");
    }
}

fn recover_all(store: &Store) -> Result<()> {
    let Ok(entries) = fs::read_dir(store.root.join(DIR)) else { return Ok(()) };
    let journals: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    if journals.is_empty() {
        return Ok(());
    }
    // A move that runs holds the lock: its journal is not ours to resolve.
    let _t = crate::trace::begin("project move recovery: check the journals");
    let Some(_moves) = move_lock(&store.root, false)? else { return Ok(()) };
    for path in journals {
        if let Err(e) = resolve(store, &path) {
            eprintln!("layr: project move recovery of {} waits: {e:#}", path.display());
        }
    }
    Ok(())
}

fn resolve(store: &Store, path: &Path) -> Result<()> {
    let root = store.root.as_path();
    let machine = store.machine.id.as_str();
    let journal: Journal = serde_json::from_slice(&fs::read(path)?).context("read the journal")?;
    ensure!(journal.format == FORMAT, "journal format {} is not supported", journal.format);
    let layout = Layout::load(root, machine)?;
    let src = layout.path_of(&journal.from).context("the source volume is unknown")?.join("projects").join(&journal.project);
    let live = layout.volume_of(&journal.project);
    if live == journal.to {
        let dst = layout.ready(&journal.to)?.join("projects").join(&journal.project);
        finish(root, machine, &journal.project, &journal.from, &journal.to, &src, &dst)
    } else if live == journal.from {
        let target_root = layout.ready(&journal.to)?;
        discard_copy(&target_root, &target_root.join("projects").join(&journal.project))?;
        fs::remove_file(path)?;
        Ok(())
    } else {
        bail!("project '{}' is on volume '{live}', neither the source nor the target", journal.project)
    }
}
