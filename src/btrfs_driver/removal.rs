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
        backend(
            crate::sys::subvol_set_flags(file.as_fd(), flags & !crate::sys::SUBVOL_RDONLY)
                .map_err(|error| anyhow::anyhow!("unseal owned root: {error:#}")),
        )?;
    }
    let mode = file.metadata()?.mode() & 0o7777;
    if mode & 0o700 != 0o700 {
        rustix::fs::fchmod(file, Mode::from_raw_mode(mode | 0o700))
            .map_err(|error| FsError::backend(anyhow::anyhow!("open owned root permissions: {error}")))?;
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
        rustix::fs::mkdirat(
            driver.anchor.as_ref(),
            OsStr::from_bytes(operation.name.as_bytes()),
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        )
        .map_err(|error| FsError::backend(anyhow::anyhow!("create removal custody: {error}")))?;
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
    fn acquire(&mut self, driver: &BtrfsFs) -> Result<()> {
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
            // A nested root inherits authority only while it still occupies its observed
            // parent slot. Normal users must unseal readonly roots before renaming them.
            if !matches(self.tree.reader.directory.root_file(), &location.parent, &location.name)? {
                return Err(FsError::LocationChanged);
            }
            prepare_owned(self.tree.reader.directory.root_file())?;
            driver.step("quarantine-rename")?;
            let captured = Placement::Custody { stage: stage.clone(), restore: location.clone() };
            rustix::fs::renameat_with(
                location.parent.as_ref(),
                OsStr::from_bytes(location.name.as_bytes()),
                stage.directory.as_ref(),
                "tree",
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|error| FsError::backend(anyhow::anyhow!("acquire owned removal custody: {error}")))?;
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
    pub fn remove(&mut self, driver: &BtrfsFs) -> Result<()> {
        // Extracted children remain owned even when an earlier attempt was interrupted.
        self.finish_children(driver)?;
        if matches!(self.placement, Placement::Removed) {
            return Ok(());
        }
        if self.tree.reader.directory.root_file().metadata()?.nlink() == 0 {
            self.placement = Placement::Removed;
            return Ok(());
        }
        self.acquire(driver)?;
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
            Err(error) if crate::fs::is_not_empty(&error) => {}
            Err(error) => return Err(FsError::backend(error)),
        }
        let root = self.tree.reader.directory.root_file().try_clone()?;
        // The usual nested subvolume (a working folder's .git) goes first, with no search.
        {
            let _t = crate::trace::span("delete: known nested subvolumes");
            self.known(driver, &root, b".git")?;
            if crate::sys::subvol_destroy(directory.as_fd(), "tree").is_ok() {
                self.placement = Placement::Removed;
                return Ok(());
            }
        }
        let _t = crate::trace::span("delete: full search for nested subvolumes");
        self.nested(driver, &root)?;
        backend(crate::sys::subvol_destroy(directory.as_fd(), "tree"))?;
        self.placement = Placement::Removed;
        Ok(())
    }
    /// Take the nested subvolume `name` directly below `parent` into custody and destroy it.
    fn known(&mut self, driver: &BtrfsFs, parent: &File, name: &[u8]) -> Result<()> {
        let parent_metadata = parent.metadata()?;
        let child = match rustix::fs::openat(
            parent,
            OsStr::from_bytes(name),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(child) => File::from(child),
            Err(_) => return Ok(()),
        };
        let metadata = child.metadata()?;
        if metadata.ino() != 256 || metadata.dev() == parent_metadata.dev() {
            return Ok(());
        }
        let name = RelPath::new(name.to_vec())?;
        driver.step("nested-opened")?;
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
        self.finish_children(driver)
    }
    fn finish_children(&mut self, driver: &BtrfsFs) -> Result<()> {
        while let Some(child) = self.children.last_mut() {
            match child.remove(driver) {
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
                // Every nested root enters verified custody before unsealing or deletion.
                // A descriptor that moved outside before acquisition never grants authority.
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
