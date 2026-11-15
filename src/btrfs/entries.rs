//! Native entry IO delegates managed-tree destruction to its ownership protocol.
use anyhow::{Context, Result};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    sync::Arc,
};
struct Directory {
    parent: Arc<File>,
    name: OsString,
    file: Arc<File>,
    entries: std::vec::IntoIter<OsString>,
}
fn enter(parent: Arc<File>, name: OsString) -> Result<Option<Directory>> {
    let stat = match rustix::fs::statat(parent.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        rustix::fs::unlinkat(parent.as_ref(), &name, AtFlags::empty())?;
        return Ok(None);
    }
    if stat.st_ino == 256 && stat.st_dev != rustix::fs::fstat(parent.as_ref())?.st_dev {
        // /proc resolves only the retained parent. Admission pins the leaf and subsequent
        // removal keeps its locators and anchor alive, including in a retry error.
        let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(&name);
        let result = crate::fs::remove(&path);
        result.with_context(|| format!("remove owned subvolume {name:?}, mode {:o}, owner {}", stat.st_mode, stat.st_uid))?;
        return Ok(None);
    }
    let file = Arc::new(File::from(
        rustix::fs::openat(
            parent.as_ref(),
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("open removal directory {name:?}, inode {}, mode {:o}", stat.st_ino, stat.st_mode))?,
    ));
    let mode = rustix::fs::fstat(file.as_ref())?.st_mode;
    if mode & 0o700 != 0o700 {
        rustix::fs::fchmod(file.as_ref(), Mode::from_raw_mode((mode & 0o7777) | 0o700))?;
    }
    let mut entries = Vec::new();
    for entry in rustix::fs::Dir::read_from(file.as_ref())? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            entries.push(OsStr::from_bytes(name).to_owned());
        }
    }
    Ok(Some(Directory { parent, name, file, entries: entries.into_iter() }))
}
/// Keep deep directory traversal off the call stack. A failed owned deletion stops the walk
/// immediately; it never starts another deletion around that operation's private custody.
pub(crate) fn remove_at(parent: &OwnedFd, name: &str) -> Result<()> {
    let Some(root) = enter(Arc::new(File::from(rustix::io::dup(parent)?)), OsString::from(name))? else { return Ok(()) };
    let mut pending = vec![root];
    while let Some(current) = pending.last_mut() {
        if let Some(name) = current.entries.next() {
            if let Some(child) = enter(current.file.clone(), name)? {
                pending.push(child);
            }
        } else {
            let Some(done) = pending.pop() else { break };
            rustix::fs::unlinkat(done.parent.as_ref(), &done.name, AtFlags::REMOVEDIR)?;
        }
    }
    Ok(())
}
