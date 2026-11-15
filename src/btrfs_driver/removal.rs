//! Bind deletion to the owned root before using destructive named ioctls.
use super::creation::{OperationDirectory, Provenance, Stage};
use super::*;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::ffi::OsStr;

#[derive(Clone)]
pub(super) struct Locator {
    pub parent: Arc<File>,
    pub name: RelPath,
}
pub(super) enum Placement {
    Public(Locator),
    // No destruction is permitted until this candidate has matched the owned descriptor.
    Custody { stage: Stage, restore: Locator },
    Private(Stage),
    Removed,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Acquisition {
    Owned,
    Descendant,
}

pub(super) struct Pinned {
    pub tree: NativeTree,
    pub placement: Placement,
    pub cleanup: Vec<OperationDirectory>,
    pub children: Vec<Pinned>,
}
pub(super) fn cleanup_directories(driver: &BtrfsFs, pending: &mut Vec<OperationDirectory>) -> Result<()> {
    while let Some(directory) = pending.last() {
        driver.step("directory-cleanup")?;
        directory.remove()?;
        pending.pop();
    }
    Ok(())
}
fn matches(file: &File, parent: &File, name: &RelPath) -> Result<bool> {
    let expected = file.metadata()?;
    match rustix::fs::statat(parent, OsStr::from_bytes(name.as_bytes()), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(actual) => Ok(expected.dev() == actual.st_dev && expected.ino() == actual.st_ino),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => Ok(false),
        Err(error) => Err(FsError::Io(error.into())),
    }
}
fn prepare_owned(file: &File) -> Result<()> {
    let flags = crate::sys::subvol_flags(file.as_fd())?;
    if flags & crate::sys::SUBVOL_RDONLY != 0 {
        crate::sys::subvol_set_flags(file.as_fd(), flags & !crate::sys::SUBVOL_RDONLY)?;
    }
    let mode = file.metadata()?.mode() & 0o7777;
    if mode & 0o700 != 0o700 {
        io(rustix::fs::fchmod(file, Mode::from_raw_mode(mode | 0o700)))?;
    }
    Ok(())
}
impl Pinned {
    fn stage(&mut self, driver: &BtrfsFs) -> Result<Stage> {
        creation::trusted(&driver.anchor, false)?;
        let operation = OperationDirectory {
            anchor: driver.anchor.clone(),
            name: RelPath::new(format!(".layr-fs-{}", crate::ids::new_id()).as_bytes())?,
        };
        self.cleanup.reserve(1);
        let record = operation.clone();
        io(rustix::fs::mkdirat(
            driver.anchor.as_ref(),
            OsStr::from_bytes(operation.name.as_bytes()),
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        ))?;
        self.cleanup.push(record);
        driver.step("quarantine-directory")?;
        let directory = Arc::new(File::from(io(rustix::fs::openat(
            driver.anchor.as_ref(),
            OsStr::from_bytes(operation.name.as_bytes()),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ))?));
        Ok(Stage { operation, directory })
    }
    fn acquire(&mut self, driver: &BtrfsFs, acquisition: Acquisition) -> Result<()> {
        if let Placement::Public(location) = &self.placement {
            if !matches(self.tree.reader.directory.root_file(), &location.parent, &location.name)? {
                return Err(FsError::LocationChanged);
            }
            cleanup_directories(driver, &mut self.cleanup)?;
            let stage = self.stage(driver)?;
            driver.step("quarantine-precheck")?;
            let Placement::Public(location) = &self.placement else {
                return Err(FsError::Invalid("public locator absent"));
            };
            if acquisition == Acquisition::Owned {
                prepare_owned(self.tree.reader.directory.root_file())?;
            }
            driver.step("quarantine-rename")?;
            let captured = Placement::Custody { stage: stage.clone(), restore: location.clone() };
            io(rustix::fs::renameat_with(
                location.parent.as_ref(),
                OsStr::from_bytes(location.name.as_bytes()),
                stage.directory.as_ref(),
                "tree",
                rustix::fs::RenameFlags::NOREPLACE,
            ))?;
            self.placement = captured;
            driver.step("quarantined")?;
        }
        if let Placement::Custody { stage, restore } = &self.placement {
            driver.step("quarantine-inspect")?;
            if matches(self.tree.reader.directory.root_file(), &stage.directory, &RelPath::new(b"tree".to_vec())?)? {
                self.placement = Placement::Private(stage.clone());
            } else {
                driver.step("restore-custody")?;
                let restored = restore.clone();
                rustix::fs::renameat_with(
                    stage.directory.as_ref(),
                    "tree",
                    restore.parent.as_ref(),
                    OsStr::from_bytes(restore.name.as_bytes()),
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(|error| {
                    FsError::backend(anyhow::anyhow!(
                        "restore displaced tree from private slot {:?}/tree: {error}",
                        stage.operation.name.as_bytes()
                    ))
                })?;
                self.placement = Placement::Public(restored);
                cleanup_directories(driver, &mut self.cleanup)?;
                return Err(FsError::LocationChanged);
            }
        }
        Ok(())
    }
    pub fn remove(&mut self, driver: &BtrfsFs, acquisition: Acquisition) -> Result<()> {
        // Extracted children remain owned even when an earlier attempt was interrupted.
        self.finish_children(driver)?;
        if matches!(self.placement, Placement::Removed) {
            return Ok(());
        }
        if self.tree.reader.directory.root_file().metadata()?.nlink() == 0 {
            self.placement = Placement::Removed;
            return Ok(());
        }
        self.acquire(driver, acquisition)?;
        let Placement::Private(stage) = &self.placement else {
            return Err(FsError::Invalid("deletion has no verified private slot"));
        };
        let directory = stage.directory.clone();
        prepare_owned(self.tree.reader.directory.root_file())?;
        driver.step("destroy-root")?;
        match crate::sys::subvol_destroy(directory.as_fd(), "tree") {
            Ok(()) => {
                self.placement = Placement::Removed;
                return Ok(());
            }
            Err(error) if native::is_not_empty(&error) => {}
            Err(error) => return Err(FsError::backend(error)),
        }
        let root = self.tree.reader.directory.root_file().try_clone()?;
        self.nested(driver, &root)?;
        backend(crate::sys::subvol_destroy(directory.as_fd(), "tree"))?;
        self.placement = Placement::Removed;
        Ok(())
    }
    fn finish_children(&mut self, driver: &BtrfsFs) -> Result<()> {
        while let Some(child) = self.children.last_mut() {
            match child.remove(driver, Acquisition::Descendant) {
                Ok(()) => {}
                Err(FsError::LocationChanged) if matches!(child.placement, Placement::Public(_)) => {
                    // A nested root that left its observed parent before acquisition is no
                    // longer in the tree. Do not follow its retained FD into an outside tree.
                    cleanup_directories(driver, &mut child.cleanup)?;
                }
                Err(error) => return Err(error),
            }
            driver.step("child-destroyed")?;
            cleanup_directories(driver, &mut child.cleanup)?;
            self.children.pop();
        }
        Ok(())
    }
    fn nested(&mut self, driver: &BtrfsFs, parent: &File) -> Result<()> {
        let parent_metadata = parent.metadata()?;
        let mut names = Vec::new();
        for entry in io(rustix::fs::Dir::read_from(parent))? {
            let entry = io(entry)?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(RelPath::new(name.to_vec())?);
            }
        }
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for name in names {
            let stat = match rustix::fs::statat(parent, OsStr::from_bytes(name.as_bytes()), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(FsError::Io(error.into())),
            };
            if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
                continue;
            }
            let child = match rustix::fs::openat(
                parent,
                OsStr::from_bytes(name.as_bytes()),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(child) => File::from(child),
                Err(error)
                    if error == rustix::io::Errno::ACCESS && (stat.st_ino != 256 || stat.st_dev == parent_metadata.dev()) =>
                {
                    continue
                }
                Err(error) => return Err(FsError::Io(error.into())),
            };
            let metadata = child.metadata()?;
            if metadata.ino() == 256 && metadata.dev() != parent_metadata.dev() {
                driver.step("nested-opened")?;
                // This parent remains inside the acquired subvolume: ordinary directories
                // cannot cross subvolume boundaries. Named deletion never follows a moved-out
                // child by ID. Only roots requiring descent are acquired into private custody.
                match crate::sys::subvol_destroy_bytes(parent.as_fd(), name.as_bytes()) {
                    Ok(()) => continue,
                    Err(error)
                        if error.chain().any(|cause| {
                            cause.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                        }) =>
                    {
                        continue
                    }
                    Err(error) if native::is_not_empty(&error) => {}
                    Err(error) => return Err(FsError::backend(error)),
                }
                let reader = Arc::new(Reader { directory: DirectoryTree::from_file(child)? });
                self.children.push(Pinned {
                    tree: NativeTree {
                        path: self.tree.path.join(OsStr::from_bytes(name.as_bytes())),
                        reader,
                        access: Access::Working,
                        provenance: Provenance::Other,
                    },
                    placement: Placement::Public(Locator { parent: Arc::new(parent.try_clone()?), name }),
                    cleanup: Vec::new(),
                    children: Vec::new(),
                });
                self.finish_children(driver)?;
            } else {
                // Ordinary directories cannot move out across btrfs subvolume boundaries.
                self.nested(driver, &child)?;
            }
        }
        Ok(())
    }
}
