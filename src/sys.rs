//! The only module with `unsafe` code: system calls that `rustix` and the standard library do not
//! wrap. Every other module uses safe wrappers.
//!
//! - btrfs ioctls: create, snapshot and destroy subvolumes, read and set subvolume flags,
//!   subvolume information,
//!   and `FICLONERANGE`;
//! - `fork` (the service forks one process per request while it is single threaded);
//! - the SIGTERM action of the service (it stops after its running requests);
//! - switching a child process to a user before `exec` (groups, gid, uid);
//! - `setfsuid`/`setfsgid` (file permission checks as a user, for the current thread);
//! - user database lookups (`getpwuid_r`, `getpwnam_r`, `getgrouplist`).

use anyhow::{anyhow, Context, Result};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::process::Command;
use zerocopy::FromZeros;

const CHECK_OPEN_FILES_COUNT: u64 = 1024;
const PASSWD_INITIAL_BYTES: usize = 16 * 1024;
const PASSWD_MAX_BYTES: usize = 1024 * 1024;
const GROUPS_INITIAL_COUNT: usize = 1024;
const GROUPS_MAX_COUNT: usize = 65_536;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
const XATTR_INITIAL_VALUE_BYTES: usize = 256;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
const XATTR_INITIAL_NAMES_BYTES: usize = 512;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
const XATTR_GROWTH_SLACK_BYTES: usize = 64;
// Linux limits both individual attributes and attribute name lists to 64 KiB.
#[cfg(feature = "overlayfs")]
const XATTR_MAX_BYTES: usize = 64 * 1024;
// Bound memory for Darwin attributes, including potentially large thin-layer markers.
#[cfg(target_os = "macos")]
const XATTR_MAX_BYTES: usize = 16 * 1024 * 1024;
#[cfg(target_os = "macos")]
const ATTR_BULK_BUFFER_BYTES: usize = 256 * 1024;
#[cfg(target_os = "macos")]
const DARWIN_SUPPLEMENTARY_GROUPS_COUNT: usize = 16;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
const BTRFS_ROOT_INODE: u64 = 256;
#[cfg(target_os = "macos")]
const CLONE_ACL: u32 = 0x0004;
#[cfg(target_os = "macos")]
const ACL_TYPE_EXTENDED: libc::c_int = 0x100;

#[cfg(target_os = "linux")]
const NICE_LOWEST: libc::c_int = 19;
#[cfg(target_os = "linux")]
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
#[cfg(target_os = "linux")]
const IOPRIO_CLASS_BE: libc::c_int = 2;
#[cfg(target_os = "linux")]
const IOPRIO_BE_LOWEST: libc::c_int = 7;
#[cfg(target_os = "linux")]
const IOPRIO_CLASS_SHIFT: libc::c_int = 13;

/// Lower the calling thread's CPU and I/O priority, best effort. Call only from threads
/// dedicated to background work: on Linux these priorities apply per thread.
#[cfg(target_os = "linux")]
pub fn lower_thread_priority() {
    let _ = rustix::process::setpriority_process(None, NICE_LOWEST);
    // SAFETY: ioprio_set operates on the calling thread and has no pointer arguments.
    unsafe {
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | IOPRIO_BE_LOWEST);
    }
}

#[cfg(target_os = "macos")]
thread_local! {
    static BACKGROUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(target_os = "macos")]
