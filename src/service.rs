//! Service client and daemon-version negotiation. The command process host lives in `cmd::service_host`.
//!
//! A client sends its arguments, working folder and environment over a Unix socket, with its
//! standard input, output and error and an open handle of its working folder as `SCM_RIGHTS`.
//! The service forks one process per request (the parent stays single threaded), takes the
//! caller's uid from `SO_PEERCRED`, runs the command with the caller's streams and returns the
//! exit code. Commands check the caller against line owners and the project admin.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

pub(crate) const REQUEST_TIMEOUT_SECONDS: u64 = 10;

pub fn socket_path() -> PathBuf {
    std::env::var_os("LAYR_SOCKET").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(if cfg!(target_os = "macos") { "/var/run/layr/layr.sock" } else { "/run/layr/layr.sock" })
    })
}

/// Fields follow the sorted JSON object order used by the original Value encoding.
#[derive(serde::Serialize)]
struct CommandRequest<'a> {
    argv: &'a [String],
    cwd: &'a Path,
    env: &'a BTreeMap<String, String>,
    version: CommandPeer<'a>,
}

#[derive(serde::Serialize)]
struct CommandPeer<'a> {
    build: &'a str,
    format: u32,
    schema: layr_api::SchemaVersion,
    version: &'a str,
}

impl<'a> From<&'a layr_api::Peer> for CommandPeer<'a> {
    fn from(peer: &'a layr_api::Peer) -> Self {
        Self { build: &peer.version.build, format: peer.version.format, schema: peer.schema, version: &peer.version.version }
    }
}

pub(crate) fn command_frame(argv: &[String], cwd: &Path, env: &BTreeMap<String, String>, peer: &layr_api::Peer) -> Result<Vec<u8>> {
    let mut data = vec![0; std::mem::size_of::<u32>()];
    serde_json::to_writer(&mut data, &CommandRequest { argv, cwd, env, version: peer.into() })?;
    let len = (data.len() - std::mem::size_of::<u32>()) as u32;
    data[..std::mem::size_of::<u32>()].copy_from_slice(&len.to_be_bytes());
    Ok(data)
}



pub(crate) fn send_with_fds(sock: &UnixStream, data: &[u8], fds: &[BorrowedFd]) -> Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(fds)) {
        bail!("too many descriptors");
    }
    #[cfg(target_os = "linux")]
    let flags = SendFlags::NOSIGNAL;
    #[cfg(target_os = "macos")]
    let flags = SendFlags::empty();
    let n = rustix::net::sendmsg(sock, &[std::io::IoSlice::new(data)], &mut control, flags)?;
    if n < data.len() {
        (&*sock).write_all(&data[n..])?;
    }
    Ok(())
}

pub(crate) fn recv_with_fds(sock: &UnixStream, buf: &mut [u8]) -> Result<(usize, Vec<OwnedFd>)> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    #[cfg(target_os = "linux")]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = RecvFlags::empty();
    let r = rustix::net::recvmsg(sock, &mut [std::io::IoSliceMut::new(buf)], &mut control, flags)?;
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(it) = m {
            fds.extend(it);
        }
    }
    #[cfg(target_os = "macos")]
    for fd in &fds {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }
    Ok((r.bytes, fds))
}

/// The client's error when the service closes the connection without an exit code.
const ENDED: &str = "the layr service closed the connection before the command finished: the service stopped, or the \
     request process failed (see the log of the layr service)";

/// Client: run a command through the service. Returns the exit code.
pub fn forward(argv: &[String]) -> Result<i32> {
    let path = socket_path();
    let sock = UnixStream::connect(&path).map_err(|e| connect_error(&path, &e))?;
    negotiate(&sock).context("cannot send the request to the layr service")?;
    let cwd = std::env::current_dir()?;
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let data = command_frame(argv, &cwd, &env, &crate::store_version::peer()?)?;
    let dir = std::fs::File::open(&cwd)?;
    // The service answers "B" at once when it is busy, or "R" and the exit code at the end. A
    // busy service may close the connection before it reads the request, so the answer is read
    // even when sending failed; without an answer, the send error is the reason.
    let sent = send_with_fds(
        &sock,
        &data,
        &[std::io::stdin().as_fd(), std::io::stdout().as_fd(), std::io::stderr().as_fd(), dir.as_fd()],
    );
    let mut kind = [0u8; 1];
    if (&sock).read_exact(&mut kind).is_err() {
        sent.context("cannot send the request to the layr service")?;
        bail!("{ENDED}");
    }
    if kind[0] == b'B' {
        bail!("the layr service is busy (too many running requests); try again");
    }
    sent.context("cannot send the request to the layr service")?;
    if kind[0] != b'R' {
        bail!("unexpected answer from the layr service");
    }
    let mut code = [0u8; 4];
    (&sock).read_exact(&mut code).context(ENDED)?;
    Ok(i32::from_be_bytes(code))
}

