//! Thin wrappers around the btrfs ioctls that layr needs, and the `btrfs` command for streams.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};

const SNAP_CREATE_V2: libc::c_ulong = 0x5000_9417;
const SUBVOL_CREATE_V2: libc::c_ulong = 0x5000_9418;
const SNAP_DESTROY_V2: libc::c_ulong = 0x5000_943f;
const SUBVOL_GETFLAGS: libc::c_ulong = 0x8008_9419;
const SUBVOL_SETFLAGS: libc::c_ulong = 0x4008_941a;
const GET_SUBVOL_INFO: libc::c_ulong = 0x81f8_943c;
const FICLONE: libc::c_ulong = 0x4004_9409;
const FICLONERANGE: libc::c_ulong = 0x4020_940d;
const SUBVOL_RDONLY: u64 = 1 << 1;
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
const FIRST_FREE_OBJECTID: u64 = 256;

#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    sec: u64,
    nsec: u32,
}

#[repr(C)]
struct SubvolInfoArgs {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    parent_uuid: [u8; 16],
    received_uuid: [u8; 16],
    ctransid: u64,
    otransid: u64,
    stransid: u64,
    rtransid: u64,
    ctime: Timespec,
    otime: Timespec,
    stime: Timespec,
    rtime: Timespec,
    reserved: [u64; 8],
}

#[repr(C)]
struct CloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

#[derive(Debug, Clone)]
pub struct SubvolInfo {
    pub tree_id: u64,
    pub generation: u64,
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub received_uuid: Option<String>,
    pub readonly: bool,
}

fn fmt_uuid(b: &[u8; 16]) -> Option<String> {
    if b.iter().all(|x| *x == 0) {
        return None;
    }
    Some(uuid::Uuid::from_bytes(*b).hyphenated().to_string())
}

fn ioctl<T>(fd: i32, req: libc::c_ulong, arg: *mut T) -> std::io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, req as _, arg) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn split_parent(path: &Path) -> Result<(File, Vec<u8>)> {
    let parent = path.parent().ok_or_else(|| anyhow!("no parent: {}", path.display()))?;
    let name = path.file_name().ok_or_else(|| anyhow!("no name: {}", path.display()))?;
    let dir = File::open(if parent.as_os_str().is_empty() { Path::new(".") } else { parent })
        .with_context(|| format!("open {}", parent.display()))?;
    let bytes = name.as_bytes().to_vec();
    if bytes.len() >= 4040 {
        bail!("name too long: {}", path.display());
    }
    Ok((dir, bytes))
}

fn vol_args(fd: i64, flags: u64, name: &[u8]) -> Box<VolArgsV2> {
    let mut a = Box::new(VolArgsV2 { fd, transid: 0, flags, unused: [0; 4], name: [0; 4040] });
    a.name[..name.len()].copy_from_slice(name);
    a
}

/// True when `path` lives on a btrfs file system.
pub fn is_btrfs(path: &Path) -> bool {
    let c = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    st.f_type as i64 == BTRFS_SUPER_MAGIC
}

/// True when `path` is the root of a subvolume.
pub fn is_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.is_dir() && m.ino() == FIRST_FREE_OBJECTID && is_btrfs(path),
        Err(_) => false,
    }
}

pub fn create_subvolume(path: &Path) -> Result<()> {
    let (dir, name) = split_parent(path)?;
    let mut a = vol_args(0, 0, &name);
    ioctl(dir.as_raw_fd(), SUBVOL_CREATE_V2, &mut *a).with_context(|| format!("create subvolume {}", path.display()))
}

/// Snapshot `src` (a subvolume) to `dst`. The destination must not exist.
pub fn snapshot(src: &Path, dst: &Path, readonly: bool) -> Result<()> {
    let s = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let (dir, name) = split_parent(dst)?;
    let mut a = vol_args(s.as_raw_fd() as i64, if readonly { SUBVOL_RDONLY } else { 0 }, &name);
    ioctl(dir.as_raw_fd(), SNAP_CREATE_V2, &mut *a)
        .with_context(|| format!("snapshot {} to {}", src.display(), dst.display()))
}