pub fn lower_thread_priority() {
    BACKGROUND.set(true);
    // SAFETY: this changes only the calling thread and has no pointer arguments.
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn lower_thread_priority() {}

/// Lower priority for a process dedicated to background work and its future workers.
#[cfg(target_os = "macos")]
pub fn lower_process_priority() {
    BACKGROUND.set(true);
    // SAFETY: this changes only the calling process and has no pointer arguments.
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn lower_process_priority() {
    lower_thread_priority();
}

/// Make a child of a background thread a background process on macOS. Linux inherits priorities.
#[cfg(target_os = "macos")]
pub fn inherit_background(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    if BACKGROUND.get() {
        // SAFETY: the child calls only setpriority before exec; failure is best effort.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
                Ok(())
            });
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn inherit_background(_cmd: &mut Command) {}

#[cfg(all(test, target_os = "linux"))]
mod priority_tests {
    use super::{IOPRIO_BE_LOWEST, IOPRIO_CLASS_BE, IOPRIO_CLASS_SHIFT, IOPRIO_WHO_PROCESS, NICE_LOWEST};

    fn priority() -> (libc::c_int, libc::c_long) {
        // SAFETY: ioprio_get queries only the calling thread and takes no pointers.
        unsafe {
            (rustix::process::getpriority_process(None).unwrap(), libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0))
        }
    }

    #[test]
    fn lowers_only_the_calling_thread() {
        let before = priority();
        let child = std::thread::spawn(|| {
            super::lower_thread_priority();
            priority()
        })
        .join()
        .unwrap();
        assert_eq!(child, (NICE_LOWEST, ((IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | IOPRIO_BE_LOWEST).into()));
        assert_eq!(priority(), before);
    }
}

/// Bound automatic check processes before executing any project code.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn check_limits_before_exec(cmd: &mut Command, memory: u64, processes: u64, seconds: u64) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec only calls async-signal-safe system calls with stack values.
    unsafe {
        cmd.pre_exec(move || {
            for (resource, value) in [
                // Darwin exposes RLIMIT_AS as RLIMIT_RSS and rejects an address-space cap.
                // Keep the other hard limits there; Linux also enforces virtual memory.
                #[cfg(target_os = "linux")]
                (rustix::process::Resource::As, memory),
                (rustix::process::Resource::Nproc, processes),
                (rustix::process::Resource::Cpu, seconds),
                (rustix::process::Resource::Nofile, CHECK_OPEN_FILES_COUNT),
                (rustix::process::Resource::Core, 0),
                (rustix::process::Resource::Fsize, memory),
            ] {
                // Only root raises a hard limit: a lower hard limit of the caller stays in force.
                let now = rustix::process::getrlimit(resource);
                let value = now.maximum.map_or(value, |maximum| value.min(maximum));
                rustix::process::setrlimit(resource, rustix::process::Rlimit { current: Some(value), maximum: Some(value) })?;
            }
            #[cfg(target_os = "linux")]
            rustix::thread::set_no_new_privs(true)?;
            Ok(())
        });
    }
    Ok(())
}

// ----- btrfs ioctls -----

const SNAP_CREATE_V2: libc::c_ulong = 0x5000_9417;
const SUBVOL_CREATE_V2: libc::c_ulong = 0x5000_9418;
const SNAP_DESTROY_V2: libc::c_ulong = 0x5000_943f;
const SUBVOL_GETFLAGS: libc::c_ulong = 0x8008_9419;
const SUBVOL_SETFLAGS: libc::c_ulong = 0x4008_941a;
const GET_SUBVOL_INFO: libc::c_ulong = 0x81f8_943c;
#[cfg(target_os = "linux")]
const BTRFS_FS_INFO: libc::c_ulong = 0x8400_941f;
#[cfg(target_os = "linux")]
#[repr(C)]
struct FsInfo {
    max_id: u64,
    devices: u64,
    uuid: [u8; 16],
    rest: [u8; 992],
}
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const _: () = {
    assert!(std::mem::size_of::<FsInfo>() == 1024);
    assert!(std::mem::offset_of!(FsInfo, uuid) == 16);
};
#[cfg(target_os = "linux")]
const BTRFS_SET_RECEIVED_SUBVOL: libc::c_ulong = 0xc0c8_9425;
#[cfg(target_os = "linux")]
#[repr(C)]
struct ReceivedSubvolArgs {
    uuid: [u8; 16],
    stransid: u64,
    rtransid: u64,
    stime: Timespec,
    rtime: Timespec,
    flags: u64,
    reserved: [u64; 16],
}
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const _: () = {
    assert!(std::mem::size_of::<ReceivedSubvolArgs>() == 200);
    assert!(std::mem::offset_of!(ReceivedSubvolArgs, flags) == 64);
};
#[cfg(target_os = "linux")]
const BTRFS_SEND: libc::c_ulong = 0x4048_9426;
#[cfg(target_os = "linux")]
#[repr(C)]
struct SendArgs {
    send_fd: i64,
    clone_sources_count: u64,
    clone_sources: u64,
    parent_root: u64,
    flags: u64,
    version: u32,
    reserved: [u8; 28],
}
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const _: () = {
    assert!(std::mem::size_of::<SendArgs>() == 72);
    assert!(std::mem::offset_of!(SendArgs, version) == 40);
};
const FICLONERANGE: libc::c_ulong = 0x4020_940d;
pub const SUBVOL_RDONLY: u64 = 1 << 1;

/// The UAPI `btrfs_ioctl_search_key`, in native byte order.
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[repr(C)]
#[derive(Clone, Copy, Default, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::KnownLayout, zerocopy::Immutable)]
pub struct SearchKey {
    pub tree_id: u64,
    pub min_objectid: u64,
    pub max_objectid: u64,
    pub min_offset: u64,
    pub max_offset: u64,
    pub min_transid: u64,
    pub max_transid: u64,
    pub min_type: u32,
    pub max_type: u32,
    pub nr_items: u32,
    pub unused: u32,
    pub unused1: [u64; 4],
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
// The packed DST has no dynamic trailing padding; its backing Vec<u64> still provides UAPI alignment.
#[repr(C, packed)]
#[derive(zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::KnownLayout, zerocopy::Immutable)]
struct SearchArgs {
    key: SearchKey,
    buf_size: u64,
    buf: [u8],
}

#[cfg(all(test, target_os = "linux", not(feature = "overlayfs")))]
#[test]
fn tree_search_uapi_layout() {
    use std::mem::{offset_of, size_of};
    assert_eq!(size_of::<SearchKey>(), 104);
    assert_eq!(offset_of!(SearchKey, nr_items), 64);
    assert_eq!(offset_of!(SearchArgs, key), 0);
    assert_eq!(offset_of!(SearchArgs, buf_size), 104);
    use zerocopy::{FromBytes, IntoBytes};
    let mut storage = [0u64; 15];
    let args = SearchArgs::mut_from_bytes(storage.as_mut_bytes()).unwrap();
    assert_eq!(args.buf.as_ptr() as usize - args as *const SearchArgs as *const () as usize, 112);
}

/// Reusable, aligned storage for TREE_SEARCH_V2's flexible array argument.
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
pub struct TreeSearch {
    args: Vec<u64>,
    size: usize,
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
impl TreeSearch {
    pub fn new(size: usize) -> Self {
        const WORD: usize = std::mem::size_of::<u64>();
        let prefix = std::mem::size_of::<SearchKey>() + WORD;
        Self { args: vec![0; (prefix + size).div_ceil(WORD)], size }
    }

    pub fn run(&mut self, fd: BorrowedFd<'_>, key: &mut SearchKey) -> io::Result<(usize, &[u8])> {
        use zerocopy::{FromBytes, IntoBytes};
        const TREE_SEARCH_V2: libc::c_ulong = 0xc070_9411;
        const WORD: usize = std::mem::size_of::<u64>();
        let prefix = std::mem::size_of::<SearchKey>() + WORD;
        loop {
            let bytes = &mut self.args.as_mut_bytes()[..prefix + self.size];
            let args = SearchArgs::mut_from_bytes(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            args.key = *key;
            args.buf_size = self.size as u64;
            // SAFETY: args is aligned and initialized, and its flexible buffer has buf_size bytes.
            let rc = unsafe { libc::ioctl(fd.as_raw_fd(), TREE_SEARCH_V2 as _, args as *mut SearchArgs as *mut libc::c_void) };
            if rc < 0 {
                let error = io::Error::last_os_error();
                let required = usize::try_from(args.buf_size).unwrap_or(0);
                if error.raw_os_error() != Some(libc::EOVERFLOW) || required <= self.size {
                    return Err(error);
                }
                let len = prefix.checked_add(required).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
                self.args.resize(len.div_ceil(WORD), 0);
                self.size = required;
                continue;
            }
            *key = args.key;
            break;
        }
        Ok((key.nr_items as usize, &self.args.as_bytes()[prefix..prefix + self.size]))
    }
}

#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

#[repr(C)]
#[derive(Clone, Copy, FromZeros)]
struct Timespec {
    sec: u64,
    nsec: u32,
}

#[repr(C)]
#[derive(FromZeros)]
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

fn vol_args(fd: i64, flags: u64, name: &str) -> Result<Box<VolArgsV2>> {
    let b = name.as_bytes();
    if b.is_empty() || b.len() >= 4040 || b.contains(&0) || b.contains(&b'/') {
        return Err(anyhow!("bad subvolume name {name:?}"));
    }
    let mut a = Box::new(VolArgsV2 { fd, transid: 0, flags, unused: [0; 4], name: [0; 4040] });
    a.name[..b.len()].copy_from_slice(b);
    Ok(a)
}

fn ioctl_ptr<T>(fd: BorrowedFd, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: fd stays open and arg has the size and alignment required by the request.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg as *mut T) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// True when the folder `path`, or the nearest folder above it that exists, ignores case
/// (ext4 `casefold`, set with `chattr +F`). New folders take this flag from their parent.
#[cfg(all(target_os = "linux", feature = "overlayfs"))]
pub fn casefold(path: &std::path::Path) -> bool {
    use rustix::fs::{open, Mode, OFlags};
    const FS_CASEFOLD_FL: libc::c_long = 0x4000_0000;
    let Some(dir) = path.ancestors().find(|p| p.is_dir()) else {
        return false;
    };
    let Ok(fd) = open(dir, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()) else {
        return false;
    };
    let mut flags: libc::c_long = 0;
    // File systems without inode flags, such as some network ones, answer with an error: no casefold.
    ioctl_ptr(fd.as_fd(), libc::FS_IOC_GETFLAGS as _, &mut flags).is_ok() && flags & FS_CASEFOLD_FL != 0
}

/// Create the subvolume `name` in the folder `dir`.
pub fn subvol_create(dir: BorrowedFd, name: &str) -> Result<()> {
    let mut a = vol_args(0, 0, name)?;
    ioctl_ptr(dir, SUBVOL_CREATE_V2, &mut *a).with_context(|| format!("create subvolume {name}"))
}

/// Snapshot the subvolume `src` as `name` in the folder `dir`.
pub fn subvol_snapshot(src: BorrowedFd, dir: BorrowedFd, name: &str, readonly: bool) -> Result<()> {
    let mut a = vol_args(src.as_raw_fd() as i64, if readonly { SUBVOL_RDONLY } else { 0 }, name)?;
    ioctl_ptr(dir, SNAP_CREATE_V2, &mut *a).with_context(|| format!("snapshot to {name}"))
}

/// Destroy the subvolume `name` in the folder `dir`.
pub fn subvol_destroy(dir: BorrowedFd, name: &str) -> Result<()> {
    let mut a = vol_args(0, 0, name)?;
    ioctl_ptr(dir, SNAP_DESTROY_V2, &mut *a).with_context(|| format!("delete subvolume {name}"))
}

pub fn subvol_flags(fd: BorrowedFd) -> io::Result<u64> {
    let mut flags: u64 = 0;
    ioctl_ptr(fd, SUBVOL_GETFLAGS, &mut flags)?;
    Ok(flags)
}

#[cfg(target_os = "linux")]
pub fn filesystem_uuid(fd: BorrowedFd) -> io::Result<[u8; 16]> {
    let mut info = FsInfo { max_id: 0, devices: 0, uuid: [0; 16], rest: [0; 992] };
    ioctl_ptr(fd, BTRFS_FS_INFO, &mut info)?;
    Ok(info.uuid)
}

#[cfg(target_os = "linux")]
/// Set the flags of a subvolume (`SUBVOL_RDONLY` makes it read-only).
pub fn subvol_set_flags(fd: BorrowedFd, flags: u64) -> io::Result<()> {
    let mut f: u64 = flags;
    ioctl_ptr(fd, SUBVOL_SETFLAGS, &mut f)
}

/// Set the identity of a verified file restore, as btrfs receive does before sealing it.
#[cfg(target_os = "linux")]
pub fn subvol_set_received(fd: BorrowedFd, uuid: [u8; 16], transid: u64) -> io::Result<()> {
    let zero_time = Timespec { sec: 0, nsec: 0 };
    let mut args = ReceivedSubvolArgs {
        uuid,
        stransid: transid,
        rtransid: 0,
        stime: zero_time,
        rtime: zero_time,
        flags: 0,
        reserved: [0; 16],
    };
    ioctl_ptr(fd, BTRFS_SET_RECEIVED_SUBVOL, &mut args)
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
const INO_LOOKUP: libc::c_ulong = 0xd000_9412;

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[repr(C)]
struct InoLookupArgs {
    treeid: u64,
    objectid: u64,
    name: [u8; 4080],
}

/// The id of the subvolume that holds `fd`, for a folder of any depth (`btrfs inspect-internal
/// rootid`). The top-level subvolume has id 5.
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
pub fn subvol_id(fd: BorrowedFd) -> io::Result<u64> {
    // Object 256 is the first free inode: the root folder of the subvolume.
    let mut a = Box::new(InoLookupArgs { treeid: 0, objectid: BTRFS_ROOT_INODE, name: [0; 4080] });
    ioctl_ptr(fd, INO_LOOKUP, &mut *a)?;
    Ok(a.treeid)
}

pub struct RawSubvolInfo {
    pub tree_id: u64,
    pub generation: u64,
    pub uuid: [u8; 16],
    pub parent_uuid: [u8; 16],
    pub received_uuid: [u8; 16],
    pub ctransid: u64,
    pub stransid: u64,
}

pub fn subvol_info(fd: BorrowedFd) -> io::Result<RawSubvolInfo> {
    let mut a = Box::new(SubvolInfoArgs::new_zeroed());
    ioctl_ptr(fd, GET_SUBVOL_INFO, &mut *a)?;
    Ok(RawSubvolInfo {
        tree_id: a.treeid,
        generation: a.generation,
        uuid: a.uuid,
        parent_uuid: a.parent_uuid,
        received_uuid: a.received_uuid,
        ctransid: a.ctransid,
        stransid: a.stransid,
    })
}

/// Produce a version 1 metadata stream through an open pipe.
#[cfg(target_os = "linux")]
pub fn send_no_data(src: BorrowedFd, out: BorrowedFd, parent_root: u64) -> io::Result<()> {
    const BTRFS_SEND_NO_FILE_DATA: u64 = 1;
    // Version zero selects the original protocol.
    let mut args = SendArgs {
        send_fd: out.as_raw_fd() as i64,
        clone_sources_count: 0,
        clone_sources: 0,
        parent_root,
        flags: BTRFS_SEND_NO_FILE_DATA,
        version: 0,
        reserved: [0; 28],
    };
    ioctl_ptr(src, BTRFS_SEND, &mut args)
}

/// Reflink a byte range from `src` into `dst`.
pub fn clone_range(src: &impl AsFd, src_off: u64, len: u64, dst: &impl AsFd, dst_off: u64) -> io::Result<()> {
    let mut a = CloneRange { src_fd: src.as_fd().as_raw_fd() as i64, src_offset: src_off, src_length: len, dest_offset: dst_off };
    ioctl_ptr(dst.as_fd(), FICLONERANGE, &mut a)
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
const FIDEDUPERANGE: libc::c_ulong = 0xc018_9436;
#[cfg_attr(target_os = "macos", allow(dead_code))]
const FS_IOC_FIEMAP: libc::c_ulong = 0xc020_660b;
/// Extent flags that give no stable device offset: unknown, delayed allocation, inline data.
#[cfg_attr(target_os = "macos", allow(dead_code))]
const FIEMAP_UNSTABLE: u32 = FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_DATA_INLINE;
#[cfg_attr(target_os = "macos", allow(dead_code))]
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
#[cfg_attr(target_os = "macos", allow(dead_code))]
const FIEMAP_EXTENT_DELALLOC: u32 = 0x4;
#[cfg_attr(target_os = "macos", allow(dead_code))]
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x200;

/// `struct file_dedupe_range` with one `struct file_dedupe_range_info`.
#[repr(C)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
struct DedupeArgs {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved3: u32,
}

#[repr(C)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extent: FiemapExtent,
}

/// Share a byte range of `src` with `dst` when the kernel finds the bytes equal (FIDEDUPERANGE).
/// The kernel compares inside, so a write to either file during the call cannot lose data.
/// Returns the bytes shared: 0 when the ranges differ. `dst` is open for writing.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn dedupe_range(src: &impl AsFd, src_off: u64, len: u64, dst: &impl AsFd, dst_off: u64) -> io::Result<u64> {
    let mut a = DedupeArgs {
        src_offset: src_off,
        src_length: len,
        dest_count: 1,
        reserved1: 0,
        reserved2: 0,
        dest_fd: dst.as_fd().as_raw_fd() as i64,
        dest_offset: dst_off,
        bytes_deduped: 0,
        status: 0,
        reserved3: 0,
    };
    ioctl_ptr(src.as_fd(), FIDEDUPERANGE, &mut a)?;
    match a.status {
        0 => Ok(a.bytes_deduped),
        e if e < 0 => Err(io::Error::from_raw_os_error(-e)),
        _ => Ok(0),
    }
}

/// The device offset of the byte at `off`, or `None` when the file system gives none (a hole,
/// delayed allocation, inline data, or no FIEMAP). Equal offsets in two files mean one extent.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn physical_at(f: &impl AsFd, off: u64) -> Option<u64> {
    let mut m = Fiemap {
        start: off,
        length: 1,
        flags: 0,
        mapped_extents: 0,
        extent_count: 1,
        reserved: 0,
        extent: FiemapExtent { logical: 0, physical: 0, length: 0, reserved64: [0; 2], flags: 0, reserved: [0; 3] },
    };
    ioctl_ptr(f.as_fd(), FS_IOC_FIEMAP, &mut m).ok()?;
    let e = &m.extent;
    if m.mapped_extents == 0 || e.flags & FIEMAP_UNSTABLE != 0 || off < e.logical {
        return None;
    }
    Some(e.physical + (off - e.logical))
}

// ----- processes -----

pub enum Fork {
    Child,
    Parent(i32),
}

/// `fork`. Only call it while the process has one thread (the service loop).
// Scoped pools must have returned before any fork; the service parent stays single-threaded.
pub fn fork() -> io::Result<Fork> {
    // SAFETY: the service forks only from its single-threaded main loop, allowing normal Rust in the child.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            crate::pools::mark_worker();
            // A request process stops on SIGTERM; only the service waits for its requests.
            set_terminate_action(libc::SIG_DFL)?;
            Ok(Fork::Child)
        }
        pid => Ok(Fork::Parent(pid)),
    }
}

/// Set when the process got SIGTERM after `catch_terminate`.
static TERMINATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_terminate(_: libc::c_int) {
    TERMINATE.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Catch SIGTERM: from then on `terminate_requested` tells whether it came. A `fork` child has the default action.
pub fn catch_terminate() -> io::Result<()> {
    set_terminate_action(on_terminate as extern "C" fn(libc::c_int) as libc::sighandler_t)
}

/// Whether SIGTERM came after `catch_terminate`.
pub fn terminate_requested() -> bool {
    TERMINATE.load(std::sync::atomic::Ordering::SeqCst)
}

fn set_terminate_action(handler: libc::sighandler_t) -> io::Result<()> {
    // SAFETY: sigaction is initialized with an empty mask and a handler that only stores to an atomic.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Make the child of `cmd` drop to a user before `exec`: its groups, then gid, then uid.
#[cfg(target_os = "linux")]
pub fn switch_user_before_exec(cmd: &mut Command, uid: u32, gid: u32, groups: Vec<u32>) {
    use std::os::unix::process::CommandExt;
    let gids: Vec<rustix::process::Gid> = groups.iter().map(|g| rustix::process::Gid::from_raw(*g)).collect();
    // SAFETY: pre_exec uses only nonallocating credential syscalls and values moved in before fork.
    unsafe {
        cmd.pre_exec(move || {
            rustix::thread::set_thread_groups(&gids)?;
            rustix::thread::set_thread_gid(rustix::process::Gid::from_raw(gid))?;
            rustix::thread::set_thread_uid(rustix::process::Uid::from_raw(uid))?;
            if rustix::process::geteuid().as_raw() != uid {
                return Err(io::Error::other("uid did not change"));
            }
            Ok(())
        });
    }
}

/// Set the file system uid and gid of the calling thread; returns the previous ids.
#[cfg(target_os = "linux")]
pub fn set_fs_ids(uid: u32, gid: u32) -> Result<(u32, u32)> {
    // SAFETY: setfsgid/setfsuid take no pointers; repeating the call verifies the current ID.
    unsafe {
        let old_gid = libc::setfsgid(gid) as u32;
        if libc::setfsgid(gid) as u32 != gid {
            return Err(anyhow!("cannot set the file system gid to {gid}"));
        }
        let old_uid = libc::setfsuid(uid) as u32;
        if libc::setfsuid(uid) as u32 != uid {
            libc::setfsgid(old_gid);
            return Err(anyhow!("cannot set the file system uid to {uid}"));
        }
        Ok((old_uid, old_gid))
    }
}

/// Set the supplementary groups of the process (the service child is single threaded).
#[cfg(target_os = "linux")]
pub fn set_groups(groups: &[u32]) -> io::Result<()> {
    // The service calls this only in its single-threaded request child.
    let groups: Vec<_> = groups.iter().copied().map(rustix::process::Gid::from_raw).collect();
    Ok(rustix::thread::set_thread_groups(&groups)?)
}

#[cfg(target_os = "macos")]
pub fn set_groups(groups: &[u32]) -> io::Result<()> {
    // Darwin stores at most NGROUPS_MAX (16) supplementary groups. Its directory
    // service can return more memberships than the kernel credential can contain.
    #[cfg(target_os = "macos")]
    let groups = &groups[..groups.len().min(DARWIN_SUPPLEMENTARY_GROUPS_COUNT)];
    // SAFETY: the pointer and length describe a valid slice of gid_t for the call.
    let rc = unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn groups() -> io::Result<Vec<u32>> {
    Ok(rustix::process::getgroups()?.into_iter().map(|gid| gid.as_raw()).collect())
}

// ----- the user database -----

pub struct UserInfo {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: String,
}

fn resize_lookup_buffer<T: Copy + Default>(buffer: &mut Vec<T>, length: usize, maximum: usize, what: &str) -> io::Result<()> {
    if length > maximum {
        return Err(io::Error::other(format!("{what} exceeds the limit of {maximum}")));
    }
    if length > buffer.len() {
        buffer
            .try_reserve(length - buffer.len())
            .map_err(|error| io::Error::other(format!("cannot allocate {what}: {error}")))?;
    }
    buffer.resize(length, T::default());
    Ok(())
}

fn passwd(by_uid: Option<u32>, by_name: Option<&str>) -> io::Result<Option<UserInfo>> {
    passwd_with_buffer(by_uid, by_name, PASSWD_INITIAL_BYTES)
}

fn passwd_with_buffer(by_uid: Option<u32>, by_name: Option<&str>, initial_bytes: usize) -> io::Result<Option<UserInfo>> {
    let name = by_name.map(CString::new).transpose()?;
    let mut buf: Vec<u8> = Vec::new();
    resize_lookup_buffer(&mut buf, initial_bytes, PASSWD_MAX_BYTES, "passwd buffer bytes")?;
    loop {
        // SAFETY: NSS writes pw/result and bounded buf; strings are copied before buf is resized or dropped.
        unsafe {
            let mut pw = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut result = std::ptr::null_mut();
            let rc = match (by_uid, name.as_ref()) {
                (Some(uid), _) => libc::getpwuid_r(uid, pw.as_mut_ptr(), buf.as_mut_ptr().cast(), buf.len(), &mut result),
                (None, Some(name)) => {
                    libc::getpwnam_r(name.as_ptr(), pw.as_mut_ptr(), buf.as_mut_ptr().cast(), buf.len(), &mut result)
                }
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing passwd lookup key")),
            };
            if rc == libc::ERANGE {
                if buf.len() == PASSWD_MAX_BYTES {
                    return Err(io::Error::other(format!("passwd buffer bytes exceed the limit of {PASSWD_MAX_BYTES}")));
                }
                let next = buf.len().max(1).saturating_mul(2).min(PASSWD_MAX_BYTES);
                resize_lookup_buffer(&mut buf, next, PASSWD_MAX_BYTES, "passwd buffer bytes")?;
                continue;
            }
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            if result.is_null() {
                return Ok(None);
            }
            let pw = pw.assume_init();
            return Ok(Some(UserInfo {
                uid: pw.pw_uid,
                gid: pw.pw_gid,
                name: CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned(),
                home: CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned(),
            }));
        }
    }
}

/// Best-effort display/policy lookup; use try_user_by_uid for credential changes.
pub fn user_by_uid(uid: u32) -> Option<UserInfo> {
    try_user_by_uid(uid).ok().flatten()
}

/// Look up credentials without treating an NSS failure as a missing user.
pub fn try_user_by_uid(uid: u32) -> io::Result<Option<UserInfo>> {
    passwd(Some(uid), None)
}

/// Best-effort name lookup for display and policy resolution.
pub fn user_by_name(name: &str) -> Option<UserInfo> {
    passwd(None, Some(name)).ok().flatten()
}

/// The groups of a user (its primary group included).
pub fn user_groups(name: &str, gid: u32) -> io::Result<Vec<u32>> {
    let c = CString::new(name)?;
    lookup_groups(|groups, count| {
        // SAFETY: the capacity bounds writes; Darwin uses signed int pointers for the same 32-bit GID ABI.
        unsafe { libc::getgrouplist(c.as_ptr(), gid as _, groups.as_mut_ptr().cast(), count) }
    })
}

#[derive(Clone, Copy)]
enum GroupCount {
    RequiredTotal,
    FilledCapacity,
}

fn group_buffer_growth(capacity: usize, reported: usize, semantics: GroupCount) -> io::Result<usize> {
    if reported > GROUPS_MAX_COUNT || capacity == GROUPS_MAX_COUNT {
        return Err(io::Error::other(format!("supplementary group count exceeds the limit of {GROUPS_MAX_COUNT}")));
    }
    match semantics {
        // Darwin can stop at the filled capacity without counting the remaining memberships.
        GroupCount::FilledCapacity if reported >= capacity => Ok(reported.max(capacity.saturating_mul(2)).min(GROUPS_MAX_COUNT)),
        // glibc reports the complete required count on insufficient space.
        GroupCount::RequiredTotal if reported > capacity => Ok(reported),
        _ => Err(io::Error::other("NSS group lookup failed without a larger required count")),
    }
}

fn lookup_groups(lookup: impl FnMut(&mut [libc::gid_t], &mut libc::c_int) -> libc::c_int) -> io::Result<Vec<u32>> {
    let semantics = if cfg!(target_os = "macos") { GroupCount::FilledCapacity } else { GroupCount::RequiredTotal };
    lookup_groups_with_count(semantics, lookup)
}

fn lookup_groups_with_count(
    semantics: GroupCount,
    mut lookup: impl FnMut(&mut [libc::gid_t], &mut libc::c_int) -> libc::c_int,
) -> io::Result<Vec<u32>> {
    let mut groups = Vec::new();
    resize_lookup_buffer(&mut groups, GROUPS_INITIAL_COUNT, GROUPS_MAX_COUNT, "supplementary group count")?;
    loop {
        let mut count = groups.len() as libc::c_int;
        let rc = lookup(&mut groups, &mut count);
        let required = usize::try_from(count).map_err(|_| io::Error::other("NSS returned a negative group count"))?;
        if rc >= 0 {
            if required > groups.len() {
                return Err(io::Error::other("NSS returned an invalid group count on success"));
            }
            groups.truncate(required);
            return Ok(groups);
        }
        let next = group_buffer_growth(groups.len(), required, semantics)?;
        resize_lookup_buffer(&mut groups, next, GROUPS_MAX_COUNT, "supplementary group count")?;
    }
}

#[cfg(test)]
mod nss_tests {
    use super::*;

    #[test]
    fn passwd_grows_after_erange() {
        let uid = rustix::process::getuid().as_raw();
        let expected = user_by_uid(uid).expect("current user in NSS");
        let actual = passwd_with_buffer(Some(uid), None, 1).expect("retry small NSS buffer").expect("current user in NSS");
        assert_eq!(
            (actual.uid, actual.gid, actual.name, actual.home),
            (expected.uid, expected.gid, expected.name, expected.home)
        );
    }

    #[test]
    fn passwd_buffer_limit_is_an_explicit_error() {
        let error = passwd_with_buffer(Some(0), None, PASSWD_MAX_BYTES + 1).err().expect("reject oversized buffer");
        assert!(error.to_string().contains("passwd buffer bytes exceeds the limit"));
    }

    #[test]
    fn group_lookup_rejects_excessive_counts_and_provider_errors() {
        for count in [libc::c_int::MAX, -1, GROUPS_INITIAL_COUNT as libc::c_int] {
            let error = lookup_groups(|_, needed| {
                *needed = count;
                -1
            })
            .unwrap_err();
            assert!(!error.to_string().is_empty());
        }
        assert!(user_groups("invalid\0name", 7).is_err());
        assert!(lookup_groups(|groups, count| {
            *count = groups.len() as libc::c_int + 1;
            0
        })
        .is_err());
    }

    #[test]
    fn darwin_filled_capacity_response_grows_and_preserves_memberships() {
        let required = GROUPS_INITIAL_COUNT + 1;
        let mut calls = 0;
        let groups = lookup_groups_with_count(GroupCount::FilledCapacity, |groups, count| {
            calls += 1;
            if groups.len() < required {
                *count = groups.len() as libc::c_int;
                return -1;
            }
            groups[..required].fill(9);
            groups[0] = 7;
            *count = required as libc::c_int;
            0
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(groups.len(), required);
        assert_eq!(groups[0], 7);
        assert!(groups[1..].iter().all(|gid| *gid == 9));
        assert_eq!(group_buffer_growth(1024, 1024, GroupCount::FilledCapacity).unwrap(), 2048);
        assert!(group_buffer_growth(1024, 1024, GroupCount::RequiredTotal).is_err());
        assert!(group_buffer_growth(GROUPS_MAX_COUNT, GROUPS_MAX_COUNT, GroupCount::FilledCapacity).is_err());
        assert!(group_buffer_growth(1024, 0, GroupCount::FilledCapacity).is_err());
    }

    #[test]
    fn group_lookup_retries_required_count() {
        let required = GROUPS_INITIAL_COUNT + 1;
        let mut calls = 0;
        let groups = lookup_groups(|groups, count| {
            calls += 1;
            *count = required as libc::c_int;
            if groups.len() < required {
                return -1;
            }
            groups.fill(9);
            groups[0] = 7;
            required as libc::c_int
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(groups.len(), required);
        assert_eq!(groups[0], 7);
        assert!(groups[1..].iter().all(|gid| *gid == 9));
    }
}

#[cfg(target_os = "macos")]
pub fn switch_user_before_exec(cmd: &mut Command, uid: u32, gid: u32, mut groups: Vec<u32>) {
    use std::os::unix::process::CommandExt;
    // Match Darwin's fixed kernel group limit, as in set_groups.
    groups.truncate(DARWIN_SUPPLEMENTARY_GROUPS_COUNT);
    // SAFETY: the child calls only async-signal-safe credential syscalls before exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            if rustix::process::geteuid().as_raw() != uid {
                return Err(io::Error::other("uid did not change"));
            }
            Ok(())
        });
    }
}
#[cfg(target_os = "macos")]
pub fn set_fs_ids(uid: u32, gid: u32) -> Result<(u32, u32)> {
    // The daemon forks a single-threaded command child. macOS has no setfsuid;
    // temporary effective IDs restrict all its file access, with saved root retained.
    // SAFETY: these syscalls have no pointer arguments. Restore root before changing gid.
    unsafe {
        let old = (rustix::process::geteuid().as_raw(), rustix::process::getegid().as_raw());
        if uid == 0 && libc::seteuid(0) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if libc::setegid(gid) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if libc::seteuid(uid) != 0 {
            let error = io::Error::last_os_error();
            libc::setegid(old.1);
            return Err(error.into());
        }
        Ok(old)
    }
}

#[cfg(target_os = "macos")]
fn cpath(path: &std::path::Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}
/// The name of the file system at `path`, as `layr volume list` shows it (`apfs`, `btrfs`,
/// `ext4`, `xfs`). Other file systems show their type number.
#[cfg(target_os = "macos")]
pub fn fs_type_name(path: &std::path::Path) -> io::Result<String> {
    let stat = rustix::fs::statfs(path)?;
    let name: Vec<u8> = stat.f_fstypename.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// The name of the file system at `path`, as `layr volume list` shows it (`apfs`, `btrfs`,
/// `ext4`, `xfs`). Other file systems show their type number.
#[cfg(target_os = "linux")]
pub fn fs_type_name(path: &std::path::Path) -> io::Result<String> {
    let st = rustix::fs::statfs(path)?;
    let magic = st.f_type as i64;
    const BTRFS_SUPER_MAGIC: i64 = libc::BTRFS_SUPER_MAGIC as i64;
    const EXT4_SUPER_MAGIC: i64 = libc::EXT4_SUPER_MAGIC as i64;
    const XFS_SUPER_MAGIC: i64 = libc::XFS_SUPER_MAGIC as i64;
    const OVERLAYFS_SUPER_MAGIC: i64 = libc::OVERLAYFS_SUPER_MAGIC as i64;
    const TMPFS_MAGIC: i64 = libc::TMPFS_MAGIC as i64;
    Ok(match magic {
        BTRFS_SUPER_MAGIC => "btrfs".into(),
        EXT4_SUPER_MAGIC => "ext4".into(),
        XFS_SUPER_MAGIC => "xfs".into(),
        OVERLAYFS_SUPER_MAGIC => "overlayfs".into(),
        TMPFS_MAGIC => "tmpfs".into(),
        other => format!("fs-type-{other:#x}"),
    })
}

#[cfg(target_os = "macos")]
pub fn apfs_supported(path: &std::path::Path) -> bool {
    fs_type_name(path).is_ok_and(|name| name == "apfs")
}

/// The name of the entry at `path` as the volume stores it. On a volume that ignores case,
/// `readme` can find an entry stored as `README`. `None` when nothing is there.
#[cfg(target_os = "macos")]
pub fn stored_name(path: &std::path::Path) -> Option<std::ffi::OsString> {
    name_attribute(path, libc::ATTR_CMN_NAME)
}

/// The full path of the entry at `path` with the names as the volume stores them.
#[cfg(target_os = "macos")]
pub fn stored_path(path: &std::path::Path) -> Option<std::ffi::OsString> {
    name_attribute(path, libc::ATTR_CMN_FULLPATH)
}

/// One name attribute (`ATTR_CMN_NAME` or `ATTR_CMN_FULLPATH`) of the entry at `path`.
#[cfg(target_os = "macos")]
fn name_attribute(path: &std::path::Path, attr: libc::attrgroup_t) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    // Paths below a deep store can be longer than PATH_MAX. Status asks for each file: one
    // buffer per thread instead of an allocation per call.
    thread_local!(static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0u8; 4 + 8 + 64 * 1024]));
    let c = cpath(path).ok()?;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: attr,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    BUF.with_borrow_mut(|buf| {
        // SAFETY: attrs and buf are valid for their sizes; the path is NUL-terminated.
        if unsafe {
            libc::getattrlist(
                c.as_ptr(),
                (&mut attrs as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::FSOPT_NOFOLLOW,
            )
        } != 0
        {
            return None;
        }
        let off = 4 + i32::from_ne_bytes(buf[4..8].try_into().ok()?) as usize;
        let len = u32::from_ne_bytes(buf[8..12].try_into().ok()?) as usize;
        let raw = buf.get(off..off + len)?;
        Some(std::ffi::OsStr::from_bytes(raw.strip_suffix(&[0]).unwrap_or(raw)).to_os_string())
    })
}

/// `case_sensitive` for a device, remembered per process.
#[cfg(target_os = "macos")]
pub fn case_sensitive_dev(dev: u64, path: &std::path::Path) -> bool {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<std::collections::HashMap<u64, bool>>> = OnceLock::new();
    let map = SEEN.get_or_init(Default::default);
    if let Some(v) = map.lock().unwrap().get(&dev) {
        return *v;
    }
    let v = case_sensitive(path);
    map.lock().unwrap().insert(dev, v);
    v
}

/// True when names that differ only in case are different entries on the volume of `path`.
/// A path that cannot be checked counts as case-sensitive: Layr then refuses nothing and keeps
/// its earlier behavior.
#[cfg(target_os = "macos")]
pub fn case_sensitive(path: &std::path::Path) -> bool {
    // The nearest folder that exists answers for a path that is not made yet.
    let mut at = Some(path);
    while let Some(p) = at {
        if p.exists() {
            let Ok(c) = cpath(p) else {
                return true;
            };
            // SAFETY: the path is NUL-terminated.
            return unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) } != 0;
        }
        at = p.parent();
    }
    true
}
#[cfg(target_os = "macos")]
pub fn apfs_clone(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    use rustix::fs::{open, openat, Mode, OFlags};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;
    fn open_parent(path: &std::path::Path) -> io::Result<std::os::fd::OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut dir = open(if path.is_absolute() { "/" } else { "." }, flags, Mode::empty())?;
        for part in path.parent().ok_or_else(|| io::Error::other("missing clone parent"))?.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => dir = openat(&dir, name, flags, Mode::empty())?,
                _ => return Err(io::Error::other("invalid clone path component")),
            }
        }
        Ok(dir)
    }
    let source_dir = open_parent(src)?;
    let source = openat(
        &source_dir,
        src.file_name().ok_or_else(|| io::Error::other("missing clone source name"))?,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let destination = open_parent(dst)?;
    let name = CString::new(dst.file_name().ok_or_else(|| io::Error::other("missing clone destination name"))?.as_bytes())?;
    rustix::fs::fclonefileat(
        &source,
        &destination,
        &name,
        rustix::fs::CloneFlags::NOOWNERCOPY | rustix::fs::CloneFlags::from_bits_retain(CLONE_ACL),
    )?;
    Ok(())
}
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApfsDataIdentity {
    pub generation: Option<u32>,
    pub clone_id: Option<u64>,
}