/// Exchange versions on the same connection before a client sends a command or descriptors.
pub(crate) fn negotiate(sock: &UnixStream) -> Result<()> {
    // A busy daemon writes "B" and closes at once. macOS then refuses the timeout on the closed
    // socket, and the write fails too. The answer is still read; a closed socket never waits.
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECONDS)));
    let request = br#"{"hello":true}"#;
    let mut packet = (request.len() as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(request);
    // The write error is reported only when no answer is queued.
    let sent = (&*sock).write_all(&packet);
    let mut kind = [0];
    if let Err(e) = (&*sock).read_exact(&mut kind) {
        sent?;
        return Err(e).context("the daemon did not return its store format");
    }
    if kind[0] == b'B' {
        bail!("the layr service is busy (too many running requests); try again");
    }
    sent?;
    if kind[0] != b'V' {
        bail!(
            "incompatible daemon: unknown store format (unguarded layr); this binary uses {}. upgrade both the CLI and the daemon",
            crate::store_version::description(&crate::store_version::binary())
        );
    }
    let mut length = [0; 4];
    (&*sock).read_exact(&mut length)?;
    let size = u32::from_be_bytes(length) as usize;
    if size > 4096 {
        bail!("invalid daemon version response");
    }
    let mut bytes = vec![0; size];
    (&*sock).read_exact(&mut bytes)?;
    let daemon = serde_json::from_slice::<layr_api::Peer>(&bytes)
        .context("incompatible daemon negotiation (a valid database schema is required); upgrade both the CLI and the daemon")?;
    crate::store_version::require_peer_compatible(&crate::store_version::peer()?, &daemon, "daemon")?;
    // Commands can take longer than negotiation.
    sock.set_read_timeout(None)?;
    Ok(())
}

/// What a daemon on a socket answers to a hello. A hello sends no request.
pub enum Hello {
    /// No daemon accepts on the socket (none, stale socket file, or refused).
    Absent,
    /// A daemon accepted but gave no version reply (busy or closing).
    Busy,
    /// A daemon. `root` is its store root; None from a build that does not report it. `version`
    /// is its binary version; None when the reply does not parse.
    Running { root: Option<PathBuf>, version: Option<crate::store_version::Version> },
}

/// The socket of the daemon of this user: the user socket on macOS (LaunchAgent), the service
/// socket elsewhere. The same rules as `daemon`.
pub fn user_daemon_socket() -> PathBuf {
    if cfg!(target_os = "macos") && std::env::var_os("LAYR_SOCKET").is_none() {
        return PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Caches/layr/layr.sock");
    }
    socket_path()
}

/// Ask the daemon on `path` for its store root (the reply of `handle`, read as `negotiate` does).
pub fn hello(path: &Path) -> Hello {
    let Ok(sock) = UnixStream::connect(path) else { return Hello::Absent };
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let request = br#"{"hello":true}"#;
    let mut packet = (request.len() as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(request);
    // A busy daemon closes at once, so the write may fail; its answer is still read.
    let _ = (&sock).write_all(&packet);
    let mut kind = [0u8; 1];
    if (&sock).read_exact(&mut kind).is_err() || kind[0] != b'V' {
        return Hello::Busy;
    }
    let mut length = [0u8; 4];
    if (&sock).read_exact(&mut length).is_err() {
        return Hello::Busy;
    }
    let size = u32::from_be_bytes(length) as usize;
    let mut bytes = vec![0u8; size.min(4096)];
    if size > 4096 || (&sock).read_exact(&mut bytes).is_err() {
        return Hello::Busy;
    }
    let reply = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default();
    let root = reply["root"].as_str().map(PathBuf::from);
    let version = serde_json::from_value(reply).ok();
    Hello::Running { root, version }
}

/// Why a client could not connect, and what to do. The service opens its socket to every local
/// user (mode 0666 in a 0755 folder) and decides each request by the caller's uid; so a refused
/// connection comes from the client's side or from a missing service, never from a role.
pub(crate) fn connect_error(path: &Path, e: &std::io::Error) -> anyhow::Error {
    let at = path.display();
    match e.raw_os_error() {
        // connect() itself refused by a system call filter (seccomp) or a sandbox profile: the
        // service never saw the request. File permissions give EACCES, not EPERM.
        Some(code) if code == rustix::io::Errno::PERM.raw_os_error() => anyhow!(
            "cannot connect to the layr service at {at} ({e}). The socket is open to every user; the \
             connect() system call was refused before it reached the service, usually by a sandbox or \
             seccomp filter of this process. Run layr outside that sandbox, or allow connect() to {at}."
        ),
        Some(code) if code == rustix::io::Errno::ACCESS.raw_os_error() => anyhow!(
            "cannot connect to the layr service at {at} ({e}). This user may not open the socket or a \
             folder above it; the service makes the socket mode 0666 in a folder of mode 0755. Check \
             those modes, or a security module (AppArmor, SELinux) that limits this process."
        ),
        Some(code)
            if code == rustix::io::Errno::NOENT.raw_os_error() || code == rustix::io::Errno::CONNREFUSED.raw_os_error() =>
        {
            anyhow!(
                "cannot reach the layr service at {at} ({e}). Start it as root with 'layr daemon', set \
                 LAYR_SOCKET to its socket, or run layr as root. A sandbox that hides {at} gives the \
                 same error."
            )
        }
        _ => anyhow!("cannot reach the layr service at {at} ({e}). Start it as root with 'layr daemon', or run layr as root."),
    }
}