/// Delete the subvolume at `path`. Nested subvolumes must be deleted first.
pub fn delete_subvolume(path: &Path) -> Result<()> {
    let (dir, name) = split_parent(path)?;
    let mut a = vol_args(0, 0, &name);
    ioctl(dir.as_raw_fd(), SNAP_DESTROY_V2, &mut *a).with_context(|| format!("delete subvolume {}", path.display()))
}

pub fn set_readonly(path: &Path, readonly: bool) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut flags: u64 = 0;
    ioctl(f.as_raw_fd(), SUBVOL_GETFLAGS, &mut flags)?;
    if readonly {
        flags |= SUBVOL_RDONLY
    } else {
        flags &= !SUBVOL_RDONLY
    }
    ioctl(f.as_raw_fd(), SUBVOL_SETFLAGS, &mut flags).with_context(|| format!("set flags {}", path.display()))
}

pub fn subvol_info(path: &Path) -> Result<SubvolInfo> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut a: Box<SubvolInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
    ioctl(f.as_raw_fd(), GET_SUBVOL_INFO, &mut *a).with_context(|| format!("subvolume info {}", path.display()))?;
    let mut flags: u64 = 0;
    ioctl(f.as_raw_fd(), SUBVOL_GETFLAGS, &mut flags)?;
    Ok(SubvolInfo {
        tree_id: a.treeid,
        generation: a.generation,
        uuid: fmt_uuid(&a.uuid).unwrap_or_default(),
        parent_uuid: fmt_uuid(&a.parent_uuid),
        received_uuid: fmt_uuid(&a.received_uuid),
        readonly: flags & SUBVOL_RDONLY != 0,
    })
}

/// Delete a subvolume and every subvolume nested inside it.
pub fn delete_tree(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if is_subvolume(path) {
        let info = subvol_info(path)?;
        if info.readonly {
            // Nested subvolumes of a read-only snapshot are only placeholders.
            return delete_subvolume(path);
        }
    }
    for nested in nested_subvolumes(path)? {
        delete_tree(&nested)?;
    }
    if is_subvolume(path) {
        delete_subvolume(path)
    } else {
        std::fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
    }
}

/// Subvolumes nested anywhere below `root` (not following symlinks, not entering them).
pub fn nested_subvolumes(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    let root_dev = std::fs::symlink_metadata(root)?.dev();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if !ft.is_dir() {
                continue;
            }
            let p = e.path();
            let m = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.dev() != root_dev && m.ino() == FIRST_FREE_OBJECTID {
                out.push(p);
            } else if m.dev() == root_dev {
                stack.push(p);
            }
        }
    }
    Ok(out)
}

/// Reflink the whole content of `src` into `dst` (both open files).
pub fn clone_file(src: &File, dst: &File) -> std::io::Result<()> {
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Reflink a byte range. Offsets and length must be aligned to the block size, except a range
/// that ends at the end of the source file.
pub fn clone_range(src: &File, src_off: u64, len: u64, dst: &File, dst_off: u64) -> std::io::Result<()> {
    let mut a = CloneRange { src_fd: src.as_raw_fd() as i64, src_offset: src_off, src_length: len, dest_offset: dst_off };
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONERANGE as _, &mut a) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Run `btrfs send` with no file data and return the raw stream.
pub fn send_no_data(parent: &Path, child: &Path) -> Result<Vec<u8>> {
    let out = Command::new("btrfs")
        .arg("send")
        .arg("-q")
        .arg("--no-data")
        .arg("-p")
        .arg(parent)
        .arg(child)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .context("run btrfs send")?;
    if !out.status.success() {
        bail!(
            "btrfs send --no-data -p {} {} failed: {}",
            parent.display(),
            child.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Start a full data `btrfs send` (with an optional parent) writing to stdout of the child.
pub fn send_command(parent: Option<&Path>, child: &Path) -> Command {
    let mut c = Command::new("btrfs");
    c.arg("send").arg("-q");
    if let Some(p) = parent {
        c.arg("-p").arg(p);
    }
    c.arg(child);
    c
}

/// `btrfs receive` into `dir`, reading the stream from stdin of the child.
pub fn receive_command(dir: &Path) -> Command {
    let mut c = Command::new("btrfs");
    c.arg("receive").arg("-q").arg("-e").arg(dir);
    c
}

/// Commit the current transaction of the file system that holds `path`.
pub fn sync(path: &Path) -> Result<()> {
    let f = File::open(path)?;
    if unsafe { libc::syncfs(f.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