/// Kernel data identity, not a timestamp or a user-writable extended attribute.
#[cfg(target_os = "macos")]
pub fn apfs_data_identity(path: &std::path::Path) -> io::Result<ApfsDataIdentity> {
    let path = cpath(path)?;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_GEN_COUNT,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: libc::ATTR_CMNEXT_CLONEID,
    };
    // getattrlist packs all fields on four-byte boundaries, including u64.
    // PACK_INVAL_ATTRS keeps the layout fixed when an attribute is unsupported.
    let mut buf = [0u8; 36];
    let flags = libc::FSOPT_NOFOLLOW | libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED;
    // SAFETY: attrs and buf are valid for their declared sizes; path is NUL-terminated.
    if unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&mut attrs as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            flags as _,
        )
    } != 0
    {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENOTSUP | libc::EINVAL) => Ok(ApfsDataIdentity::default()),
            _ => Err(e),
        };
    }
    let word = |off| u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
    if word(0) != buf.len() as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid APFS data identity length"));
    }
    let generation = word(24);
    let clone_id = u64::from_ne_bytes(buf[28..36].try_into().unwrap());
    Ok(ApfsDataIdentity {
        // Apple documents zero for an invalid count, including mapped files.
        generation: (word(4) & libc::ATTR_CMN_GEN_COUNT != 0 && generation != 0).then_some(generation),
        clone_id: (word(20) & libc::ATTR_CMNEXT_CLONEID != 0 && clone_id != 0).then_some(clone_id),
    })
}

