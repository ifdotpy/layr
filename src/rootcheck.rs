//! Refuses a btrfs store that a rollback of the root subvolume would empty.
//!
//! A rollback (a system snapshot restored from the boot menu, or by snapper) replaces the root
//! subvolume. Its plain files revert. Every nested line and state subvolume comes back as an
//! empty folder, and its data stays in the old root subvolume.
//!
//! - `check_store` runs at every store open. The store root, the projects folder and each moved
//!   line folder must not share a subvolume with the root file system (`ROOT_PATH_VAR`, default
//!   `/`), and no snapper config may cover them.
//! - `check_project_folder` and `check_project` run at every project open. An empty plain folder
//!   where the database expects a subvolume means the store was rolled back already.
//!
//! Only the btrfs backend checks. On APFS and OverlayFS every function does nothing.

/// `1` allows a store in the root subvolume.
pub const ALLOW_VAR: &str = "LAYR_ALLOW_ROOT_SNAPSHOTS";
/// The command-line form of `ALLOW_VAR`, accepted by `init` and `daemon`.
pub const ALLOW_FLAG: &str = "--allow-root-snapshots";

/// Remove the override flag from `argv` and set the override variable for this process. The
/// process that opens the store must be the one that receives the flag.
pub fn take_allow_flag(argv: &mut Vec<String>) {
    let before = argv.len();
    argv.retain(|arg| arg != ALLOW_FLAG);
    if argv.len() != before {
        std::env::set_var(ALLOW_VAR, "1");
    }
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
mod imp {
    use super::{ALLOW_FLAG, ALLOW_VAR};
    use crate::store::Project;
    use crate::sys;
    use anyhow::{bail, Context, Result};
    use std::fs::File;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    /// The folder whose subvolume the store must not share (the root file system). Tests set it.
    const ROOT_PATH_VAR: &str = "LAYR_ROOT_SUBVOLUME_PATH";
    /// The folder of snapper config files. Tests set it.
    const SNAPPER_DIR_VAR: &str = "LAYR_SNAPPER_CONFIGS";
    /// Newest local states whose folders each project open checks (unit: states).
    const STATES_CHECKED: usize = 8;

    /// The file system (device) and the subvolume id that hold a folder.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Place {
        dev: u64,
        subvol: u64,
    }

    /// Where `path` lives, judged by its nearest existing folder. `None` on another file system.
    fn place(path: &Path) -> Result<Option<Place>> {
        let Some(folder) = path.ancestors().find(|p| p.exists()) else {
            return Ok(None);
        };
        if !crate::storage::is_supported(folder) {
            return Ok(None);
        }
        let file = File::open(folder).with_context(|| format!("open {}", folder.display()))?;
        let dev = file.metadata()?.dev();
        let subvol = sys::subvol_id(file.as_fd()).with_context(|| format!("subvolume of {}", folder.display()))?;
        Ok(Some(Place { dev, subvol }))
    }

    fn allowed() -> bool {
        std::env::var(ALLOW_VAR).is_ok_and(|v| v == "1")
    }

    /// The SUBVOLUME value of every snapper config file.
    fn snapper_subvolumes() -> Vec<PathBuf> {
        let dir = std::env::var_os(SNAPPER_DIR_VAR).map(PathBuf::from).unwrap_or_else(|| "/etc/snapper/configs".into());
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in text.lines() {
                if let Some(value) = line.trim().strip_prefix("SUBVOLUME=") {
                    out.push(PathBuf::from(value.trim().trim_matches(|c: char| c == '"' || c == '\'')));
                }
            }
        }
        out
    }

    /// Refuse a store, projects folder or moved line folder that the root subvolume or a snapper
    /// config covers. Runs before the store folder is created.
    pub fn check_store(root: &Path) -> Result<()> {
        if allowed() || place(root)?.is_none() {
            return Ok(());
        }
        let base_folder = std::env::var_os(ROOT_PATH_VAR).map(PathBuf::from).unwrap_or_else(|| "/".into());
        let base = place(&base_folder)?;
        let mut folders = vec![("store folder", root.to_path_buf()), ("projects folder", root.join("projects"))];
        // A moved folder that no longer exists cannot be a subvolume of the root.
        folders
            .extend(crate::linepaths::moved_folders(root).into_iter().filter(|p| p.exists()).map(|p| ("moved line folder", p)));
        let snapper = snapper_subvolumes();
        for (what, folder) in folders {
            let Some(here) = place(&folder)? else {
                continue;
            };
            if base == Some(here) {
                bail!(
                    "the {what} {} is in the same subvolume as {}, the root file system.\n\
                     A rollback of that subvolume (a system snapshot restored from the boot menu, or by snapper) \
                     reverts the plain files of the store and leaves every line and state subvolume as an empty folder. \
                     The data stays in the old root subvolume.\n\
                     Fix: put the store in its own subvolume, for example @layr mounted at the store path \
                     (see docs/btrfs-root-snapshots.md).\n\
                     To use this store anyway, add {ALLOW_FLAG} to init or daemon, or set {ALLOW_VAR}=1.",
                    folder.display(),
                    base_folder.display()
                );
            }
            for sv in &snapper {
                if folder.starts_with(sv) && place(sv)? == Some(here) {
                    bail!(
                        "the {what} {} is covered by the snapper config for {}.\n\
                         Snapper snapshots that subvolume, so a rollback empties the store (see docs/btrfs-root-snapshots.md).\n\
                         Put the store in its own subvolume, or add {ALLOW_FLAG} to init or daemon, or set {ALLOW_VAR}=1.",
                        folder.display(),
                        sv.display()
                    );
                }
            }
        }
        Ok(())
    }

    fn rollback(what: &str, path: &Path) -> anyhow::Error {
        anyhow::anyhow!(
            "the store looks rolled back by a system snapshot: the {what} {} is an empty folder, but the database says it is a subvolume.\n\
             The data is in the old root subvolume, not in this store.\n\
             To recover, run as root:\n\
             \x20 findmnt -no SOURCE /                  # the device of the root file system\n\
             \x20 mount -o subvolid=5 <device> /mnt     # the top-level subvolume\n\
             \x20 ls /mnt                               # find the previous @ (for example @.bak or a snapper snapshot)\n\
             Then copy the store folder back from it.\n\
             Do not run gc or save on this store.",
            path.display()
        )
    }

    /// A folder that is a plain folder with no content on btrfs. A rollback leaves exactly this.
    /// Other file systems have no subvolumes to lose, so a plain folder there is not a rollback.
    fn emptied(path: &Path) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return false;
        };
        meta.is_dir()
            && crate::storage::is_supported(path)
            && !crate::storage::is_subvolume(path)
            && std::fs::read_dir(path).map(|mut entries| entries.next().is_none()).unwrap_or(false)
    }

    fn refuse_emptied(what: &str, path: &Path) -> Result<()> {
        if emptied(path) {
            return Err(rollback(what, path));
        }
        Ok(())
    }

    /// The project database folder (`meta/db`) must not be an emptied folder. Runs before the
    /// database opens, so a rolled back store is never written to.
    pub fn check_project_folder(project_dir: &Path) -> Result<()> {
        refuse_emptied("project database folder", &project_dir.join("meta").join("db"))
    }

    /// The working folders of this machine's lines and the newest local state folders.
    pub fn check_project(p: &Project) -> Result<()> {
        for line in p.db.lines()? {
            if line.machine == p.store.machine.id {
                refuse_emptied(&format!("working folder of line {}", line.name), &p.line_path(&line.name))?;
            }
        }
        for id in p.db.newest_present_states(STATES_CHECKED)? {
            refuse_emptied(&format!("state {id}"), &p.state_path(&id))?;
        }
        Ok(())
    }
}

#[cfg(not(all(target_os = "linux", not(feature = "overlayfs"))))]
mod imp {
    use crate::store::Project;
    use anyhow::Result;
    use std::path::Path;

    pub fn check_store(_root: &Path) -> Result<()> {
        Ok(())
    }

    pub fn check_project_folder(_project_dir: &Path) -> Result<()> {
        Ok(())
    }

    pub fn check_project(_p: &Project) -> Result<()> {
        Ok(())
    }
}

pub use imp::{check_project, check_project_folder, check_store};
