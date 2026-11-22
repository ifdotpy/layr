use std::io;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use nix::errno::Errno;

use crate::dev_fuse::DevFuse;
use crate::passthrough::BackingId;

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug, Clone)]
pub(crate) struct Channel(Arc<DevFuse>);

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Channel {
    /// The device of this channel (layr patch: io_uring).
    pub(crate) fn dev(&self) -> Arc<DevFuse> {
        self.0.clone()
    }

    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<DevFuse>) -> Self {
        Self(device)
    }

    /// Receives data up to the capacity of the given buffer (can block).
    pub(crate) fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        nix::unistd::read(&self.0, buffer)
    }

    /// layr patch: reads of a pausable reader do not block.
    pub(crate) fn set_nonblocking(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let fd = self.0.as_raw_fd();
        // SAFETY: fcntl on our descriptor.
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// layr patch: whether a request waits, within `ms`.
    pub(crate) fn wait_readable(&self, ms: i32) -> nix::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut p = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r < 0 {
            let e = Errno::last();
            return if e == Errno::EINTR { Ok(false) } else { Err(e) };
        }
        if p.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return Err(Errno::ENODEV);
        }
        Ok(r > 0)
    }

    /// layr patch: one read.
    pub(crate) fn receive_once(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        nix::unistd::read(&self.0, buffer)
    }

    /// Receives data up to the capacity of the given buffer (can block),
    /// retrying on errors that are safe to retry (ENOENT, EINTR, EAGAIN).
    ///
    /// - ENOENT: Operation interrupted. According to FUSE, this is safe to retry.
    /// - EINTR: Interrupted system call, retry.
    /// - EAGAIN: Explicitly instructed to try again.
    pub(crate) fn receive_retrying(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        loop {
            match self.receive(buffer) {
                Ok(size) => return Ok(size),
                Err(Errno::ENOENT | Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        ChannelSender(self.0.clone(), None)
    }

    /// Clone the FUSE device fd using FUSE_DEV_IOC_CLONE ioctl.
    ///
    /// This creates a new fd that can read FUSE requests independently,
    /// enabling true parallel request processing. The kernel distributes
    /// requests across all cloned fds.
    ///
    /// Requires Linux 4.5+. Returns an error on older kernels or non-Linux.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_fd(&self) -> io::Result<Channel> {
        use std::os::fd::AsRawFd;

        let new_dev = DevFuse::open()?;

        let mut source_fd = self.0.as_raw_fd() as u32;
        // SAFETY: fuse_dev_ioc_clone is a valid ioctl for /dev/fuse
        unsafe {
            crate::ll::ioctl::fuse_dev_ioc_clone(new_dev.as_raw_fd(), &mut source_fd)?;
        }

        Ok(Channel::new(Arc::new(new_dev)))
    }
}

/// Sends replies: to the device, or (layr patch: io_uring) into a buffer that the ring thread
/// copies to its ring entry.
#[derive(Clone, Debug)]
pub(crate) struct ChannelSender(Arc<DevFuse>, Option<Arc<std::sync::Mutex<Vec<u8>>>>);

impl ChannelSender {
    pub(crate) fn capture(dev: Arc<DevFuse>, buf: Arc<std::sync::Mutex<Vec<u8>>>) -> Self {
        ChannelSender(dev, Some(buf))
    }

    pub(crate) fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        if let Some(c) = &self.1 {
            let mut c = c.lock().unwrap();
            for b in bufs {
                c.extend_from_slice(b);
            }
            return Ok(());
        }
        let rc = nix::sys::uio::writev(&self.0, bufs)?;
        // writev is atomic, so do not need to check how many bytes are written.
        // libfuse does not do it either
        // https://github.com/libfuse/libfuse/blob/6278995cca991978abd25ebb2c20ebd3fc9e8a13/lib/fuse_lowlevel.c#L267
        debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc);
        Ok(())
    }

    pub(crate) fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        BackingId::create(&self.0, fd)
    }

    pub(crate) fn dev_fd(&self) -> &DevFuse {
        &self.0
    }

    pub(crate) unsafe fn wrap_backing(&self, id: u32) -> BackingId {
        unsafe { BackingId::wrap_raw(&self.0, id) }
    }
}