/// Write the dirty data of a file to the device with `fsync`. On macOS `File::sync_data` is
/// `F_FULLFSYNC`, which also flushes the drive cache of the whole device: about 0.5 ms on an
/// idle Mac and much more while other programs write. Physical addresses need only the data
/// on the device.
#[cfg(target_os = "macos")]
pub fn fsync_data(f: &std::fs::File) -> io::Result<()> {
    Ok(rustix::fs::fsync(f)?)
}

/// The physical extents of a file: (file offset, length, device offset). `None` when APFS does
/// not report a mapping for some range (a hole, or data not yet allocated).
#[cfg(target_os = "macos")]
pub fn apfs_extents(f: &std::fs::File, len: u64) -> Option<Vec<(u64, u64, u64)>> {
    use std::os::fd::AsRawFd;
    // The kernel structure is packed to four bytes.
    #[repr(C, packed(4))]
    struct Log2Phys {
        flags: u32,
        contig: i64,
        offset: i64,
    }
    let mut out = Vec::new();
    let mut pos = 0u64;
    while pos < len {
        let mut l = Log2Phys { flags: 0, contig: (len - pos) as i64, offset: pos as i64 };
        // SAFETY: the descriptor is live and `l` is the in/out structure of F_LOG2PHYS_EXT.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut l as *mut Log2Phys) } != 0 {
            return None;
        }
        let (contig, dev) = (l.contig, l.offset);
        if contig <= 0 || dev < 0 {
            return None;
        }
        let n = (contig as u64).min(len - pos);
        out.push((pos, n, dev as u64));
        pos += n;
    }
    Some(out)
}

