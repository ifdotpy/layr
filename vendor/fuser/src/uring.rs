//! FUSE over io_uring (layr patch, Linux 6.14 and later with `fuse.enable_uring=1`).
//!
//! One thread per possible CPU, pinned to it, with its own io_uring and ring entries. The kernel
//! queues a request on the ring of the CPU where the caller runs, so the thread that serves it
//! and the caller share a CPU: no wakeup crosses CPUs (costly in a virtual machine). FORGET and
//! INTERRUPT still come through `/dev/fuse`, so the normal session threads keep running.
//!
//! A request arrives split in three parts (the `fuse_in_header`, the operation header, the
//! payload). This module joins them into the layout of a `/dev/fuse` read, dispatches it as
//! usual with a sender that keeps the reply, and splits the reply into the out header and the
//! payload of the ring entry.

use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use crate::Filesystem;
use crate::channel::Channel;
use crate::channel::ChannelSender;
use crate::dev_fuse::DevFuse;
use crate::request::RequestWithSender;
use crate::session::FilesystemHolder;
use crate::session::SessionACL;
use crate::session::SessionEventLoop;

const IORING_OP_URING_CMD: u8 = 46;
const IORING_SETUP_SQE128: u32 = 1 << 10;
const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;
const IORING_SETUP_DEFER_TASKRUN: u32 = 1 << 13;
const IORING_ENTER_GETEVENTS: u32 = 1;
const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;
const CMD_REGISTER: u32 = 1;
const CMD_COMMIT_AND_FETCH: u32 = 2;
/// `struct fuse_uring_req_header`: in_out (128), op_in (128), ring_ent_in_out (32).
const HEADER: usize = 288;
const OP_IN: usize = 128;
const ENT_IN_OUT: usize = 256;
const IN_HEADER: usize = 40;
const OUT_HEADER: usize = 16;
/// Ring entries per queue.
const ENTRIES: usize = 2;

/// The payload size of each entry; must be at least the kernel's max(max_write, max_pages pages).
pub(crate) static PAYLOAD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[repr(C)]
#[derive(Default)]
struct SqOff {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct CqOff {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqOff,
    cq_off: CqOff,
}

/// A minimal io_uring with 128-byte submission entries.
struct Ring {
    fd: i32,
    sq_ptr: *mut u8,
    sq_len: usize,
    cq_ptr: *mut u8,
    cq_len: usize,
    sqes: *mut u8,
    sqes_len: usize,
    p: Params,
    pending: u32,
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the mappings and the descriptor belong to this ring.
        unsafe {
            libc::munmap(self.sqes.cast(), self.sqes_len);
            if self.cq_ptr != self.sq_ptr {
                libc::munmap(self.cq_ptr.cast(), self.cq_len);
            }
            libc::munmap(self.sq_ptr.cast(), self.sq_len);
            libc::close(self.fd);
        }
    }
}

fn mmap(fd: i32, len: usize, off: i64) -> io::Result<*mut u8> {
    // SAFETY: a shared mapping of the ring descriptor at a documented offset.
    let p = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_POPULATE, fd, off)
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(p.cast())
}

