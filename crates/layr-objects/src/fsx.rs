//! File system helpers without unsafe code: extended attributes and clones through `rustix`.

use anyhow::Result;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The extended attributes that a tree records. Left out: attributes that the system stamps on
/// every file (`com.apple.provenance`), layr's own markers, and the classes that need privileges
/// or name file system internals (`trusted.`, `security.`, `system.`: SELinux labels, POSIX ACLs,
/// overlayfs whiteouts). docs/objects.md lists them.
pub fn keep_xattr(name: &[u8]) -> bool {
    name != b"com.apple.provenance"
        && !name.starts_with(b"com.layr.")
        && !name.starts_with(b"trusted.")
        && !name.starts_with(b"security.")
        && !name.starts_with(b"system.")
}

fn read_buffer(mut read: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>) -> io::Result<Vec<u8>> {
    let mut size = 256;
    loop {
        let mut buf = vec![0u8; size];
        match read(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            Err(rustix::io::Errno::RANGE) if size < 1 << 26 => size *= 4,
            Err(e) => return Err(e.into()),
        }
    }
}

fn unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(code) if code == rustix::io::Errno::NOTSUP.raw_os_error()
        || code == rustix::io::Errno::OPNOTSUPP.raw_os_error())
}

fn missing_attribute() -> rustix::io::Errno {
    #[cfg(target_os = "macos")]
    return rustix::io::Errno::NOATTR;
    #[cfg(not(target_os = "macos"))]
    rustix::io::Errno::NODATA
}

/// The kept extended attributes of `path` (not followed if it is a link), sorted by name.
pub fn xattrs(path: &Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let names = match read_buffer(|buf| rustix::fs::llistxattr(path, buf)) {
        Ok(names) => names,
        Err(e) if unsupported(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty() && keep_xattr(n)) {
        let cname = CString::new(name)?;
        match read_buffer(|buf| rustix::fs::lgetxattr(path, cname.as_c_str(), buf)) {
            Ok(value) => out.push((name.to_vec(), value)),
            // Removed between the list and the read.
            Err(e) if e.raw_os_error() == Some(missing_attribute().raw_os_error()) => {}
            Err(e) => return Err(e),
        }
    }
    out.sort();
    Ok(out)
}

/// Remove every extended attribute of the file `path` (a store copy of a file must not carry the
/// attributes of the one source it was cloned from). The file gets the owner write bit.
pub fn clear_xattrs(path: &Path) -> io::Result<()> {
    let names = match read_buffer(|buf| rustix::fs::llistxattr(path, buf)) {
        Ok(names) => names,
        Err(e) if unsupported(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    if !names.split(|b| *b == 0).any(|n| !n.is_empty() && keep_xattr(n)) {
        return Ok(());
    }
    // macOS removes no attribute of a read-only file, even for its owner.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::symlink_metadata(path)?.permissions().mode();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode((mode & 0o7777) | 0o200))?;
    // Only the attributes that a tree keeps: a security label (SELinux) belongs to the file system,
    // and removing it is refused.
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty() && keep_xattr(n)) {
        let cname = CString::new(name)?;
        match rustix::fs::lremovexattr(path, cname.as_c_str()) {
            Ok(()) => {}
            Err(e) if e == missing_attribute() => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn set_xattrs(path: &Path, attrs: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    for (name, value) in attrs {
        let cname = CString::new(name.clone())?;
        rustix::fs::lsetxattr(path, cname.as_c_str(), value, rustix::fs::XattrFlags::empty()).map_err(|e| {
            anyhow::anyhow!("cannot set extended attribute {} on {}: {e}", String::from_utf8_lossy(name), path.display())
        })?;
    }
    Ok(())
}

pub fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

/// Make `dst` (absent) a clone of the file `src`: `clonefile` on APFS, a reflink on btrfs and XFS.
/// `Ok(false)`: the file system cannot clone; nothing was made.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let source = File::open(src)?;
        match rustix::fs::fclonefileat(&source, rustix::fs::CWD, dst, rustix::fs::CloneFlags::NOOWNERCOPY) {
            Ok(()) => Ok(true),
            Err(e) if e == rustix::io::Errno::NOTSUP || e == rustix::io::Errno::XDEV => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let source = File::open(src)?;
        let target = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(dst)?;
        match rustix::fs::ioctl_ficlone(&target, &source) {
            Ok(()) => Ok(true),
            Err(e) => {
                drop(target);
                std::fs::remove_file(dst)?;
                if e == rustix::io::Errno::NOTSUP
                    || e == rustix::io::Errno::OPNOTSUPP
                    || e == rustix::io::Errno::XDEV
                    || e == rustix::io::Errno::INVAL
                    || e == rustix::io::Errno::NOTTY
                {
                    Ok(false)
                } else {
                    Err(e.into())
                }
            }
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (src, dst);
        Ok(false)
    }
}

/// `clone_file`, or a byte copy where the file system cannot clone. True for a clone.
pub fn clone_or_copy(src: &Path, dst: &Path) -> io::Result<bool> {
    if clone_file(src, dst)? {
        return Ok(true);
    }
    let mut from = File::open(src)?;
    let mut to = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    io::copy(&mut from, &mut to)?;
    Ok(false)
}