/// One entry of a folder as `getattrlistbulk` returns it.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkEntry {
    pub name: std::ffi::OsString,
    /// `st_mode`: the type bits and the permission bits.
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
    pub birth: (i64, i64),
    /// `st_flags`: the immutable flag of a sealed entry of an older store (`apfs_seal_entry`).
    pub flags: u32,
    pub data: ApfsDataIdentity,
}

/// Read the metadata of all entries of an open folder with `getattrlistbulk`: one call returns
/// many entries, with their change counters and clone IDs. The order is the folder order.
#[cfg(target_os = "macos")]
pub fn apfs_bulk_read(dir: std::os::fd::BorrowedFd<'_>, buf: &mut Vec<u8>) -> io::Result<Vec<BulkEntry>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_CRTIME
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_CHGTIME
            | libc::ATTR_CMN_OWNERID
            | libc::ATTR_CMN_GRPID
            | libc::ATTR_CMN_ACCESSMASK
            | libc::ATTR_CMN_FLAGS
            | libc::ATTR_CMN_GEN_COUNT
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: libc::ATTR_DIR_LINKCOUNT | libc::ATTR_DIR_DATALENGTH,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH,
        forkattr: libc::ATTR_CMNEXT_CLONEID,
    };
    let flags = libc::FSOPT_NOFOLLOW | libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED;
    if buf.len() < ATTR_BULK_BUFFER_BYTES {
        buf.resize(ATTR_BULK_BUFFER_BYTES, 0);
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: the folder descriptor is live; attrs and buf are valid for their sizes.
        let n = unsafe {
            libc::getattrlistbulk(
                dir.as_raw_fd(),
                (&mut attrs as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                flags as u64,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(out);
        }
        let mut pos = 0usize;
        for _ in 0..n {
            let rd = |off: usize, len: usize| -> io::Result<&[u8]> {
                buf.get(off..off + len).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short getattrlistbulk entry"))
            };
            let u32_at = |off: usize| -> io::Result<u32> { Ok(u32::from_ne_bytes(rd(off, 4)?.try_into().unwrap())) };
            let i32_at = |off: usize| -> io::Result<i32> { Ok(i32::from_ne_bytes(rd(off, 4)?.try_into().unwrap())) };
            let u64_at = |off: usize| -> io::Result<u64> { Ok(u64::from_ne_bytes(rd(off, 8)?.try_into().unwrap())) };
            let time_at = |off: usize| -> io::Result<(i64, i64)> { Ok((u64_at(off)? as i64, u64_at(off + 8)? as i64)) };
            let len = u32_at(pos)? as usize;
            if len == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "empty getattrlistbulk entry"));
            }
            // Layout: length, returned attributes (5 words), then the attributes in bit order.
            let returned = pos + 4;
            let (cmn, fork) = (u32_at(returned)?, u32_at(returned + 16)?);
            let mut q = returned + 20;
            let name_ref = q;
            q += 8;
            let dev = u32_at(q)? as u64;
            q += 4;
            let birth = time_at(q)?;
            q += 16;
            let mtime = time_at(q)?;
            q += 16;
            let ctime = time_at(q)?;
            q += 16;
            let uid = u32_at(q)?;
            q += 4;
            let gid = u32_at(q)?;
            q += 4;
            let access = u32_at(q)?;
            q += 4;
            let flags = u32_at(q)?;
            q += 4;
            let generation = u32_at(q)?;
            q += 4;
            let ino = u64_at(q)?;
            q += 8;
            // The folder group comes only for folders, the file group only for other objects;
            // both start with a link count and a size. The access mask carries the type bits.
            // (Asking for ATTR_CMN_OBJTYPE as well makes the kernel drop the setuid bit.)
            let links = u32_at(q)?;
            q += 4;
            let size = u64_at(q)?;
            q += 8;
            let clone_id = u64_at(q)?;
            let name_off = name_ref as i64 + i32_at(name_ref)? as i64;
            let name_len = u32_at(name_ref + 4)? as usize;
            let raw = rd(name_off as usize, name_len)?;
            let name = std::ffi::OsStr::from_bytes(raw.strip_suffix(&[0]).unwrap_or(raw)).to_os_string();
            out.push(BulkEntry {
                name,
                mode: access,
                dev,
                ino,
                uid,
                gid,
                size,
                nlink: links as u64,
                mtime,
                ctime,
                birth,
                flags,
                data: ApfsDataIdentity {
                    generation: (cmn & libc::ATTR_CMN_GEN_COUNT != 0 && generation != 0).then_some(generation),
                    clone_id: (fork & libc::ATTR_CMNEXT_CLONEID != 0 && clone_id != 0).then_some(clone_id),
                },
            });
            pos += len;
        }
    }
}

