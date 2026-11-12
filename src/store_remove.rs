//! `layr store remove <path>`: delete a whole store folder (docs/commands.md, "Remove a store").
//!
//! Layr seals store entries (on macOS the immutable flag and a deny entry in the ACL; on btrfs read-only
//! subvolumes), so `rm -rf` of a store fails. This command checks that the folder is a layr store that no
//! layr process uses, unmounts what is mounted below it, and deletes it with the delete of the store backend
//! (`storage::delete_tree`), which opens each sealed entry first. It runs in the caller's process and never
//! opens the store: an open would write to it.

use crate::args::{parse, Spec};
use anyhow::{bail, Context, Result};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: layr store remove <path> [--force] [-q]";

pub fn run(args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("-f|--force").flag("-q|--quiet"), args)?;
    let rest = a.rest();
    match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["remove", path] => {
            let root = remove(Path::new(path), a.has("-f"))?;
            if !a.has("-q") {
                println!("Removed store {}", root.display());
            }
            Ok(0)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

/// Delete the store at `given`. Returns its resolved path.
fn remove(given: &Path, force: bool) -> Result<PathBuf> {
    let meta = fs::symlink_metadata(given).with_context(|| format!("cannot read {}", given.display()))?;
    if meta.file_type().is_symlink() {
        bail!("{} is a symbolic link; give the path of the store folder itself. Nothing was removed.", given.display());
    }
    if !meta.is_dir() {
        bail!("{} is not a folder. Nothing was removed.", given.display());
    }
    let root = crate::store::resolve_root(given)?;
    if root.parent().is_none() {
        bail!("refusing to remove {}. Nothing was removed.", root.display());
    }
    if !crate::store_version::is_store(&root)? && !remains(&root)? {
        bail!(
            "{} is not a layr store: it has no {} and no machine/id. Nothing was removed.",
            root.display(),
            crate::store_version::MARKER
        );
    }
    let own = crate::store::resolve_root(&crate::store::default_root()).ok();
    if own.as_deref() == Some(root.as_path()) && !force {
        bail!(
            "{} is the store of this shell (LAYR_ROOT, or the default store). Add --force to remove it. Nothing was removed.",
            root.display()
        );
    }
    // Every layr process that opened the store holds a shared lock on the version lock, and the trash
    // emptier holds the trash lock. Hold both until the store is gone: a new command waits and then
    // finds no store.
    let _version = exclusive(&root.join(".layr-version.lock"), "a layr command or daemon uses the store")?;
    let _trash = exclusive(&root.join("trash.lock"), "a layr process empties the trash of the store")?;
    unmount_below(&root)?;
    let outside = outside_data(&root);
    crate::storage::delete_tree(&root).with_context(|| format!("remove {}", root.display()))?;
    if fs::symlink_metadata(&root).is_ok() {
        bail!("{} is still there after the delete", root.display());
    }
    for (path, what) in outside {
        if fs::symlink_metadata(&path).is_ok() {
            eprintln!("layr: kept {what} outside the store: {}", path.display());
        }
    }
    Ok(root)
}

/// What `rm -rf` leaves of a store on macOS, after it deleted the marker: sealed entries (the immutable flag or the
/// seal entry of layr in the ACL) and the folders that lead to them, with at least one sealed entry below
/// `projects/`. A file that is not sealed means the folder holds more than a store, so it is not removed.
#[cfg(target_os = "macos")]
fn remains(root: &Path) -> Result<bool> {
    use std::os::macos::fs::MetadataExt;
    fn only_sealed(dir: &Path, found: &mut bool) -> Result<bool> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let m = fs::symlink_metadata(&path)?;
            if m.st_flags() & libc::UF_IMMUTABLE != 0 || crate::sys::apfs_has_seal_entry(&path)? {
                *found = true;
            } else if !m.is_dir() || !only_sealed(&path, found)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    let (mut anywhere, mut in_projects) = (false, false);
    Ok(root.join("projects").is_dir()
        && only_sealed(root, &mut anywhere)?
        && only_sealed(&root.join("projects"), &mut in_projects)?
        && in_projects)
}

/// Other systems keep no remains that this command takes for a store.
#[cfg(not(target_os = "macos"))]
fn remains(_root: &Path) -> Result<bool> {
    Ok(false)
}

/// An exclusive lock on `path` when the file exists; `busy` names the holder when another process
/// has it.
fn exclusive(path: &Path, busy: &str) -> Result<Option<fs::File>> {
    let file = match fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => {
            bail!("{busy} ({} is locked). Stop it and run the command again. Nothing was removed.", path.display())
        }
        Err(e) => Err(e).with_context(|| format!("lock {}", path.display())),
    }
}

