//! Private creation, descriptor acquisition, and identity-preserving publication.
use super::*;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::ffi::OsStr;

pub(super) struct NativeTree {
    pub path: PathBuf,
    pub reader: Arc<Reader>,
    pub access: Access,
    pub provenance: Provenance,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Provenance {
    Snapshot,
    Other,
}

#[derive(Clone)]
pub(super) struct OperationDirectory {
    pub(super) anchor: Arc<File>,
    pub(super) name: RelPath,
}
impl OperationDirectory {
    pub(super) fn remove(&self) -> Result<()> {
        match rustix::fs::unlinkat(self.anchor.as_ref(), OsStr::from_bytes(self.name.as_bytes()), AtFlags::REMOVEDIR) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(FsError::Io(error.into())),
        }
    }
}
#[derive(Clone)]
pub(super) struct Stage {
    pub(super) operation: OperationDirectory,
    pub(super) directory: Arc<File>,
}
pub(super) enum Resource {
    Vacant,
    Directories(Vec<OperationDirectory>),
    Staged { stage: Stage, cleanup: Vec<OperationDirectory> },
    Borrowed(NativeTree),
    Pinned(Pinned),
}
impl Resource {
    pub fn pinned(&self) -> Option<&NativeTree> {
        match self {
            Self::Pinned(pinned) => Some(&pinned.tree),
            Self::Borrowed(tree) => Some(tree),
            _ => None,
        }
    }
    pub fn remove(&mut self, driver: &BtrfsFs) -> Result<()> {
        match self {
            Self::Vacant => Ok(()),
            Self::Borrowed(_) => Err(FsError::Invalid("borrowed tree has no deletion authority")),
            Self::Directories(directories) => removal::cleanup_directories(driver, directories),
            Self::Staged { stage, cleanup } => {
                match rustix::fs::statat(stage.directory.as_ref(), "tree", AtFlags::SYMLINK_NOFOLLOW) {
                    Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(FsError::Io(error.into())),
                    Ok(stat) => {
                        if stat.st_ino != 256
                            || rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory
                        {
                            return Err(FsError::Invalid("private creation slot is not a subvolume root"));
                        }
                        io(rustix::fs::chmodat(
                            stage.directory.as_ref(),
                            "tree",
                            Mode::from_raw_mode((stat.st_mode & 0o7777) | 0o700),
                            AtFlags::empty(),
                        ))?;
                        backend(crate::sys::subvol_destroy(stage.directory.as_fd(), "tree"))?;
                    }
                }
                *self = Self::Directories(std::mem::take(cleanup));
                driver.step("destroyed")?;
                self.remove(driver)
            }
            Self::Pinned(pinned) => {
                pinned.remove(driver, removal::Acquisition::Owned)?;
                *self = Self::Directories(std::mem::take(&mut pinned.cleanup));
                driver.step("destroyed")?;
                self.remove(driver)
            }
        }
    }
}
pub(super) enum Source<'a> {
    Snapshot(&'a File),
    Copy(&'a dyn TreeRead),
}
pub(super) fn trusted(file: &File, private: bool) -> Result<()> {
    let metadata = file.metadata()?;
    let mask = if private { 0o077 } else { 0o022 };
    if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & mask != 0 {
        return Err(FsError::Invalid(if private {
            "frozen publication requires a private destination parent"
        } else {
            "staging anchor must be owned and writable only by the filesystem service"
        }));
    }
    Ok(())
}
impl BtrfsFs {
    /// The caller supplies an existing trusted namespace on the destination filesystem.
    /// Its retained descriptor, owner and permission checks protect private-slot provenance.
    pub(crate) fn new(anchor: File) -> Result<Self> {
        trusted(&anchor, false)?;
        crate::sys::filesystem_uuid(anchor.as_fd())?;
        Ok(Self {
            anchor: Arc::new(anchor),
            issuer: TreeIssuer::default(),
            trees: Mutex::new(HashMap::new()),
            destinations: Mutex::new(HashMap::new()),
            #[cfg(test)]
            fault: Mutex::new(None),
        })
    }
    /// Legacy adapters select a protected existing ancestor and pass its descriptor explicitly.
    /// Ancestor names can move afterward: staging never resolves them again.
    pub(crate) fn for_path(path: &Path) -> Result<Arc<Self>> {
        Self::select_anchor(path, None)
    }
    pub(crate) fn for_snapshot(source: &Path, destination: &Path) -> Result<Arc<Self>> {
        let source = File::open(source)?;
        Self::select_anchor(destination, Some(subvolume_id(&source)?))
    }
    fn select_anchor(path: &Path, excluded: Option<u64>) -> Result<Arc<Self>> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        let mut directory = File::open(parent)?;
        let filesystem = crate::sys::filesystem_uuid(directory.as_fd())?;
        loop {
            if trusted(&directory, false).is_ok()
                && excluded.is_none_or(|id| crate::sys::subvol_info(directory.as_fd()).is_ok_and(|info| info.tree_id != id))
            {
                return Ok(Arc::new(Self::new(directory)?));
            }
            let next = File::from(io(rustix::fs::openat(
                &directory,
                "..",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            ))?);
            let (a, b) = (directory.metadata()?, next.metadata()?);
            if (a.dev(), a.ino()) == (b.dev(), b.ino()) || crate::sys::filesystem_uuid(next.as_fd())? != filesystem {
                return Err(FsError::Invalid("no trusted staging anchor outside the source tree"));
            }
            directory = next;
        }
    }
    pub(super) fn step(&self, point: &'static str) -> Result<()> {
        #[cfg(test)]
        {
            if let Some(fault) = self.fault.lock().map_err(|_| FsError::Invalid("creation fault lock poisoned"))?.as_mut() {
                fault(point)?;
            }
        }
        #[cfg(not(test))]
        let _ = point;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn set_fault(&self, fault: impl FnMut(&'static str) -> Result<()> + Send + 'static) -> Result<()> {
        *self.fault.lock().map_err(|_| FsError::Invalid("creation fault lock poisoned"))? = Some(Box::new(fault));
        Ok(())
    }
    pub(super) fn rollback(&self, ownership: OwnedTree, cause: FsError) -> FsError {
        match self.remove(ownership) {
            Ok(()) => cause,
            Err(failure) => FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) },
        }
    }
    pub(super) fn create(&self, source: Source<'_>, dst: NewTreeLocation, access: Access) -> Result<(Arc<Reader>, OwnedTree)> {
        let destination = self.destination(&dst)?;
        trusted(&self.anchor, false)?;
        if access == Access::Frozen {
            trusted(&destination.parent, true)?;
        }
        if crate::sys::filesystem_uuid(self.anchor.as_fd())? != crate::sys::filesystem_uuid(destination.parent.as_fd())? {
            return Err(FsError::Invalid("staging anchor is on another filesystem"));
        }
        if let Source::Snapshot(source) = source {
            if crate::sys::subvol_info(source.as_fd())?.tree_id == crate::sys::subvol_info(self.anchor.as_fd())?.tree_id {
                return Err(FsError::Invalid("snapshot staging anchor is inside its source tree"));
            }
        }
        let operation = OperationDirectory {
            anchor: self.anchor.clone(),
            name: RelPath::new(format!(".layr-fs-{}", crate::ids::new_id()).as_bytes())?,
        };
        let ownership = self.issuer.own();
        let key = self.issuer.owned_key(&ownership)?;
        self.trees()?.insert(key.clone(), Resource::Vacant);
        let created = (|| -> Result<Arc<Reader>> {
            // Update the existing reservation under the same lock as mkdir: no post-mkdir
            // allocation or second lock acquisition can lose directory cleanup authority.
            {
                let mut trees = self.trees()?;
                let resource = trees.get_mut(&key).ok_or(FsError::Invalid("creation reservation absent"))?;
                let record = vec![operation.clone()];
                io(rustix::fs::mkdirat(
                    self.anchor.as_ref(),
                    OsStr::from_bytes(operation.name.as_bytes()),
                    Mode::RUSR | Mode::WUSR | Mode::XUSR,
                ))?;
                *resource = Resource::Directories(record);
            }
            self.step("operation-directory")?;
            let directory = Arc::new(File::from(io(rustix::fs::openat(
                self.anchor.as_ref(),
                OsStr::from_bytes(operation.name.as_bytes()),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ))?));
            let stage = Stage { operation, directory };
            self.trees()?.insert(key.clone(), Resource::Staged { stage: stage.clone(), cleanup: vec![stage.operation.clone()] });
            // The reserved private slot remains authoritative even if an ioctl reports an
            // error after committing some kernel work. Rollback inspects only this slot.
            match source {
                Source::Snapshot(file) => {
                    backend(crate::sys::subvol_snapshot(file.as_fd(), stage.directory.as_fd(), "tree", false))?;
                    self.step("snapshot-created")?;
                }
                Source::Copy(_) => {
                    backend(crate::sys::subvol_create(stage.directory.as_fd(), "tree"))?;
                    self.step("copy-created")?;
                }
            }
            self.step("created")?;
            let initial = io(rustix::fs::statat(stage.directory.as_ref(), "tree", AtFlags::SYMLINK_NOFOLLOW))?;
            let original_mode = initial.st_mode & 0o7777;
            // Only owner bits change. Restoring the original mode also restores the POSIX ACL
            // owner/mask projection; named and default ACL entries stay attached to the snapshot.
            io(rustix::fs::chmodat(
                stage.directory.as_ref(),
                "tree",
                Mode::from_raw_mode(original_mode | 0o700),
                AtFlags::empty(),
            ))?;
            self.step("open-created")?;
            let root = File::from(io(rustix::fs::openat(
                stage.directory.as_ref(),
                "tree",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ))?);
            subvolume_id(&root)?;
            let reader = Arc::new(Reader { directory: DirectoryTree::from_file(root)? });
            self.trees()?.insert(
                key.clone(),
                Resource::Pinned(Pinned {
                    tree: NativeTree {
                        path: destination.path.clone(),
                        reader: reader.clone(),
                        access,
                        provenance: match source {
                            Source::Snapshot(_) => Provenance::Snapshot,
                            Source::Copy(_) => Provenance::Other,
                        },
                    },
                    placement: Placement::Private(stage.clone()),
                    cleanup: vec![stage.operation.clone()],
                    children: Vec::new(),
                }),
            );
            let mode = match source {
                Source::Copy(tree) => {
                    layr_fs_common::copy_tree(tree, reader.as_ref())?;
                    reader.directory.root_file().metadata()?.mode() & 0o7777
                }
                Source::Snapshot(_) => original_mode,
            };
            self.step("prepared")?;
            let root = reader.directory.root_file();
            io(rustix::fs::fchmod(root, Mode::from_raw_mode(mode | 0o700)))?;
            self.step("publish")?;
            // NOREPLACE leaves an unrelated destination untouched. A mount-instance EXDEV is
            // an error with rollback, even when filesystem UUIDs match.
            {
                let mut trees = self.trees()?;
                let Some(Resource::Pinned(pinned)) = trees.get_mut(&key) else {
                    return Err(FsError::Invalid("prepared root absent"));
                };
                let location = Locator { parent: destination.parent.clone(), name: dst.name().clone() };
                io(rustix::fs::renameat_with(
                    stage.directory.as_ref(),
                    "tree",
                    destination.parent.as_ref(),
                    OsStr::from_bytes(dst.name().as_bytes()),
                    rustix::fs::RenameFlags::NOREPLACE,
                ))?;
                pinned.placement = Placement::Public(location);
            }
            self.step("published")?;
            io(rustix::fs::fchmod(root, Mode::from_raw_mode(mode)))?;
            if root.metadata()?.mode() & 0o7777 != mode {
                return Err(FsError::Invalid("snapshot root mode could not be restored"));
            }
            if access == Access::Frozen {
                crate::sys::subvol_set_flags(root.as_fd(), crate::sys::SUBVOL_RDONLY)?;
                native::remember_snapshot(&destination.path);
            }
            let mut trees = self.trees()?;
            if let Some(Resource::Pinned(pinned)) = trees.get_mut(&key) {
                removal::cleanup_directories(self, &mut pinned.cleanup)?;
            }
            Ok(reader)
        })();
        match created {
            Ok(reader) => Ok((reader, ownership)),
            Err(cause) => Err(self.rollback(ownership, cause)),
        }
    }
}

