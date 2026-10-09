//! File system helpers: metadata, content compare, reflink copies, walks and safe paths.
//!
//! Lines are writable by agents. Every write into a line checks that the parent folders of the
//! target are real folders inside the root, so a symbolic link in the line cannot redirect a write
//! of the service (which runs as root) outside the line.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FType {
    File,
    Dir,
    Symlink,
    Special,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Meta {
    pub ftype: FType,
    pub mode: u32,
    pub size: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
}

impl Meta {
    pub fn is_file(&self) -> bool {
        self.ftype == FType::File
    }
    pub fn is_dir(&self) -> bool {
        self.ftype == FType::Dir
    }
    pub fn executable(&self) -> bool {
        self.mode & 0o111 != 0
    }
    /// The git mode of this entry: 100644, 100755, 120000 or 040000.
    pub fn git_mode(&self) -> u32 {
        match self.ftype {
            FType::Symlink => 0o120000,
            FType::Dir => 0o040000,
            _ if self.executable() => 0o100755,
            _ => 0o100644,
        }
    }
}

pub fn meta_of(m: &fs::Metadata) -> Meta {
    let ft = m.file_type();
    let ftype = if ft.is_symlink() {
        FType::Symlink
    } else if ft.is_dir() {
        FType::Dir
    } else if ft.is_file() {
        FType::File
    } else {
        FType::Special
    };
    Meta { ftype, mode: m.mode() & 0o7777, size: m.len(), ino: m.ino(), uid: m.uid(), gid: m.gid(), mtime: m.mtime() }
}

/// Check a relative path: no absolute paths, no `..`, no empty components.
pub fn check_rel(path: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    let p = Path::new(path);
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            _ => bail!("invalid path: {path}"),
        }
    }
    Ok(())
}

/// Join `rel` to `root` after checking that every parent folder is a real folder (not a link).
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    check_rel(rel)?;
    let mut cur = root.to_path_buf();
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        if i + 1 < comps.len() {
            match fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => bail!("path goes through a symbolic link: {rel}"),
                _ => {}
            }
        }
    }
    Ok(cur)
}

/// Metadata of `rel` below `root` without following a final link. `None` if absent.
pub fn lmeta(root: &Path, rel: &str) -> Option<Meta> {
    let p = safe_join(root, rel).ok()?;
    fs::symlink_metadata(p).ok().map(|m| meta_of(&m))
}

pub fn read_link(root: &Path, rel: &str) -> Result<String> {
    let p = safe_join(root, rel)?;
    Ok(fs::read_link(&p).with_context(|| format!("readlink {}", p.display()))?.to_string_lossy().into_owned())
}

/// Read a regular file (not following links).
pub fn read_file(root: &Path, rel: &str) -> Result<Vec<u8>> {
    let p = safe_join(root, rel)?;
    let mut f =
        OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&p).with_context(|| format!("open {}", p.display()))?;
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    Ok(v)
}

/// The bytes of an entry for diffs: file content, or the link target for a symbolic link.
pub fn entry_bytes(root: &Path, rel: &str) -> Result<Vec<u8>> {
    match lmeta(root, rel) {
        Some(m) if m.ftype == FType::Symlink => Ok(read_link(root, rel)?.into_bytes()),
        Some(m) if m.ftype == FType::File => read_file(root, rel),
        Some(_) => Ok(Vec::new()),
        None => Ok(Vec::new()),
    }
}

fn open_ro(p: &Path) -> Result<File> {
    OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(p).with_context(|| format!("open {}", p.display()))
}

fn same_range(a: &mut File, b: &mut File, off: u64, len: u64) -> Result<bool> {
    a.seek(SeekFrom::Start(off))?;
    b.seek(SeekFrom::Start(off))?;
    let mut left = len;
    let mut ba = vec![0u8; 1 << 16];
    let mut bb = vec![0u8; 1 << 16];
    while left > 0 {
        let want = left.min(ba.len() as u64) as usize;
        let na = read_full(a, &mut ba[..want])?;
        let nb = read_full(b, &mut bb[..want])?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na < want {
            break;
        }
        left -= na as u64;
    }
    Ok(true)
}