/// Unmount each file system mounted below `root`, the deepest first (a virtual line of LayrFS, a test
/// mount). The root itself must not be a mount point: its content belongs to that file system.
fn unmount_below(root: &Path) -> Result<()> {
    let mut below: Vec<PathBuf> = Vec::new();
    for point in crate::sys::mount_points().context("list the mounted file systems")? {
        if point == root {
            bail!(
                "{} is the root of a mounted file system; unmount it, then delete its content. Nothing was removed.",
                root.display()
            );
        }
        if point.starts_with(root) {
            below.push(point);
        }
    }
    below.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for point in below {
        let tool = if cfg!(target_os = "macos") { "/sbin/umount" } else { "umount" };
        let out = std::process::Command::new(tool).arg(&point).output().with_context(|| format!("run {tool}"))?;
        if !out.status.success() {
            bail!(
                "cannot unmount {}: {}\n(close the programs and shells that use the folder, then run the command again)",
                point.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Data of the store in other folders, which this command keeps: volumes and moved line folders.
fn outside_data(root: &Path) -> Vec<(PathBuf, &'static str)> {
    let volumes = crate::volumes::Layout::for_root(root).map(|l| l.volume_paths()).unwrap_or_default();
    let lines = crate::linepaths::moved_folders(root);
    volumes
        .into_iter()
        .map(|p| (p, "a volume"))
        .chain(lines.into_iter().map(|p| (p, "a moved line folder")))
        .filter(|(p, _)| !p.starts_with(root))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("layr-store-remove-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_folder_without_a_marker_is_refused() {
        let dir = scratch("foreign");
        fs::write(dir.join("notes.txt"), b"keep").unwrap();
        let error = remove(&dir, true).unwrap_err().to_string();
        assert!(error.contains("is not a layr store"), "{error}");
        assert!(dir.join("notes.txt").exists(), "a refused folder keeps its files");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn remains_are_sealed_entries_below_projects_only() {
        let dir = scratch("remains");
        let state = dir.join("projects/p/states/s");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("a.txt"), b"a").unwrap();
        assert!(!remains(&dir).unwrap(), "a folder without a sealed entry is no store");
        crate::sys::apfs_flag(&state, true).unwrap();
        assert!(remains(&dir).unwrap(), "a sealed state below projects is what rm -rf leaves");
        fs::write(dir.join("notes.txt"), b"keep").unwrap();
        assert!(!remains(&dir).unwrap(), "a file that is not sealed belongs to someone else");
        fs::remove_file(dir.join("notes.txt")).unwrap();
        remove(&dir, true).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn the_mount_list_has_the_root_file_system() {
        assert!(crate::sys::mount_points().unwrap().contains(&PathBuf::from("/")));
    }

    #[test]
    fn a_link_is_refused() {
        let dir = scratch("link");
        fs::create_dir(dir.join("store")).unwrap();
        fs::write(dir.join("store").join(crate::store_version::MARKER), b"{}").unwrap();
        std::os::unix::fs::symlink(dir.join("store"), dir.join("link")).unwrap();
        let error = remove(&dir.join("link"), true).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(dir.join("store").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_locked_store_is_refused() {
        let dir = scratch("busy");
        fs::create_dir(dir.join("machine")).unwrap();
        fs::write(dir.join("machine/id"), b"x").unwrap();
        let held = fs::File::create(dir.join(".layr-version.lock")).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockShared).unwrap();
        let error = remove(&dir, true).unwrap_err().to_string();
        assert!(error.contains("uses the store"), "{error}");
        assert!(dir.join("machine/id").exists());
        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }
}
