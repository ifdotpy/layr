//! File operations inside a folder tree that another user can change at the same time (a line
//! belongs to its owner, the service runs as root).
//!
//! Every path is resolved from a descriptor of the tree root with `openat2` and
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`: a folder that the owner replaces with a link fails
//! the resolution instead of sending a root write outside the tree. The last component is then
//! used with `*at` calls that never follow a link.

use crate::fsutil::{meta_of, Meta};
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const SNAP_DESTROY_V2: libc::c_ulong = 0x5000_943f;
const SUBVOL_CREATE_V2: libc::c_ulong = 0x5000_9418;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a path"))
}

fn openat2(dir: RawFd, path: &str, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let p = cstr(if path.is_empty() { "." } else { path })?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: mode as u64,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    };
    let fd = unsafe { libc::syscall(libc::SYS_openat2, dir, p.as_ptr(), &how as *const OpenHow, std::mem::size_of::<OpenHow>()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn split(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

fn vol_args(fd: i64, flags: u64, name: &str) -> Box<VolArgsV2> {
    let mut a = Box::new(VolArgsV2 { fd, transid: 0, flags, unused: [0; 4], name: [0; 4040] });
    let b = name.as_bytes();
    a.name[..b.len().min(4039)].copy_from_slice(&b[..b.len().min(4039)]);
    a
}

pub struct Tree {
    root: OwnedFd,
    pub path: PathBuf,
}

impl Tree {
    pub fn open(path: &Path) -> Result<Tree> {
        let p = CString::new(path.as_os_str().to_string_lossy().as_bytes())?;
        let fd = unsafe { libc::open(p.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(anyhow!("open {}: {}", path.display(), io::Error::last_os_error()));
        }
        Ok(Tree { root: unsafe { OwnedFd::from_raw_fd(fd) }, path: path.to_path_buf() })
    }

    fn check_rel(rel: &str) -> Result<()> {
        crate::fsutil::check_rel(rel)
    }

    /// A descriptor of the folder `rel_dir` (no links on the way).
    fn dir(&self, rel_dir: &str) -> io::Result<OwnedFd> {
        // A readable descriptor (not O_PATH): btrfs ioctls need one.
        openat2(self.root.as_raw_fd(), rel_dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)
    }

    fn parent(&self, rel: &str) -> Result<(OwnedFd, CString)> {
        Self::check_rel(rel)?;
        let (p, n) = split(rel);
        if n.is_empty() {
            bail!("empty name");
        }
        let d = self.dir(p).with_context(|| format!("{}: folder '{p}' (a link or missing)", self.path.display()))?;
        Ok((d, cstr(n)?))
    }

    pub fn stat(&self, rel: &str) -> Option<Meta> {
        if rel.is_empty() {
            let f = unsafe { File::from_raw_fd(libc::dup(self.root.as_raw_fd())) };
            return f.metadata().ok().map(|m| meta_of(&m));
        }
        let (d, n) = self.parent(rel).ok()?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(d.as_raw_fd(), n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return None;
        }
        // Open the entry as a path to get std metadata (the type is already known: no follow).
        let fd = unsafe { libc::openat(d.as_raw_fd(), n.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let f = unsafe { File::from_raw_fd(fd) };
        f.metadata().ok().map(|m| meta_of(&m))
    }

    /// Create the folders of `rel_dir` (replacing a non-folder in the way).
    pub fn mkdirs(&self, rel_dir: &str, own: Option<(u32, u32)>) -> Result<()> {
        Self::check_rel(rel_dir)?;
        let mut cur = self.dir("")?;
        for comp in rel_dir.split('/').filter(|c| !c.is_empty()) {
            let n = cstr(comp)?;
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let exists = unsafe { libc::fstatat(cur.as_raw_fd(), n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0;
            if exists && (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
                check(unsafe { libc::unlinkat(cur.as_raw_fd(), n.as_ptr(), 0) })?;
            }
            if !exists || (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
                let rc = unsafe { libc::mkdirat(cur.as_raw_fd(), n.as_ptr(), 0o755) };
                if rc != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(anyhow!("mkdir {comp}: {}", io::Error::last_os_error()));
                }
                if let Some((u, g)) = own {
                    check(unsafe { libc::fchownat(cur.as_raw_fd(), n.as_ptr(), u, g, libc::AT_SYMLINK_NOFOLLOW) })?;
                }
            }
            cur =
                openat2(cur.as_raw_fd(), comp, libc::O_RDONLY | libc::O_DIRECTORY, 0).with_context(|| format!("enter {comp}"))?;
        }
        Ok(())
    }

    pub fn mkdirs_for(&self, rel: &str, own: Option<(u32, u32)>) -> Result<()> {
        self.mkdirs(split(rel).0, own)
    }

    /// Remove an entry; a folder with its content (nested subvolumes are destroyed).
    pub fn remove(&self, rel: &str) -> Result<()> {
        // A parent that is missing, a file or a link: nothing of that name is in the tree.
        let (d, n) = match self.parent(rel) {
            Ok(x) => x,
            Err(_) => return Ok(()),
        };
        remove_at(d.as_raw_fd(), &n)
    }

    /// Remove a folder only if it is empty. Returns true if removed.
    pub fn rmdir(&self, rel: &str) -> bool {
        match self.parent(rel) {
            Ok((d, n)) => unsafe { libc::unlinkat(d.as_raw_fd(), n.as_ptr(), libc::AT_REMOVEDIR) == 0 },
            Err(_) => false,
        }
    }

    /// A new file at `rel` (an existing entry there is removed first). Never through a link.
    pub fn create(&self, rel: &str, mode: u32) -> Result<File> {
        self.mkdirs_for(rel, None)?;
        let (d, n) = self.parent(rel)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(d.as_raw_fd(), n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
            remove_at(d.as_raw_fd(), &n)?;
        }
        let fd = unsafe {
            libc::openat(
                d.as_raw_fd(),
                n.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(anyhow!("create {rel}: {}", io::Error::last_os_error()));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// An existing regular file opened for writing, or None (absent, a link, not a file).
    pub fn open_write(&self, rel: &str) -> Result<Option<File>> {
        let (d, n) = match self.parent(rel) {
            Ok(x) => x,
            Err(_) => return Ok(None),
        };
        let fd = unsafe {
            libc::openat(d.as_raw_fd(), n.as_ptr(), libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        };
        if fd < 0 {
            return Ok(None);
        }
        let f = unsafe { File::from_raw_fd(fd) };
        if !f.metadata()?.is_file() {
            return Ok(None);
        }
        Ok(Some(f))
    }

    /// A regular file opened for reading, never through a link.
    pub fn open_read(&self, rel: &str) -> Result<File> {
        let (d, n) = self.parent(rel)?;
        let fd = unsafe {
            libc::openat(d.as_raw_fd(), n.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        };
        if fd < 0 {
            return Err(anyhow!("open {rel}: {}", io::Error::last_os_error()));
        }
        let f = unsafe { File::from_raw_fd(fd) };
        if !f.metadata()?.is_file() {
            bail!("{rel} is not a regular file");
        }
        Ok(f)
    }

    pub fn read_link(&self, rel: &str) -> Result<String> {
        let (d, n) = self.parent(rel)?;
        let mut buf = vec![0u8; 4096];
        let len = unsafe { libc::readlinkat(d.as_raw_fd(), n.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if len < 0 {
            return Err(anyhow!("readlink {rel}: {}", io::Error::last_os_error()));
        }
        buf.truncate(len as usize);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    pub fn symlink(&self, rel: &str, target: &str) -> Result<()> {
        self.mkdirs_for(rel, None)?;
        let (d, n) = self.parent(rel)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(d.as_raw_fd(), n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
            remove_at(d.as_raw_fd(), &n)?;
        }
        let t = cstr(target)?;
        check(unsafe { libc::symlinkat(t.as_ptr(), d.as_raw_fd(), n.as_ptr()) }).with_context(|| format!("symlink {rel}"))?;
        Ok(())
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (df, nf) = self.parent(from)?;
        self.mkdirs_for(to, None)?;
        let (dt, nt) = self.parent(to)?;
        check(unsafe { libc::renameat(df.as_raw_fd(), nf.as_ptr(), dt.as_raw_fd(), nt.as_ptr()) })
            .with_context(|| format!("move {from} to {to}"))?;
        Ok(())
    }

    pub fn chown(&self, rel: &str, uid: u32, gid: u32) -> Result<()> {
        if rel.is_empty() {
            check(unsafe { libc::fchownat(self.root.as_raw_fd(), c"".as_ptr(), uid, gid, libc::AT_EMPTY_PATH) })?;
            return Ok(());
        }
        let (d, n) = self.parent(rel)?;
        check(unsafe { libc::fchownat(d.as_raw_fd(), n.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW) })
            .with_context(|| format!("chown {rel}"))?;
        Ok(())
    }

    /// Set the mode of a file or folder (a link is left alone).
    pub fn chmod(&self, rel: &str, mode: u32) -> Result<()> {
        let (d, n) = if rel.is_empty() { (self.dir("")?, cstr(".")?) } else { self.parent(rel)? };
        let fd = unsafe {
            libc::openat(d.as_raw_fd(), n.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        };
        if fd < 0 {
            return Ok(());
        }
        let f = unsafe { OwnedFd::from_raw_fd(fd) };
        check(unsafe { libc::fchmod(f.as_raw_fd(), mode & 0o7777) })?;
        Ok(())
    }

    /// Names in a folder of the tree.
    pub fn list(&self, rel_dir: &str) -> Result<Vec<String>> {
        let d = openat2(self.root.as_raw_fd(), rel_dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        list_fd(d.as_raw_fd())
    }

    pub fn create_subvolume(&self, rel: &str) -> Result<()> {
        let (d, n) = self.parent(rel)?;
        let mut a = vol_args(0, 0, n.to_str().unwrap_or(""));
        check(unsafe { libc::ioctl(d.as_raw_fd(), SUBVOL_CREATE_V2 as _, &mut *a) })
            .with_context(|| format!("create subvolume {rel}"))?;
        Ok(())
    }

    /// Destroy every subvolume nested in the tree (before the tree root is deleted).
    pub fn destroy_nested(&self) -> Result<()> {
        let d = openat2(self.root.as_raw_fd(), "", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        destroy_nested_in(d.as_raw_fd())
    }
}

fn list_fd(dir: RawFd) -> Result<Vec<String>> {
    let dup = unsafe { libc::dup(dir) };
    if dup < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let dp = unsafe { libc::fdopendir(dup) };
    if dp.is_null() {
        unsafe { libc::close(dup) };
        return Err(io::Error::last_os_error().into());
    }
    let mut v = Vec::new();
    unsafe {
        libc::rewinddir(dp);
        loop {
            let e = libc::readdir64(dp);
            if e.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*e).d_name.as_ptr()).to_string_lossy().into_owned();
            if name != "." && name != ".." {
                v.push(name);
            }
        }
        libc::closedir(dp);
    }
    Ok(v)
}

fn is_subvol_root(st: &libc::stat, parent_dev: u64) -> bool {
    st.st_ino == 256 && st.st_dev != parent_dev
}

/// Remove `name` in the folder `dir`, recursively, never following links.
fn remove_at(dir: RawFd, name: &CString) -> Result<()> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Ok(());
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        check(unsafe { libc::unlinkat(dir, name.as_ptr(), 0) })?;
        return Ok(());
    }
    let mut pst: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(dir, &mut pst) })?;
    let sub =
        unsafe { libc::openat(dir, name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if sub < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let sub = unsafe { OwnedFd::from_raw_fd(sub) };
    if is_subvol_root(&st, pst.st_dev) {
        destroy_nested_in(sub.as_raw_fd())?;
        drop(sub);
        let mut a = vol_args(0, 0, name.to_str().unwrap_or(""));
        check(unsafe { libc::ioctl(dir, SNAP_DESTROY_V2 as _, &mut *a) })?;
        return Ok(());
    }
    for child in list_fd(sub.as_raw_fd())? {
        remove_at(sub.as_raw_fd(), &cstr(&child)?)?;
    }
    drop(sub);
    check(unsafe { libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) })?;
    Ok(())
}

fn destroy_nested_in(dir: RawFd) -> Result<()> {
    let mut pst: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(dir, &mut pst) })?;
    for child in list_fd(dir)? {
        let n = cstr(&child)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir, n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            continue;
        }
        if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
            continue;
        }
        if is_subvol_root(&st, pst.st_dev) {
            remove_at(dir, &n)?;
        } else {
            let sub =
                unsafe { libc::openat(dir, n.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
            if sub >= 0 {
                let sub = unsafe { OwnedFd::from_raw_fd(sub) };
                destroy_nested_in(sub.as_raw_fd())?;
            }
        }
    }
    Ok(())
}