impl Ring {
    fn new(entries: u32) -> io::Result<Ring> {
        let mut p = Params { flags: IORING_SETUP_SQE128 | IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN, ..Default::default() };
        // SAFETY: io_uring_setup with a valid params struct.
        let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, entries, &mut p as *mut Params) } as i32;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let sq_len = p.sq_off.array as usize + p.sq_entries as usize * 4;
        let cq_len = p.cq_off.cqes as usize + p.cq_entries as usize * 16;
        let single = p.features & 1 != 0;
        let (sq_len, cq_len) = if single { (sq_len.max(cq_len), sq_len.max(cq_len)) } else { (sq_len, cq_len) };
        let sq_ptr = mmap(fd, sq_len, IORING_OFF_SQ_RING)?;
        let cq_ptr = if single { sq_ptr } else { mmap(fd, cq_len, IORING_OFF_CQ_RING)? };
        let sqes_len = p.sq_entries as usize * 128;
        let sqes = mmap(fd, sqes_len, IORING_OFF_SQES)?;
        Ok(Ring { fd, sq_ptr, sq_len, cq_ptr, cq_len, sqes, sqes_len, p, pending: 0 })
    }

    fn u32_at(base: *mut u8, off: u32) -> &'static AtomicU32 {
        // SAFETY: the offset is one of the kernel's ring offsets inside the mapping.
        unsafe { &*(base.add(off as usize) as *const AtomicU32) }
    }

    /// Queue a URING_CMD entry for the FUSE device.
    fn push_cmd(&mut self, dev: i32, cmd_op: u32, iov: u64, user_data: u64, commit_id: u64, qid: u16) {
        let tail_a = Ring::u32_at(self.sq_ptr, self.p.sq_off.tail);
        let mask = Ring::u32_at(self.sq_ptr, self.p.sq_off.ring_mask).load(Ordering::Relaxed);
        let tail = tail_a.load(Ordering::Relaxed);
        let idx = tail & mask;
        // SAFETY: entry `idx` of the submission array, 128 bytes, owned by us until submitted.
        unsafe {
            let sqe = self.sqes.add(idx as usize * 128);
            std::ptr::write_bytes(sqe, 0, 128);
            *sqe = IORING_OP_URING_CMD;
            (sqe.add(4) as *mut i32).write_unaligned(dev);
            (sqe.add(8) as *mut u32).write_unaligned(cmd_op);
            (sqe.add(16) as *mut u64).write_unaligned(iov);
            (sqe.add(24) as *mut u32).write_unaligned(if iov != 0 { 2 } else { 0 });
            (sqe.add(32) as *mut u64).write_unaligned(user_data);
            // struct fuse_uring_cmd_req in the command area.
            (sqe.add(48 + 8) as *mut u64).write_unaligned(commit_id);
            (sqe.add(48 + 16) as *mut u16).write_unaligned(qid);
            let array = self.sq_ptr.add(self.p.sq_off.array as usize) as *mut u32;
            array.add(idx as usize).write(idx);
        }
        tail_a.store(tail.wrapping_add(1), Ordering::Release);
        self.pending += 1;
    }

    /// Submit the queued entries and wait for at least one completion.
    fn submit_and_wait(&mut self) -> io::Result<()> {
        loop {
            // SAFETY: io_uring_enter on our ring.
            let r = unsafe {
                libc::syscall(libc::SYS_io_uring_enter, self.fd, self.pending, 1u32, IORING_ENTER_GETEVENTS, std::ptr::null::<u8>(), 0usize)
            };
            if r >= 0 {
                self.pending -= (r as u32).min(self.pending);
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    /// The completions so far: (user data, result).
    fn reap(&mut self, out: &mut Vec<(u64, i32)>) {
        let head_a = Ring::u32_at(self.cq_ptr, self.p.cq_off.head);
        let tail = Ring::u32_at(self.cq_ptr, self.p.cq_off.tail).load(Ordering::Acquire);
        let mask = Ring::u32_at(self.cq_ptr, self.p.cq_off.ring_mask).load(Ordering::Relaxed);
        let mut head = head_a.load(Ordering::Relaxed);
        while head != tail {
            // SAFETY: a completion entry (16 bytes) inside the mapping.
            unsafe {
                let cqe = self.cq_ptr.add(self.p.cq_off.cqes as usize + (head & mask) as usize * 16);
                out.push(((cqe as *const u64).read_unaligned(), (cqe.add(8) as *const i32).read_unaligned()));
            }
            head = head.wrapping_add(1);
        }
        head_a.store(head, Ordering::Release);
    }
}

struct Ent {
    header: Vec<u64>, // HEADER bytes, 8-byte aligned
    payload: Vec<u8>,
    iov: Box<[libc::iovec; 2]>,
}

impl Ent {
    fn header(&mut self) -> &mut [u8] {
        // SAFETY: the u64 buffer holds HEADER bytes.
        unsafe { std::slice::from_raw_parts_mut(self.header.as_mut_ptr().cast(), HEADER) }
    }
}

fn pin(cpu: usize) {
    // SAFETY: a CPU set on the stack for this thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

fn possible_cpus() -> usize {
    std::fs::read_to_string("/sys/devices/system/cpu/possible")
        .ok()
        .and_then(|s| s.trim().rsplit(['-', ',']).next().and_then(|n| n.parse::<usize>().ok()))
        .map(|n| n + 1)
        .unwrap_or(1)
}

/// Start the ring threads; they end when the connection ends.
pub(crate) fn spawn<FS: Filesystem>(
    fs: Arc<FilesystemHolder<FS>>,
    dev: Arc<DevFuse>,
    allowed: SessionACL,
    session_owner: nix::unistd::Uid,
) -> Vec<JoinHandle<()>> {
    let n = possible_cpus();
    let mut threads = Vec::new();
    for q in 0..n {
        let (fs, dev) = (fs.clone(), dev.clone());
        let t = std::thread::Builder::new().name(format!("fuser-uring-{q}")).spawn(move || {
            if let Err(e) = queue_loop(q, fs, dev, allowed, session_owner) {
                if q == 0 {
                    log::warn!("fuse io_uring queue {q}: {e}");
                    eprintln!("fuser: io_uring queue {q} ended: {e}");
                }
            }
        });
        if let Ok(t) = t {
            threads.push(t);
        }
    }
    threads
}

fn queue_loop<FS: Filesystem>(
    q: usize,
    fs: Arc<FilesystemHolder<FS>>,
    dev: Arc<DevFuse>,
    allowed: SessionACL,
    session_owner: nix::unistd::Uid,
) -> io::Result<()> {
    pin(q);
    let payload = PAYLOAD.load(Ordering::Relaxed).max(1 << 20);
    let mut ring = Ring::new(8)?;
    let devfd = dev.as_raw_fd();
    let mut ents: Vec<Ent> = (0..ENTRIES)
        .map(|_| {
            let mut e = Ent { header: vec![0u64; HEADER / 8], payload: vec![0u8; payload], iov: Box::new([libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; 2]) };
            e.iov[0] = libc::iovec { iov_base: e.header.as_mut_ptr().cast(), iov_len: HEADER };
            e.iov[1] = libc::iovec { iov_base: e.payload.as_mut_ptr().cast(), iov_len: payload };
            e
        })
        .collect();
    for (i, e) in ents.iter().enumerate() {
        ring.push_cmd(devfd, CMD_REGISTER, e.iov.as_ptr() as u64, i as u64, 0, q as u16);
    }
    let se = SessionEventLoop { thread_name: format!("fuser-uring-{q}"), ch: Channel::new(dev.clone()), filesystem: fs, allowed, session_owner, pause: None };
    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut done = Vec::new();
    let mut buf: Vec<u8> = Vec::with_capacity(payload + IN_HEADER + OP_IN);
    loop {
        ring.submit_and_wait()?;
        done.clear();
        ring.reap(&mut done);
        for &(idx, res) in &done {
            let i = idx as usize;
            if res < 0 {
                return Err(io::Error::from_raw_os_error(-res));
            }
            let e = &mut ents[i];
            let h = e.header();
            let len = u32::from_ne_bytes(h[0..4].try_into().unwrap()) as usize;
            let unique = u64::from_ne_bytes(h[8..16].try_into().unwrap());
            let commit_id = u64::from_ne_bytes(h[ENT_IN_OUT + 8..ENT_IN_OUT + 16].try_into().unwrap());
            let psz = u32::from_ne_bytes(h[ENT_IN_OUT + 16..ENT_IN_OUT + 20].try_into().unwrap()) as usize;
            let op = len.saturating_sub(IN_HEADER + psz).min(OP_IN);
            buf.clear();
            buf.extend_from_slice(&h[..IN_HEADER]);
            buf.extend_from_slice(&h[OP_IN..OP_IN + op]);
            buf.extend_from_slice(&e.payload[..psz.min(e.payload.len())]);
            // The joined request has the length of a /dev/fuse read.
            let total = buf.len() as u32;
            buf[0..4].copy_from_slice(&total.to_ne_bytes());
            captured.lock().unwrap().clear();
            match RequestWithSender::new(ChannelSender::capture(dev.clone(), captured.clone()), &buf) {
                Some(req) => {
                    if let Ok(crate::ll::Operation::Destroy(_)) = req.request.operation() {
                        req.reply::<crate::reply::ReplyEmpty>().ok();
                    } else {
                        req.dispatch(&se);
                    }
                }
                None => {}
            }
            let out = std::mem::take(&mut *captured.lock().unwrap());
            let h = e.header();
            if out.len() >= OUT_HEADER {
                h[..OUT_HEADER].copy_from_slice(&out[..OUT_HEADER]);
                let data = &out[OUT_HEADER..];
                e.payload[..data.len()].copy_from_slice(data);
                let h = e.header();
                h[ENT_IN_OUT + 16..ENT_IN_OUT + 20].copy_from_slice(&(data.len() as u32).to_ne_bytes());
            } else {
                // No reply: an I/O error.
                h[..4].copy_from_slice(&(OUT_HEADER as u32).to_ne_bytes());
                h[4..8].copy_from_slice(&(-libc::EIO).to_ne_bytes());
                h[8..16].copy_from_slice(&unique.to_ne_bytes());
                h[ENT_IN_OUT + 16..ENT_IN_OUT + 20].copy_from_slice(&0u32.to_ne_bytes());
            }
            ring.push_cmd(devfd, CMD_COMMIT_AND_FETCH, 0, idx, commit_id, q as u16);
        }
    }
}