fn read_full(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let r = f.read(&mut buf[n..])?;
        if r == 0 {
            break;
        }
        n += r;
    }
    Ok(n)
}

/// Compare two entries. With `ranges`, only those byte ranges of two regular files can differ
/// (the rest share extents), so only they are read.
pub fn same_entry(ra: &Path, pa: &str, rb: &Path, pb: &str, ranges: Option<&[(u64, u64)]>) -> Result<bool> {
    let (ma, mb) = match (lmeta(ra, pa), lmeta(rb, pb)) {
        (Some(a), Some(b)) => (a, b),
        (None, None) => return Ok(true),
        _ => return Ok(false),
    };
    if ma.ftype != mb.ftype {
        return Ok(false);
    }
    match ma.ftype {
        FType::Symlink => Ok(read_link(ra, pa)? == read_link(rb, pb)?),
        FType::Dir | FType::Special => Ok(true),
        FType::File => {
            if ma.size != mb.size {
                return Ok(false);
            }
            let mut a = open_ro(&safe_join(ra, pa)?)?;
            let mut b = open_ro(&safe_join(rb, pb)?)?;
            match ranges {
                Some(rs) => {
                    for (off, len) in rs {
                        if *off >= ma.size {
                            continue;
                        }
                        let l = (*len).min(ma.size - off);
                        if !same_range(&mut a, &mut b, *off, l)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                None => same_range(&mut a, &mut b, 0, ma.size),
            }
        }
    }
}

/// Create the parent folders of `rel` below `root`, with owner `own`.
pub fn make_parents(root: &Path, rel: &str, own: Option<(u32, u32)>) -> Result<()> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    let mut cur = root.to_path_buf();
    for c in &comps[..comps.len().saturating_sub(1)] {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => continue,
            Ok(m) if m.file_type().is_symlink() => bail!("path goes through a symbolic link: {rel}"),
            Ok(_) => {
                fs::remove_file(&cur)?;
            }
            Err(_) => {}
        }
        fs::create_dir(&cur).with_context(|| format!("mkdir {}", cur.display()))?;
        if let Some((u, g)) = own {
            chown_nofollow(&cur, u, g)?;
        }
    }
    Ok(())
}

pub fn chown_nofollow(p: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes())?;
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EPERM) || unsafe { libc::geteuid() } == 0 {
            return Err(anyhow!("chown {}: {e}", p.display()));
        }
    }
    Ok(())
}

/// Remove an entry (recursively for a folder). Nested subvolumes are deleted as subvolumes.
pub fn remove_entry(root: &Path, rel: &str) -> Result<()> {
    let p = safe_join(root, rel)?;
    let m = match fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if m.is_dir() {
        if crate::btrfs::is_subvolume(&p) {
            crate::btrfs::delete_tree(&p)
        } else {
            for nested in crate::btrfs::nested_subvolumes(&p)? {
                crate::btrfs::delete_tree(&nested)?;
            }
            fs::remove_dir_all(&p).with_context(|| format!("remove {}", p.display()))
        }
    } else {
        fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))
    }
}