// Retry when an attribute grows between the size query and read.
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
fn read_xattr_buffer(initial_bytes: usize, mut read: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    resize_lookup_buffer(&mut buffer, initial_bytes, XATTR_MAX_BYTES, "xattr bytes")?;
    loop {
        match read(&mut buffer) {
            Ok(size) => {
                buffer.truncate(size);
                return Ok(buffer);
            }
            Err(rustix::io::Errno::RANGE) => {
                let size = read(&mut [])?;
                if buffer.len() == XATTR_MAX_BYTES || size > XATTR_MAX_BYTES {
                    return Err(io::Error::other(format!("xattr bytes exceed the limit of {XATTR_MAX_BYTES}")));
                }
                let next = size.saturating_add(XATTR_GROWTH_SLACK_BYTES).max(buffer.len().saturating_mul(2)).min(XATTR_MAX_BYTES);
                resize_lookup_buffer(&mut buffer, next, XATTR_MAX_BYTES, "xattr bytes")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(target_os = "macos")]
pub fn apfs_marker(path: &std::path::Path, value: Option<&[u8]>) -> io::Result<Vec<u8>> {
    apfs_attribute(path, c"com.layr.subvolume", value)
}
#[cfg(target_os = "macos")]
pub fn apfs_attribute(path: &std::path::Path, name: &CStr, value: Option<&[u8]>) -> io::Result<Vec<u8>> {
    if let Some(value) = value {
        rustix::fs::lsetxattr(path, name, value, rustix::fs::XattrFlags::empty())?;
        return Ok(Vec::new());
    }
    read_xattr_buffer(XATTR_INITIAL_VALUE_BYTES, |buffer| rustix::fs::lgetxattr(path, name, buffer))
}
/// Set (`readonly`) or clear the immutable flag of the entry itself. Clearing also removes the
/// seal entry of its ACL (`apfs_seal_entry`): a copy of a sealed entry can carry either seal, and
/// every caller that opens an entry for a change opens both.
#[cfg(target_os = "macos")]
pub fn apfs_immutable(path: &std::path::Path, readonly: bool) -> io::Result<()> {
    // Clear the flag before editing an ACL: an immutable entry refuses ACL writes.
    apfs_flag(path, readonly)?;
    if !readonly {
        apfs_unseal_entry(path)?;
    }
    Ok(())
}

/// Set or clear only the immutable flag of the entry itself (no ACL read).
#[cfg(target_os = "macos")]
pub fn apfs_flag(path: &std::path::Path, readonly: bool) -> io::Result<()> {
    use std::os::macos::fs::MetadataExt;
    let flags = std::fs::symlink_metadata(path)?.st_flags();
    apfs_set_flags(path, flags, if readonly { flags | libc::UF_IMMUTABLE } else { flags & !libc::UF_IMMUTABLE })
}

/// Change the flags of the entry itself from `flags` (known, from a scan) to `next`.
#[cfg(target_os = "macos")]
pub fn apfs_set_flags(path: &std::path::Path, flags: u32, next: u32) -> io::Result<()> {
    if next == flags {
        return Ok(());
    }
    let path = cpath(path)?;
    // SAFETY: the path is valid; lchflags changes the entry itself, never the target of a link.
    if unsafe { libc::lchflags(path.as_ptr(), next) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ----- The seal of the entries of a state (docs/apfs.md, "Seals") -----
//
// An entry below the root of a sealed tree has one ACL entry first: everyone is denied writes of
// its data, new or removed entries in it, and changes of its attributes, extended attributes, ACL
// and owner. A folder clone (`clonefile`) does not copy the ACLs of the entries below its root, so
// a writable copy of a sealed tree needs no pass over its entries. The root keeps the immutable
// flag: its rename and its marker need no ACL change.

/// The denied rights of the seal entry. `WRITE_DATA` is also `ADD_FILE` and `APPEND_DATA` is also
/// `ADD_SUBDIRECTORY` for a folder; `DELETE` stays allowed, so a delete opens only the folders.
#[cfg(target_os = "macos")]
const SEAL_RIGHTS: u64 = (1 << 2) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13);
#[cfg(target_os = "macos")]
const ACL_EXTENDED_DENY: libc::c_int = 2;
/// The seal entry is marked inherited: copies (`clonefile` with `CLONE_ACL`, `copyfile` with
/// `COPYFILE_ACL`) leave inherited entries out, so no copy of a sealed entry carries the seal, also
/// a copy that an older layr makes. The kernel applies the entry like any other.
#[cfg(target_os = "macos")]
const ACL_ENTRY_INHERITED: libc::c_int = 1 << 4;
#[cfg(target_os = "macos")]
const ACL_FIRST_ENTRY: libc::c_int = 0;
#[cfg(target_os = "macos")]
const ACL_NEXT_ENTRY: libc::c_int = -1;
/// The UUID of the group `everyone` (gid 12), which every user is a member of.
#[cfg(target_os = "macos")]
const EVERYONE: [u8; 16] = [0xAB, 0xCD, 0xEF, 0xAB, 0xCD, 0xEF, 0xAB, 0xCD, 0xEF, 0xAB, 0xCD, 0xEF, 0x00, 0x00, 0x00, 0x0C];

// SAFETY: these declarations match the macOS ACL C ABI (sys/acl.h).
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_free(ptr: *mut libc::c_void) -> libc::c_int;
    fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
    fn acl_set_link_np(path: *const libc::c_char, kind: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
    fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, kind: libc::c_int) -> libc::c_int;
    fn acl_get_entry(acl: *mut libc::c_void, id: libc::c_int, entry: *mut *mut libc::c_void) -> libc::c_int;
    fn acl_create_entry_np(acl: *mut *mut libc::c_void, entry: *mut *mut libc::c_void, index: libc::c_int) -> libc::c_int;
    fn acl_delete_entry(acl: *mut libc::c_void, entry: *mut libc::c_void) -> libc::c_int;
    fn acl_get_tag_type(entry: *mut libc::c_void, tag: *mut libc::c_int) -> libc::c_int;
    fn acl_set_tag_type(entry: *mut libc::c_void, tag: libc::c_int) -> libc::c_int;
    fn acl_get_qualifier(entry: *mut libc::c_void) -> *mut libc::c_void;
    fn acl_set_qualifier(entry: *mut libc::c_void, qualifier: *const libc::c_void) -> libc::c_int;
    fn acl_get_permset_mask_np(entry: *mut libc::c_void, mask: *mut u64) -> libc::c_int;
    fn acl_set_permset_mask_np(entry: *mut libc::c_void, mask: u64) -> libc::c_int;
    fn acl_to_text(acl: *mut libc::c_void, len: *mut libc::ssize_t) -> *mut libc::c_char;
    fn acl_get_flagset_np(obj: *mut libc::c_void, flagset: *mut *mut libc::c_void) -> libc::c_int;
    fn acl_set_flagset_np(obj: *mut libc::c_void, flagset: *mut libc::c_void) -> libc::c_int;
    fn acl_add_flag_np(flagset: *mut libc::c_void, flag: libc::c_int) -> libc::c_int;
    fn acl_get_flag_np(flagset: *mut libc::c_void, flag: libc::c_int) -> libc::c_int;
}

/// An ACL that libc allocated; freed once.
#[cfg(target_os = "macos")]
struct Acl(*mut libc::c_void);
#[cfg(target_os = "macos")]
impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: the pointer came from acl_init or acl_get_*_np and is freed only here.
        unsafe {
            acl_free(self.0);
        }
    }
}
#[cfg(target_os = "macos")]
impl Acl {
    /// The ACL that `get` returns; `None` when the entry has none.
    fn read(get: impl FnOnce() -> *mut libc::c_void) -> io::Result<Option<Acl>> {
        let acl = get();
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOENT) | Some(libc::EOPNOTSUPP) | Some(libc::ENOTSUP) => Ok(None),
                _ => Err(error),
            };
        }
        Ok(Some(Acl(acl)))
    }

    fn of_path(path: &CStr) -> io::Result<Option<Acl>> {
        // SAFETY: the path is a valid C string; the result is owned by the wrapper.
        Acl::read(|| unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) })
    }

    fn of_fd(fd: libc::c_int) -> io::Result<Option<Acl>> {
        // SAFETY: the descriptor is open; the result is owned by the wrapper.
        Acl::read(|| unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) })
    }

    /// The entries in order.
    fn entries(&self) -> Vec<*mut libc::c_void> {
        let mut out = Vec::new();
        let mut entry: *mut libc::c_void = std::ptr::null_mut();
        let mut id = ACL_FIRST_ENTRY;
        // SAFETY: the ACL is live; each entry pointer stays valid while the ACL is unchanged.
        while unsafe { acl_get_entry(self.0, id, &mut entry) } == 0 {
            out.push(entry);
            id = ACL_NEXT_ENTRY;
        }
        out
    }

    /// Remove the seal entries. True when there was one.
    fn remove_seal(&mut self) -> io::Result<bool> {
        let mut removed = false;
        // Deleting an entry can move the remaining entries. Obtain a new pointer each time.
        while let Some(entry) = self.entries().into_iter().find(|e| is_seal_entry(*e)) {
            // SAFETY: the entry belongs to this live, unchanged ACL.
            if unsafe { acl_delete_entry(self.0, entry) } != 0 {
                return Err(io::Error::last_os_error());
            }
            removed = true;
        }
        Ok(removed)
    }

    /// Put the seal entry first.
    fn add_seal(&mut self) -> io::Result<()> {
        let mut entry: *mut libc::c_void = std::ptr::null_mut();
        // SAFETY: the ACL is live and may be reallocated by acl_create_entry_np, which updates
        // the pointer it owns; the qualifier is 16 bytes of a guid_t.
        unsafe {
            if acl_create_entry_np(&mut self.0, &mut entry, 0) != 0
                || acl_set_tag_type(entry, ACL_EXTENDED_DENY) != 0
                || acl_set_qualifier(entry, EVERYONE.as_ptr().cast()) != 0
                || acl_set_permset_mask_np(entry, SEAL_RIGHTS) != 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut flags: *mut libc::c_void = std::ptr::null_mut();
            if acl_get_flagset_np(entry, &mut flags) != 0
                || acl_add_flag_np(flags, ACL_ENTRY_INHERITED) != 0
                || acl_set_flagset_np(entry, flags) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }

    fn set_path(&self, path: &CStr) -> io::Result<()> {
        // SAFETY: the path is a valid C string; NOFOLLOW is the _link_ variant.
        if unsafe { acl_set_link_np(path.as_ptr(), ACL_TYPE_EXTENDED, self.0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_fd(&self, fd: libc::c_int) -> io::Result<()> {
        // SAFETY: the descriptor is open and the ACL is live.
        if unsafe { acl_set_fd_np(fd, self.0, ACL_TYPE_EXTENDED) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn new() -> io::Result<Acl> {
        // SAFETY: acl_init allocates an empty ACL that the wrapper frees.
        let acl = unsafe { acl_init(1) };
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Acl(acl))
    }
}

/// True for the seal entry: an inherited deny entry of `everyone` with exactly the seal rights.
#[cfg(target_os = "macos")]
fn is_seal_entry(entry: *mut libc::c_void) -> bool {
    let (mut tag, mut mask) = (0, 0u64);
    let mut flags: *mut libc::c_void = std::ptr::null_mut();
    // SAFETY: the entry is live; the flag set belongs to it; the qualifier is a guid_t that
    // acl_get_qualifier allocates and that is freed once.
    unsafe {
        if acl_get_tag_type(entry, &mut tag) != 0 || tag != ACL_EXTENDED_DENY {
            return false;
        }
        if acl_get_permset_mask_np(entry, &mut mask) != 0 || mask != SEAL_RIGHTS {
            return false;
        }
        if acl_get_flagset_np(entry, &mut flags) != 0 || acl_get_flag_np(flags, ACL_ENTRY_INHERITED) != 1 {
            return false;
        }
        let q = acl_get_qualifier(entry);
        if q.is_null() {
            return false;
        }
        let everyone = std::slice::from_raw_parts(q.cast::<u8>(), 16) == EVERYONE;
        acl_free(q);
        everyone
    }
}

/// The ACL of the entry itself, read raw with one `getattrlist` call and no parse into a libc
/// ACL: the number of its entries and of its seal entries. `None` without an ACL. The common
/// cases (no ACL, the seal alone) need nothing more.
#[cfg(target_os = "macos")]
fn acl_counts(path: &CStr) -> io::Result<Option<(usize, usize)>> {
    const MAGIC: u32 = 0x012c_c16d;
    const KINDMASK: u32 = 0xf;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_EXTENDED_SECURITY,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: the path is a valid C string; attrs and buf are valid for their sizes.
        let r = unsafe {
            libc::getattrlist(path.as_ptr(), (&mut attrs as *mut libc::attrlist).cast(), buf.as_mut_ptr().cast(), buf.len(), libc::FSOPT_NOFOLLOW)
        };
        if r != 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) => Ok(None),
                _ => Err(error),
            };
        }
        let u32_at = |off: usize| -> Option<u32> { buf.get(off..off + 4).map(|b| u32::from_ne_bytes(b.try_into().unwrap())) };
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "short ACL attribute");
        // Layout: length, returned attributes (5 words), then the reference to the filesec.
        if u32_at(4).ok_or_else(bad)? & libc::ATTR_CMN_EXTENDED_SECURITY == 0 {
            return Ok(None);
        }
        let reference = 24;
        let offset = u32_at(reference).ok_or_else(bad)? as i32 as isize;
        let len = u32_at(reference + 4).ok_or_else(bad)? as usize;
        if len == 0 {
            return Ok(None);
        }
        let start = (reference as isize + offset) as usize;
        if start + len > buf.len() {
            buf.resize(start + len + 64, 0);
            continue;
        }
        let sec = &buf[start..start + len];
        let word = |off: usize| -> Option<[u8; 4]> { sec.get(off..off + 4).map(|b| b.try_into().unwrap()) };
        let raw_magic = word(0).ok_or_else(bad)?;
        let big = match (u32::from_ne_bytes(raw_magic), u32::from_be_bytes(raw_magic)) {
            (MAGIC, _) => false,
            (_, MAGIC) => true,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown ACL attribute")),
        };
        let value = |off: usize| -> Option<u32> { word(off).map(|w| if big { u32::from_be_bytes(w) } else { u32::from_ne_bytes(w) }) };
        // magic, owner and group GUIDs, then the ACL: entry count, flags, entries of 24 bytes.
        let count = value(36).ok_or_else(bad)?;
        if count == u32::MAX || count == 0 {
            return Ok(None);
        }
        let mut seals = 0;
        for i in 0..count as usize {
            let at = 44 + i * 24;
            let guid = sec.get(at..at + 16).ok_or_else(bad)?;
            let (flags, rights) = (value(at + 16).ok_or_else(bad)?, value(at + 20).ok_or_else(bad)?);
            if guid == EVERYONE
                && flags & KINDMASK == ACL_EXTENDED_DENY as u32
                && flags & ACL_ENTRY_INHERITED as u32 != 0
                && u64::from(rights) == SEAL_RIGHTS
            {
                seals += 1;
            }
        }
        return Ok(Some((count as usize, seals)));
    }
}

/// Seal one entry below the root of a tree: the seal entry goes first in its ACL; other ACL
/// entries stay. An entry that has it already is not written.
#[cfg(target_os = "macos")]
pub fn apfs_seal_entry(path: &std::path::Path) -> io::Result<()> {
    let path = cpath(path)?;
    if acl_counts(&path)?.is_none() {
        let mut acl = Acl::new()?;
        acl.add_seal()?;
        return acl.set_path(&path);
    }
    let mut acl = match Acl::of_path(&path)? {
        Some(acl) if acl.entries().first().is_some_and(|e| is_seal_entry(*e)) => return Ok(()),
        Some(mut acl) => {
            acl.remove_seal()?;
            acl
        }
        None => Acl::new()?,
    };
    acl.add_seal()?;
    acl.set_path(&path)
}

/// Remove the seal entry of one entry, if it has one. Other ACL entries stay.
#[cfg(target_os = "macos")]
pub fn apfs_unseal_entry(path: &std::path::Path) -> io::Result<()> {
    let path = cpath(path)?;
    if !acl_counts(&path)?.is_some_and(|(_, seals)| seals > 0) {
        return Ok(());
    }
    if let Some(mut acl) = Acl::of_path(&path)? {
        if acl.remove_seal()? {
            acl.set_path(&path)?;
        }
    }
    Ok(())
}

