//! Identity-checked custody for entries outside a driver's managed-tree namespace.
use layr_fs_core::{FsError, Result};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    os::unix::fs::MetadataExt,
    path::Path,
};
fn io<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|error| FsError::Io(error.into()))
}
enum Phase {
    Public,
    Ready(File),
    Inspect(File),
    Owned(File),
    Foreign(File),
    Restored,
    Removed,
}
pub(crate) struct Entry {
    parent: File,
    name: OsString,
    file: File,
    phase: Phase,
    pending: Vec<OsString>,
}
impl Entry {
    pub(crate) fn claim(path: &Path) -> Result<Self> {
        let parent = File::open(path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")))?;
        let name = path.file_name().ok_or(FsError::Invalid("entry has no name"))?.to_owned();
        #[cfg(target_os = "linux")]
        let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        #[cfg(target_os = "macos")]
        let flags =
            OFlags::from_bits_retain(u32::try_from(libc::O_EVTONLY).map_err(|_| FsError::Invalid("invalid event-only flag"))?)
                | OFlags::SYMLINK
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC;
        let file = File::from(io(rustix::fs::openat(&parent, &name, flags, Mode::empty()))?);
        Ok(Self { parent, name, file, phase: Phase::Public, pending: Vec::new() })
    }
    fn matches(&self, parent: &File, name: &OsStr) -> Result<bool> {
        let expected = self.file.metadata()?;
        match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(actual) => Ok((expected.dev(), expected.ino()) == (actual.st_dev as u64, actual.st_ino as u64)),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => Ok(false),
            Err(error) => Err(FsError::Io(error.into())),
        }
    }
    fn finish(&mut self) -> Result<()> {
        while let Some(name) = self.pending.last() {
            match rustix::fs::unlinkat(&self.parent, name, AtFlags::REMOVEDIR) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(FsError::Io(error.into())),
            }
            self.pending.pop();
        }
        Ok(())
    }
    pub(crate) fn remove(&mut self, erase: impl Fn(&File, &OsStr, &File) -> Result<()>) -> Result<()> {
        loop {
            match &self.phase {
                Phase::Public => {
                    if !self.matches(&self.parent, &self.name)? {
                        return Err(FsError::LocationChanged);
                    }
                    let parent = self.parent.metadata()?;
                    let uid = rustix::process::geteuid().as_raw();
                    if !((parent.uid() == uid && parent.mode() & 0o022 == 0)
                        || (parent.uid() == 0 && parent.mode() & 0o1000 != 0))
                    {
                        return Err(FsError::Invalid("cleanup needs an owned or root-owned sticky parent"));
                    }
                    let name = OsString::from(format!(".layr-remove-{}", crate::ids::new_id()));
                    self.pending.reserve(1);
                    io(rustix::fs::mkdirat(&self.parent, &name, Mode::from_raw_mode(0o700)))?;
                    self.pending.push(name.clone());
                    let directory = File::from(io(rustix::fs::openat(
                        &self.parent,
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    ))?);
                    self.phase = Phase::Ready(directory);
                }
                Phase::Ready(stage) => {
                    io(rustix::fs::renameat_with(&self.parent, &self.name, stage, "tree", RenameFlags::NOREPLACE))?;
                    let Phase::Ready(stage) = std::mem::replace(&mut self.phase, Phase::Public) else {
                        return Err(FsError::Invalid("custody transition changed"));
                    };
                    self.phase = Phase::Inspect(stage);
                }
                Phase::Inspect(stage) => {
                    let owned = self.matches(stage, OsStr::new("tree"))?;
                    let Phase::Inspect(stage) = std::mem::replace(&mut self.phase, Phase::Public) else {
                        return Err(FsError::Invalid("custody transition changed"));
                    };
                    self.phase = if owned { Phase::Owned(stage) } else { Phase::Foreign(stage) };
                }
                Phase::Owned(stage) => {
                    if self.file.metadata()?.nlink() != 0 {
                        erase(stage, OsStr::new("tree"), &self.file)?;
                    }
                    self.phase = Phase::Removed;
                }
                Phase::Foreign(stage) => {
                    io(rustix::fs::renameat_with(stage, "tree", &self.parent, &self.name, RenameFlags::NOREPLACE))?;
                    self.phase = Phase::Restored;
                }
                Phase::Restored => {
                    self.finish()?;
                    self.phase = Phase::Public;
                    return Err(FsError::LocationChanged);
                }
                Phase::Removed => {
                    self.finish()?;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(all(test, target_os = "linux", not(feature = "overlayfs")))]
mod tests {
    use super::*;
    #[test]
    fn public_cleanup_retry_resumes_partial_custody_without_touching_replacement() -> anyhow::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let directory = std::env::temp_dir().join(format!("layr-custody-{}", crate::ids::new_id()));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let tree = directory.join("tree");
        std::fs::create_dir(&tree)?;
        std::fs::write(tree.join("first"), b"removed before the failure")?;
        let blocked = tree.join("blocked");
        std::fs::create_dir(&blocked)?;
        std::fs::write(blocked.join("last"), b"retained")?;
        let root = File::open(&tree)?;
        let held = File::open(&blocked)?;
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0))?;
        let error = crate::fs::remove(&tree).expect_err("normal users cannot enumerate a mode000 child");
        assert!(!tree.exists(), "failure must retain private custody");
        std::fs::create_dir(&tree)?;
        std::fs::write(tree.join("sentinel"), b"keep")?;
        rustix::fs::fchmod(&held, Mode::from_raw_mode(0o700))?;
        let cleanup = error.downcast::<crate::fs::Cleanup>()?;
        cleanup.retry().map_err(anyhow::Error::new)?;
        assert_eq!(root.metadata()?.nlink(), 0);
        assert_eq!(std::fs::read(tree.join("sentinel"))?, b"keep");
        drop(root);
        drop(held);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux", not(feature = "overlayfs")))]
#[test]
fn ordinary_cleanup_can_remove_nested_readonly_managed_trees() -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let path = std::env::temp_dir().join(format!("layr-nested-custody-{}", crate::ids::new_id()));
    std::fs::DirBuilder::new().mode(0o700).create(&path)?;
    let tree = path.join("tree");
    crate::fs::create(&tree)?;
    std::fs::write(tree.join("file"), b"data")?;
    std::fs::write(path.join(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"non-utf8-\xff")), b"owned")?;
    let nested = tree.join("nested");
    crate::fs::create(&nested)?;
    std::fs::write(nested.join("child"), b"nested readonly")?;
    crate::fs::set_readonly(&nested)?;
    crate::fs::set_readonly(&tree)?;
    let restricted = path.join("restricted");
    std::fs::create_dir(&restricted)?;
    std::fs::write(restricted.join("file"), b"owned")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o444))?;
    crate::fs::remove(&path)?;
    assert!(!path.exists());
    Ok(())
}