/// Copy one entry from `src_root/src_rel` to `dst_root/dst_rel`: a reflink copy for a file,
/// the same target for a link, an empty folder for a folder. Replaces what is at the target.
pub fn copy_entry(src_root: &Path, src_rel: &str, dst_root: &Path, dst_rel: &str, own: Option<(u32, u32)>) -> Result<()> {
    let src = safe_join(src_root, src_rel)?;
    let m = fs::symlink_metadata(&src).with_context(|| format!("stat {}", src.display()))?;
    make_parents(dst_root, dst_rel, own)?;
    let dst = safe_join(dst_root, dst_rel)?;
    if let Ok(dm) = fs::symlink_metadata(&dst) {
        if m.file_type().is_file() && dm.file_type().is_file() {
            // Keep the inode of the target: btrfs then reports an in-place change with byte
            // ranges, and running tools keep their file handles.
            return update_in_place(&src, &dst, &m, own);
        }
        if !(m.is_dir() && dm.is_dir()) {
            remove_entry(dst_root, dst_rel)?;
        }
    }
    let ft = m.file_type();
    if ft.is_symlink() {
        let t = fs::read_link(&src)?;
        std::os::unix::fs::symlink(&t, &dst).with_context(|| format!("symlink {}", dst.display()))?;
    } else if ft.is_dir() {
        if fs::symlink_metadata(&dst).is_err() {
            fs::create_dir(&dst).with_context(|| format!("mkdir {}", dst.display()))?;
        }
        fs::set_permissions(&dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    } else if ft.is_file() {
        let s = open_ro(&src)?;
        let d = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(m.mode() & 0o7777)
            .open(&dst)
            .with_context(|| format!("create {}", dst.display()))?;
        if crate::btrfs::clone_file(&s, &d).is_err() {
            let mut s2 = open_ro(&src)?;
            let mut d2 = d.try_clone()?;
            std::io::copy(&mut s2, &mut d2)?;
        }
        d.set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777))?;
        set_mtime(&d, m.mtime(), m.mtime_nsec())?;
    } else {
        bail!("cannot copy special file {}", src.display());
    }
    if let Some((u, g)) = own {
        chown_nofollow(&dst, u, g)?;
    }
    Ok(())
}

fn update_in_place(src: &Path, dst: &Path, m: &fs::Metadata, own: Option<(u32, u32)>) -> Result<()> {
    let s = open_ro(src)?;
    let d = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dst)
        .with_context(|| format!("open {}", dst.display()))?;
    d.set_len(0)?;
    if crate::btrfs::clone_file(&s, &d).is_err() {
        let mut s2 = open_ro(src)?;
        let mut d2 = d.try_clone()?;
        std::io::copy(&mut s2, &mut d2)?;
    }
    d.set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777))?;
    set_mtime(&d, m.mtime(), m.mtime_nsec())?;
    if let Some((u, g)) = own {
        chown_nofollow(dst, u, g)?;
    }
    Ok(())
}

