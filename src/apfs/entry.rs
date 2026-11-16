//! Native descriptor copy executors. Shared callers retain copy and publication policy.
use super::*;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::{ffi::OsStr, os::fd::AsFd};
pub(crate) fn clone_new(src: &File, dir: &File, name: &OsStr) -> Result<Option<File>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => return Ok(None),
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    match crate::sys::apfs_clone_at(src.as_fd(), dir.as_fd(), name) {
        Ok(()) => {}
        Err(e) if matches!(e.raw_os_error(), Some(libc::EXDEV | libc::ENOTSUP)) => {
            return match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(rustix::io::Errno::NOENT) => Ok(None),
                _ => Err(e.into()),
            };
        }
        Err(e) => return Err(e.into()),
    }
    let fd = rustix::fs::openat(&dir, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
    crate::sys::apfs_unseal_fd(fd.as_fd())?;
    let mode = rustix::fs::fstat(&fd)?.st_mode;
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode | 0o200))?;
    let fd = rustix::fs::openat(&dir, name, OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
    Ok(Some(File::from(fd)))
}
pub(crate) fn replace_private(src: &File, dir: &File, name: &OsStr, finish: &mut dyn FnMut(&File) -> Result<()>) -> Result<bool> {
    use std::os::macos::fs::MetadataExt as _;
    use std::os::unix::fs::MetadataExt;
    let old = match rustix::fs::openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(_) => return Ok(false),
    };
    let meta = old.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.st_flags() != 0 {
        return Ok(false);
    }
    let temp = format!(".layr-copy-{}", crate::ids::new_id());
    let Some(prepared) = clone_new(src, dir, std::ffi::OsStr::new(&temp))? else { return Ok(false) };
    let result = (|| -> Result<bool> {
        // Keep destination attributes, ACL and origin identity, just as an in-place
        // copy does. COPYFILE_METADATA replaces attributes without copying data.
        crate::sys::apfs_copy_metadata_fd(old.as_fd(), prepared.as_fd())?;
        finish(&prepared)?;
        let current = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if current.st_ino != meta.ino() || current.st_dev as u64 != meta.dev() || current.st_nlink != 1 {
            bail!("index destination changed during copy: {name:?}");
        }
        rustix::fs::renameat(&dir, temp.as_str(), &dir, name)?;
        Ok(true)
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&dir, temp.as_str(), AtFlags::empty());
    }
    result
}