/// True when the entry has the seal entry in its ACL.
#[cfg(target_os = "macos")]
pub fn apfs_has_seal_entry(path: &std::path::Path) -> io::Result<bool> {
    let path = cpath(path)?;
    Ok(acl_counts(&path)?.is_some_and(|(_, seals)| seals > 0))
}

/// `apfs_unseal_entry` through an open descriptor.
#[cfg(target_os = "macos")]
fn unseal_entry_fd(fd: libc::c_int) -> io::Result<()> {
    if let Some(mut acl) = Acl::of_fd(fd)? {
        if acl.remove_seal()? {
            acl.set_fd(fd)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_publish(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(rustix::fs::CWD, src, rustix::fs::CWD, dst, rustix::fs::RenameFlags::NOREPLACE)?)
}

/// Exchange two entries in one step.
#[cfg(target_os = "macos")]
pub fn apfs_swap(a: &std::path::Path, b: &std::path::Path) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(rustix::fs::CWD, a, rustix::fs::CWD, b, rustix::fs::RenameFlags::EXCHANGE)?)
}

#[cfg(target_os = "macos")]
pub fn apfs_copy_metadata(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    let (src, dst) = (cpath(src)?, cpath(dst)?);
    // SAFETY: both paths are valid C strings. NOFOLLOW applies to both final entries.
    if unsafe {
        libc::copyfile(src.as_ptr(), dst.as_ptr(), std::ptr::null_mut(), libc::COPYFILE_METADATA | libc::COPYFILE_NOFOLLOW)
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_copy_metadata_fd(src: std::os::fd::BorrowedFd<'_>, dst: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: both descriptors remain open; the state pointer may be null.
    if unsafe { libc::fcopyfile(src.as_raw_fd(), dst.as_raw_fd(), std::ptr::null_mut(), libc::COPYFILE_METADATA) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_copy_acl_fd(src: std::os::fd::BorrowedFd<'_>, dst: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: both descriptors remain open. Copy ACLs without replacing the layer marker.
    if unsafe { libc::fcopyfile(src.as_raw_fd(), dst.as_raw_fd(), std::ptr::null_mut(), libc::COPYFILE_ACL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // The seal of a state entry is inherited: the copy leaves it out.
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_clone_at(src: std::os::fd::BorrowedFd<'_>, dir: std::os::fd::BorrowedFd<'_>, name: &str) -> io::Result<()> {
    Ok(rustix::fs::fclonefileat(
        src,
        dir,
        name,
        rustix::fs::CloneFlags::NOOWNERCOPY | rustix::fs::CloneFlags::from_bits_retain(CLONE_ACL),
    )?)
}
#[cfg(target_os = "macos")]
pub fn apfs_unseal_fd(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    let st = rustix::fs::fstat(fd)?;
    // SAFETY: fd remains open and fchflags takes only integer flags.
    if st.st_flags & libc::UF_IMMUTABLE != 0 && unsafe { libc::fchflags(fd.as_raw_fd(), st.st_flags & !libc::UF_IMMUTABLE) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    unseal_entry_fd(fd.as_raw_fd())
}

#[cfg(target_os = "macos")]
pub fn apfs_xattrs(path: &std::path::Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    apfs_xattrs_where(path, |_| true)
}

/// The attributes of `path` as `apfs_xattrs` reads them; the value of an attribute that
/// `read_value` refuses stays empty. Reading a value costs a system call of its own.
#[cfg(target_os = "macos")]
pub fn apfs_xattrs_where(path: &std::path::Path, read_value: impl Fn(&[u8]) -> bool) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let names = read_xattr_buffer(XATTR_INITIAL_NAMES_BYTES, |buffer| rustix::fs::llistxattr(path, buffer))?;
    names
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let value = if read_value(name) { apfs_attribute(path, &CString::new(name)?, None)? } else { Vec::new() };
            Ok((name.to_vec(), value))
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub fn apfs_remove_attribute(path: &std::path::Path, name: &CStr) -> io::Result<()> {
    Ok(rustix::fs::lremovexattr(path, name)?)
}
#[cfg(target_os = "macos")]
pub fn apfs_mtime(path: &std::path::Path, sec: i64, nsec: i64) -> io::Result<()> {
    let times = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_OMIT },
        last_modification: rustix::fs::Timespec { tv_sec: sec, tv_nsec: nsec },
    };
    Ok(rustix::fs::utimensat(rustix::fs::CWD, path, &times, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?)
}

/// Set the creation time of the entry itself. APFS clones keep it; new entries need it set.
#[cfg(target_os = "macos")]
pub fn apfs_set_birthtime(path: &std::path::Path, sec: i64, nsec: i64) -> io::Result<()> {
    let path = cpath(path)?;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_CRTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut time = libc::timespec { tv_sec: sec, tv_nsec: nsec };
    // SAFETY: attrs and the timespec stay live for the call; the path is NUL-terminated.
    if unsafe {
        libc::setattrlist(
            path.as_ptr(),
            (&mut attrs as *mut libc::attrlist).cast(),
            (&mut time as *mut libc::timespec).cast(),
            std::mem::size_of::<libc::timespec>(),
            libc::FSOPT_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn peer_credentials(sock: &std::os::unix::net::UnixStream) -> io::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: the socket is live, and both output pointers refer to writable uid/gid values.
    if unsafe { libc::getpeereid(sock.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((uid, gid))
}

#[cfg(target_os = "macos")]
pub fn apfs_owners_enabled(path: &std::path::Path) -> io::Result<bool> {
    let st = rustix::fs::statfs(path)?;
    Ok(st.f_flags & (libc::MNT_IGNORE_OWNERSHIP as u32) == 0)
}

#[cfg(target_os = "macos")]
pub fn apfs_has_acl(entry: &std::path::Path) -> io::Result<bool> {
    let path = cpath(entry)?;
    // SAFETY: CHECK inspects only the live source string and does not use a destination.
    let result = unsafe {
        libc::copyfile(
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            libc::COPYFILE_CHECK | libc::COPYFILE_ACL | libc::COPYFILE_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // The seal of a state entry is no ACL of its content.
    Ok(result as u32 & libc::COPYFILE_ACL != 0 && apfs_acl_text(entry)?.is_some())
}
/// The ACL of `path` (the link itself) as text, without the seal entry of a state entry;
/// `None` without another ACL entry.
#[cfg(target_os = "macos")]
pub fn apfs_acl_text(path: &std::path::Path) -> io::Result<Option<Vec<u8>>> {
    let path = cpath(path)?;
    match acl_counts(&path) {
        Ok(None) => return Ok(None),
        Ok(Some((count, seals))) if count == seals => return Ok(None),
        // A volume without ACLs, or another error: the libc read below decides.
        _ => {}
    }
    let Some(mut acl) = Acl::of_path(&path)? else { return Ok(None) };
    acl.remove_seal()?;
    if acl.is_empty() {
        return Ok(None);
    }
    let mut len: libc::ssize_t = 0;
    // SAFETY: the ACL is live; the text is owned by libc, copied, and freed once.
    unsafe {
        let text = acl_to_text(acl.0, &mut len);
        if text.is_null() {
            return Err(io::Error::last_os_error());
        }
        let out = std::ffi::CStr::from_ptr(text).to_bytes().to_vec();
        acl_free(text.cast());
        Ok(Some(out))
    }
}

#[cfg(target_os = "macos")]
pub fn apfs_clear_acl(path: &std::path::Path) -> io::Result<()> {
    let path = cpath(path)?;
    // SAFETY: acl_init allocates an empty ACL that the wrapper frees.
    let acl = Acl(unsafe { acl_init(0) });
    if acl.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    acl.set_path(&path)
}

// ----- OverlayFS mount and metadata operations -----
#[cfg(feature = "overlayfs")]
pub fn overlay_mount(
    source: Option<&std::path::Path>,
    target: &std::path::Path,
    kind: Option<&CStr>,
    flags: libc::c_ulong,
    options: Option<&str>,
) -> io::Result<()> {
    use rustix::mount::{mount, mount_bind, mount_change, mount_move, mount_remount, MountFlags, MountPropagationFlags};
    let options = options.map(CString::new).transpose().map_err(|_| io::Error::other("NUL in mount options"))?;
    let mount_flags = MountFlags::from_bits_retain(flags as u32);
    let source = source.unwrap_or_else(|| std::path::Path::new(""));
    if flags & libc::MS_REMOUNT != 0 {
        mount_remount(
            target,
            MountFlags::from_bits_retain((flags & !libc::MS_REMOUNT) as u32),
            options.as_deref().unwrap_or(c""),
        )?;
    } else if flags & libc::MS_BIND != 0 {
        mount_bind(source, target)?;
    } else if flags & libc::MS_MOVE != 0 {
        mount_move(source, target)?;
    } else if flags & (libc::MS_SHARED | libc::MS_PRIVATE) != 0 {
        mount_change(target, MountPropagationFlags::from_bits_retain(flags as u32))?;
    } else {
        mount(source, target, kind.unwrap_or(c""), mount_flags, options.as_deref())?;
    }
    Ok(())
}
#[cfg(feature = "overlayfs")]
pub fn overlay_unmount(path: &std::path::Path) -> io::Result<()> {
    // Never use lazy detach for managed data.
    Ok(rustix::mount::unmount(path, rustix::mount::UnmountFlags::empty())?)
}
#[cfg(feature = "overlayfs")]
pub fn overlay_attribute(path: &std::path::Path, name: &CStr, value: Option<&[u8]>) -> io::Result<Vec<u8>> {
    if let Some(value) = value {
        rustix::fs::lsetxattr(path, name, value, rustix::fs::XattrFlags::empty())?;
        return Ok(Vec::new());
    }
    read_xattr_buffer(XATTR_INITIAL_VALUE_BYTES, |buffer| rustix::fs::lgetxattr(path, name, buffer))
}
#[cfg(feature = "overlayfs")]
pub fn overlay_remove_attribute(path: &std::path::Path, name: &CStr) -> io::Result<()> {
    Ok(rustix::fs::lremovexattr(path, name)?)
}
#[cfg(feature = "overlayfs")]
pub fn overlay_xattrs(path: &std::path::Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let names = read_xattr_buffer(XATTR_INITIAL_NAMES_BYTES, |buffer| rustix::fs::llistxattr(path, buffer))?;
    names
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| Ok((name.to_vec(), overlay_attribute(path, &CString::new(name)?, None)?)))
        .collect()
}
#[cfg(feature = "overlayfs")]
pub fn overlay_mtime(path: &std::path::Path, sec: i64, nano: i64) -> io::Result<()> {
    let times = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_OMIT },
        last_modification: rustix::fs::Timespec { tv_sec: sec, tv_nsec: nano },
    };
    Ok(rustix::fs::utimensat(rustix::fs::CWD, path, &times, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?)
}

/// Permanently drop all user and group privileges in a single-threaded service child.
pub fn drop_to_user(uid: u32, gid: u32) -> Result<()> {
    if uid == 0 {
        anyhow::bail!("registry.user must be unprivileged");
    }
    if gid == 0 {
        anyhow::bail!("registry.user must use an unprivileged primary group");
    }
    set_groups(&[])?;
    #[cfg(target_os = "linux")]
    {
        rustix::thread::set_thread_gid(rustix::process::Gid::from_raw(gid))?;
        rustix::thread::set_thread_uid(rustix::process::Uid::from_raw(uid))?;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: the caller is a dedicated single-threaded child; these calls have no pointers.
    if unsafe { libc::setgid(gid) } != 0 || unsafe { libc::setuid(uid) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    if rustix::process::getuid().as_raw() != uid || rustix::process::geteuid().as_raw() != uid {
        anyhow::bail!("cannot drop registry privileges");
    }
    Ok(())
}

/// One entry of a folder with its sizes for `layr du`: allocated bytes and the bytes that no
/// clone shares (APFS private size).
#[cfg(target_os = "macos")]
pub struct SpaceEntry {
    pub name: std::ffi::OsString,
    pub dir: bool,
    pub ino: u64,
    pub links: u32,
    pub alloc: u64,
    pub private: u64,
    /// The data identity that clones of this file share (0: none).
    pub clone: u64,
}

/// Read the sizes of all entries of an open folder with `getattrlistbulk`.
#[cfg(target_os = "macos")]
pub fn apfs_space_read(dir: BorrowedFd<'_>, buf: &mut Vec<u8>) -> io::Result<Vec<SpaceEntry>> {
    use std::os::unix::ffi::OsStrExt;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE | libc::ATTR_CMNEXT_CLONEID,
    };
    let flags = libc::FSOPT_NOFOLLOW | libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED;
    if buf.len() < ATTR_BULK_BUFFER_BYTES {
        buf.resize(ATTR_BULK_BUFFER_BYTES, 0);
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: the folder descriptor is live; attrs and buf are valid for their sizes.
        let n = unsafe {
            libc::getattrlistbulk(
                dir.as_raw_fd(),
                (&mut attrs as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                flags as u64,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(out);
        }
        let mut pos = 0usize;
        for _ in 0..n {
            let rd = |off: usize, len: usize| -> io::Result<&[u8]> {
                buf.get(off..off + len).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short getattrlistbulk entry"))
            };
            let u32_at = |off: usize| -> io::Result<u32> { Ok(u32::from_ne_bytes(rd(off, 4)?.try_into().unwrap())) };
            let i32_at = |off: usize| -> io::Result<i32> { Ok(i32::from_ne_bytes(rd(off, 4)?.try_into().unwrap())) };
            let u64_at = |off: usize| -> io::Result<u64> { Ok(u64::from_ne_bytes(rd(off, 8)?.try_into().unwrap())) };
            let len = u32_at(pos)? as usize;
            if len == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "empty getattrlistbulk entry"));
            }
            // Layout: length, returned attributes (5 words), then the attributes in bit order:
            // name, type, file id; for a file the link count and the allocated size; then the
            // private size and the clone id. A folder has no file group.
            let mut q = pos + 4 + 20;
            let name_ref = q;
            q += 8;
            let dir = u32_at(q)? == 2; // VDIR
            q += 4;
            let ino = u64_at(q)?;
            q += 8;
            let (links, alloc) = if dir {
                (1, 0)
            } else {
                let links = u32_at(q)?;
                let alloc = u64_at(q + 4)?;
                q += 12;
                (links, alloc)
            };
            let (private, clone) = if dir { (0, 0) } else { (u64_at(q)?, u64_at(q + 8)?) };
            let name_off = name_ref as i64 + i32_at(name_ref)? as i64;
            let name_len = u32_at(name_ref + 4)? as usize;
            let raw = rd(name_off as usize, name_len)?;
            let name = std::ffi::OsStr::from_bytes(raw.strip_suffix(&[0]).unwrap_or(raw)).to_os_string();
            out.push(SpaceEntry { name, dir, ino, links, alloc, private, clone });
            pos += len;
        }
    }
}

/// Every extent of an open file (`FIEMAP`): logical offset, physical address, length and the
/// flags of the extent (`FIEMAP_EXTENT_*`). Used to find files with the same data.
#[cfg(target_os = "linux")]
pub fn fiemap_full(file: &std::fs::File) -> io::Result<Vec<(u64, u64, u64, u32)>> {
    const EXTENT_LAST: u32 = 0x1;
    const COUNT: usize = 256;
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    #[repr(C)]
    struct Map {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
        extents: [Extent; COUNT],
    }
    let mut out = Vec::new();
    let mut start = 0u64;
    loop {
        let mut map = Map {
            start,
            length: u64::MAX - start,
            flags: 0,
            mapped: 0,
            count: COUNT as u32,
            reserved: 0,
            extents: [Extent::default(); COUNT],
        };
        // SAFETY: the descriptor is live and `map` is a valid fiemap buffer with room for COUNT extents.
        if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &mut map as *mut Map) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mapped = (map.mapped as usize).min(COUNT);
        if mapped == 0 {
            return Ok(out);
        }
        for e in &map.extents[..mapped] {
            out.push((e.logical, e.physical, e.length, e.flags));
            if e.flags & EXTENT_LAST != 0 {
                return Ok(out);
            }
        }
        let last = map.extents[mapped - 1];
        start = last.logical + last.length;
    }
}

/// An extent returned by FIEMAP; unstable addresses are absent.
#[cfg(target_os = "linux")]
pub struct MappedExtent {
    pub logical: u64,
    pub addr: Option<u64>,
    pub len: u64,
    pub shared: bool,
}

/// The extents of an open file (`FIEMAP`): physical address, length, and whether another file
/// or snapshot shares it (`FIEMAP_EXTENT_SHARED`). btrfs, XFS and ext4.
#[cfg(target_os = "linux")]
pub fn fiemap_extents(file: &std::fs::File) -> io::Result<Vec<MappedExtent>> {
    const EXTENT_LAST: u32 = 0x1;
    const EXTENT_SHARED: u32 = 0x2000;
    const COUNT: usize = 256;
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    #[repr(C)]
    struct Map {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
        extents: [Extent; COUNT],
    }
    let mut out = Vec::new();
    let mut start = 0u64;
    loop {
        let mut map = Map {
            start,
            length: u64::MAX - start,
            flags: 0,
            mapped: 0,
            count: COUNT as u32,
            reserved: 0,
            extents: [Extent::default(); COUNT],
        };
        // SAFETY: the descriptor is live and `map` is a valid fiemap buffer with room for COUNT extents.
        if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &mut map as *mut Map) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mapped = (map.mapped as usize).min(COUNT);
        if mapped == 0 {
            return Ok(out);
        }
        for e in &map.extents[..mapped] {
            let addressable = e.flags & FIEMAP_UNSTABLE == 0;
            out.push(MappedExtent {
                logical: e.logical,
                addr: (addressable && e.physical != 0).then_some(e.physical),
                len: e.length,
                shared: e.flags & EXTENT_SHARED != 0,
            });
            if e.flags & EXTENT_LAST != 0 {
                return Ok(out);
            }
        }
        let last = map.extents[mapped - 1];
        start = last.logical + last.length;
    }
}

#[cfg(all(test, target_os = "linux"))]
pub fn test_thread_priority() -> (libc::c_int, libc::c_int) {
    // SAFETY: ioprio_get queries only the calling thread and takes no pointers.
    unsafe {
        (
            rustix::process::getpriority_process(None).unwrap(),
            libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0) as libc::c_int,
        )
    }
}

#[cfg(all(test, any(target_os = "macos", feature = "overlayfs")))]
mod metadata_tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use super::{
        apfs_attribute as attribute, apfs_mtime as mtime, apfs_remove_attribute as remove_attribute, apfs_xattrs as xattrs,
    };
    #[cfg(feature = "overlayfs")]
    use super::{
        overlay_attribute as attribute, overlay_mtime as mtime, overlay_remove_attribute as remove_attribute,
        overlay_xattrs as xattrs,
    };

    #[test]
    fn attribute_growth_race_retries() {
        let mut calls = 0;
        let value = read_xattr_buffer(1, |buffer| {
            calls += 1;
            if buffer.is_empty() {
                return Ok(2);
            }
            if buffer.len() < 128 {
                return Err(rustix::io::Errno::RANGE);
            }
            buffer[..128].fill(7);
            Ok(128)
        })
        .unwrap();
        assert_eq!(value, vec![7; 128]);
        assert_eq!(calls, 5);
    }

    #[test]
    fn attribute_growth_stops_at_the_size_limit() {
        let mut calls = 0;
        let error = read_xattr_buffer(1, |buffer| {
            calls += 1;
            if buffer.is_empty() {
                Ok(0)
            } else {
                Err(rustix::io::Errno::RANGE)
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("xattr bytes exceed the limit"));
        assert!(calls < 64);
        let error = read_xattr_buffer(
            1,
            |buffer| {
                if buffer.is_empty() {
                    Ok(XATTR_MAX_BYTES + 1)
                } else {
                    Err(rustix::io::Errno::RANGE)
                }
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("xattr bytes exceed the limit"));
    }

    #[test]
    fn attributes_and_mtime_do_not_follow_symlinks() -> io::Result<()> {
        use std::os::unix::fs::{symlink, MetadataExt};
        let root = std::env::temp_dir().join(format!("layr-sys-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        let result = (|| {
            let file = root.join("file");
            let link = root.join("link");
            std::fs::write(&file, b"data")?;
            symlink(&file, &link)?;
            let name = c"user.layr-test";
            // Exceed the initial read buffer while leaving room for headers in ext4's xattr block.
            const TEST_XATTR_VALUE_BYTES: usize = 1024;
            let value = vec![42; TEST_XATTR_VALUE_BYTES];
            attribute(&file, name, Some(&value))?;
            assert_eq!(attribute(&file, name, None)?, value);
            assert!(xattrs(&file)?.iter().any(|(key, data)| key == name.to_bytes() && data == &value));
            assert!(attribute(&link, name, None).is_err());
            let before = std::fs::metadata(&file)?;
            mtime(&link, 1_600_000_000, 123)?;
            let after = std::fs::metadata(&file)?;
            let link_meta = std::fs::symlink_metadata(&link)?;
            assert_eq!((before.mtime(), before.mtime_nsec()), (after.mtime(), after.mtime_nsec()));
            assert_eq!((link_meta.mtime(), link_meta.mtime_nsec()), (1_600_000_000, 123));
            remove_attribute(&file, name)?;
            assert!(attribute(&file, name, None).is_err());
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod limit_tests {
    use super::*;

    #[test]
    fn check_limits_apply_only_to_child() -> Result<()> {
        let before = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "ulimit -n"]);
        check_limits_before_exec(&mut cmd, 1 << 30, 1 << 20, 10)?;
        let output = cmd.output()?;
        assert!(output.status.success());
        let actual: u64 = std::str::from_utf8(&output.stdout)?.trim().parse()?;
        assert_eq!(actual, before.maximum.map_or(CHECK_OPEN_FILES_COUNT, |maximum| maximum.min(CHECK_OPEN_FILES_COUNT)));
        assert_eq!(rustix::process::getrlimit(rustix::process::Resource::Nofile), before);
        Ok(())
    }
}