pub fn set_mtime(f: &File, sec: i64, nsec: i64) -> Result<()> {
    use std::os::fd::AsRawFd;
    let ts = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        libc::timespec { tv_sec: sec as libc::time_t, tv_nsec: nsec as libc::c_long },
    ];
    if unsafe { libc::futimens(f.as_raw_fd(), ts.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Write bytes as a file at `root/rel` (replacing it), with a mode and owner.
pub fn write_file(root: &Path, rel: &str, data: &[u8], mode: u32, own: Option<(u32, u32)>) -> Result<()> {
    use std::io::Write;
    make_parents(root, rel, own)?;
    let dst = safe_join(root, rel)?;
    if fs::symlink_metadata(&dst).is_ok() {
        remove_entry(root, rel)?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode)
        .open(&dst)
        .with_context(|| format!("create {}", dst.display()))?;
    f.write_all(data)?;
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    if let Some((u, g)) = own {
        chown_nofollow(&dst, u, g)?;
    }
    Ok(())
}

pub fn set_mode(root: &Path, rel: &str, mode: u32) -> Result<()> {
    let p = safe_join(root, rel)?;
    let m = fs::symlink_metadata(&p)?;
    if m.file_type().is_symlink() {
        return Ok(());
    }
    fs::set_permissions(&p, fs::Permissions::from_mode(mode & 0o7777))?;
    Ok(())
}

/// Walk all entries below `root/rel` (not following links, not entering nested subvolumes).
/// Paths are relative to `root`. Folders are reported before their content.
pub fn walk(root: &Path, rel: &str, f: &mut dyn FnMut(&str, &Meta) -> Result<bool>) -> Result<()> {
    let start = safe_join(root, rel)?;
    let dev = match fs::symlink_metadata(&start) {
        Ok(m) => m.dev(),
        Err(_) => return Ok(()),
    };
    let mut stack = vec![rel.to_string()];
    while let Some(dir) = stack.pop() {
        let p = if dir.is_empty() { root.to_path_buf() } else { root.join(&dir) };
        let rd = match fs::read_dir(&p) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        let mut subdirs = Vec::new();
        for n in names {
            let child = if dir.is_empty() { n.clone() } else { format!("{dir}/{n}") };
            let m = match fs::symlink_metadata(root.join(&child)) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let meta = meta_of(&m);
            if meta.is_dir() && m.dev() != dev {
                continue;
            }
            let descend = f(&child, &meta)?;
            if meta.is_dir() && descend {
                subdirs.push(child);
            }
        }
        for s in subdirs.into_iter().rev() {
            stack.push(s);
        }
    }
    Ok(())
}

/// Write `data` to `path` atomically: a temporary file, `fsync`, rename, `fsync` of the folder.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow!("no parent"))?;
    let tmp = dir.join(format!(".{}.tmp{}", path.file_name().unwrap().to_string_lossy(), std::process::id()));
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// The git blob id of some bytes.
pub fn git_blob_id(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", data.len()).as_bytes());
    h.update(data);
    hex::encode(h.finalize())
}

pub fn sha256_file(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Binary content by the git rule: a NUL byte in the first 8000 bytes.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

// ----- writes into folders owned by agents, through file descriptors -----

fn cstr(s: &str) -> Result<std::ffi::CString> {
    Ok(std::ffi::CString::new(s)?)
}

/// Open a folder without following a link at its last component.
pub fn open_dir_nofollow(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open folder {}", path.display()))
}

/// Read a regular file `name` of an open folder; `None` if absent, a link or not a file.
pub fn read_at(dir: &File, name: &str) -> Option<Vec<u8>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let c = cstr(name).ok()?;
    let fd = unsafe {
        libc::openat(dir.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
    };
    if fd < 0 {
        return None;
    }
    let mut f = unsafe { File::from_raw_fd(fd) };
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut v = Vec::new();
    f.read_to_end(&mut v).ok()?;
    Some(v)
}

/// Replace `name` in an open folder: write a new file (never through an existing name), then
/// rename it over `name`. A link at `name` is replaced, not followed.
pub fn replace_at(dir: &File, name: &str, content: &mut dyn FnMut(&File) -> Result<()>, own: Option<(u32, u32)>) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let tmp = format!(".{name}.layr-{}", std::process::id());
    let ct = cstr(&tmp)?;
    let cn = cstr(name)?;
    unsafe {
        libc::unlinkat(dir.as_raw_fd(), ct.as_ptr(), 0);
    }
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            ct.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        return Err(anyhow!("create {tmp}: {}", std::io::Error::last_os_error()));
    }
    let f = unsafe { File::from_raw_fd(fd) };
    let r = (|| -> Result<()> {
        content(&f)?;
        if let Some((u, g)) = own {
            if unsafe { libc::fchown(f.as_raw_fd(), u, g) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        if unsafe { libc::renameat(dir.as_raw_fd(), ct.as_ptr(), dir.as_raw_fd(), cn.as_ptr()) } != 0 {
            return Err(anyhow!("rename to {name}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    })();
    if r.is_err() {
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), ct.as_ptr(), 0);
        }
    }
    r
}

/// Remove `name` from an open folder (a link is removed, not followed).
pub fn unlink_at(dir: &File, name: &str) {
    use std::os::fd::AsRawFd;
    if let Ok(c) = cstr(name) {
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0);
        }
    }
}