/// Keep the operation-local registry alive when the legacy facade returns a retry token.
/// Callers can downcast the error and retry through the retained driver and authority.
pub(crate) struct NativeCleanup {
    pub driver: Arc<BtrfsFs>,
    pub failure: RemovalFailure,
    pub(super) cause: FsError,
}
impl std::fmt::Debug for NativeCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeCleanup").field("cause", &self.cause).field("failure", &self.failure).finish_non_exhaustive()
    }
}
impl std::fmt::Display for NativeCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; cleanup requires retry: {} (staging anchor {:?})", self.cause, self.failure.error, self.driver.anchor)?;
        if let (Ok(key), Ok(trees)) = (self.driver.issuer.owned_key(&self.failure.ownership), self.driver.trees()) {
            match trees.get(&key) {
                Some(Resource::Directories(directories)) => {
                    for directory in directories {
                        write!(f, "; pending directory {:?}", OsStr::from_bytes(directory.name.as_bytes()))?;
                    }
                }
                Some(Resource::Staged { stage, .. }) => {
                    write!(f, "; private slot {:?}/tree", OsStr::from_bytes(stage.operation.name.as_bytes()))?
                }
                Some(Resource::Pinned(pinned)) => match &pinned.placement {
                    Placement::Public(location) => {
                        write!(f, "; original slot {:?}/{:?}", location.parent, OsStr::from_bytes(location.name.as_bytes()))?
                    }
                    Placement::Custody { stage, restore } => write!(
                        f,
                        "; custody {:?}/tree; restore to {:?}/{:?}",
                        OsStr::from_bytes(stage.operation.name.as_bytes()),
                        restore.parent,
                        OsStr::from_bytes(restore.name.as_bytes())
                    )?,
                    Placement::Private(stage) => {
                        write!(f, "; private slot {:?}/tree", OsStr::from_bytes(stage.operation.name.as_bytes()))?
                    }
                    Placement::Removed => {}
                },
                _ => {}
            }
        }
        Ok(())
    }
}
impl std::error::Error for NativeCleanup {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

#[cfg(test)]
#[path = "creation_tests.rs"]
mod tests;
