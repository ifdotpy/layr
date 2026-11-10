//! The root service (`layr daemon`) and the client side of `layr` for other users.
//!
//! A client sends its arguments, working folder and environment over a Unix socket, with its
//! standard input, output and error and an open handle of its working folder as `SCM_RIGHTS`.
//! The service forks one process per request (the parent stays single threaded), takes the
//! caller's uid from `SO_PEERCRED`, runs the command with the caller's streams and returns the
//! exit code. Commands check the caller against line owners and the project admin.

use crate::ctx::{Caller, Ctx};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

pub fn socket_path() -> PathBuf {
    std::env::var_os("LAYR_SOCKET").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(if cfg!(target_os = "macos") { "/var/run/layr/layr.sock" } else { "/run/layr/layr.sock" })
    })
}

const MAX_REQ_BYTES: usize = 4 << 20;
const REQUEST_BUFFER_BYTES: usize = 64 << 10;
const MAX_DESCRIPTORLESS_REQUEST_BYTES: usize = 32 << 10;
const REQUEST_TIMEOUT_SECONDS: u64 = 10;
const DEFAULT_MAX_REQUESTS: usize = 256;
const DEFAULT_MAX_REQUESTS_PER_USER: usize = 32;

/// Commands and the descriptorless hello/registry messages share a frame. Command fields
/// remain optional here for those control messages and are required before command dispatch.
#[derive(serde::Deserialize)]
struct Request {
    argv: Option<Vec<String>>,
    // The directory descriptor is authoritative; older clients omitted this hint.
    #[serde(rename = "cwd")]
    _cwd: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_client_peer")]
    version: Option<layr_api::Peer>,
    #[serde(default)]
    hello: bool,
    registry: Option<serde_json::Value>,
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

fn command_frame(argv: &[String], cwd: &Path, env: &BTreeMap<String, String>, peer: &layr_api::Peer) -> Result<Vec<u8>> {
    let mut data = vec![0; std::mem::size_of::<u32>()];
    serde_json::to_writer(&mut data, &CommandRequest { argv, cwd, env, version: peer.into() })?;
    let len = (data.len() - std::mem::size_of::<u32>()) as u32;
    data[..std::mem::size_of::<u32>()].copy_from_slice(&len.to_be_bytes());
    Ok(data)
}

/// Keep a missing/invalid schema diagnostic on the caller's stderr through typed decoding.
fn deserialize_client_peer<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<layr_api::Peer>, D::Error> {
    <Option<layr_api::Peer> as serde::Deserialize>::deserialize(deserializer).map_err(|error| {
        serde::de::Error::custom(format!(
            "incompatible client negotiation (a valid database schema is required); upgrade both the CLI and the daemon: {error}"
        ))
    })
}

fn send_with_fds(sock: &UnixStream, data: &[u8], fds: &[BorrowedFd]) -> Result<()> {
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

fn recv_with_fds(sock: &UnixStream, buf: &mut [u8]) -> Result<(usize, Vec<OwnedFd>)> {
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
fn negotiate(sock: &UnixStream) -> Result<()> {
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
fn connect_error(path: &Path, e: &std::io::Error) -> anyhow::Error {
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

#[cfg(target_os = "linux")]
fn peer_uid(sock: &UnixStream) -> Result<(u32, u32, i32)> {
    let c = rustix::net::sockopt::socket_peercred(sock)?;
    Ok((c.uid.as_raw(), c.gid.as_raw(), c.pid.as_raw_nonzero().get()))
}

fn limit(var: &str, default: usize) -> usize {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Read one request. `None`: the client closed the connection without sending a byte, which is a
/// normal end of the connection, not a bad request.
fn read_request(sock: &UnixStream, buffer: &mut Vec<u8>) -> Result<Option<Vec<OwnedFd>>> {
    buffer.resize(REQUEST_BUFFER_BYTES, 0);
    let (mut filled, mut fds) = recv_with_fds(sock, buffer)?;
    if filled == 0 {
        return Ok(None);
    }
    while filled < 4 {
        let (n, more_fds) = recv_with_fds(sock, &mut buffer[filled..4])?;
        fds.extend(more_fds);
        if n == 0 {
            bail!("bad request");
        }
        filled += n;
    }
    let len = u32::from_be_bytes(buffer[..4].try_into()?) as usize;
    if len > MAX_REQ_BYTES || (fds.is_empty() && len > MAX_DESCRIPTORLESS_REQUEST_BYTES) {
        bail!("request too large");
    }
    let frame_bytes = len + 4;
    if filled > frame_bytes {
        bail!("request exceeds its declared size");
    }
    buffer.resize(frame_bytes, 0);
    while filled < frame_bytes {
        let n = (&*sock).read(&mut buffer[filled..])?;
        if n == 0 {
            bail!("short request");
        }
        filled += n;
    }
    Ok(Some(fds))
}

fn decode_request(bytes: &[u8], fds: Vec<OwnedFd>) -> Result<(Request, Vec<OwnedFd>)> {
    // Decode errors belong on the caller's stderr, too. This runs only in the request child.
    if fds.len() == 4 {
        rustix::stdio::dup2_stdin(&fds[0])?;
        rustix::stdio::dup2_stdout(&fds[1])?;
        rustix::stdio::dup2_stderr(&fds[2])?;
    }
    let request = serde_json::from_slice(&bytes[4..]).context("invalid request")?;
    Ok((request, fds))
}

/// Child process: serve one request, then exit.
fn handle(sock: UnixStream, root: &Path, registry_uids: &[u32]) -> i32 {
    let res = (|| -> Result<i32> {
        let (uid, _gid, _pid) = peer_uid(&sock)?;
        // A client that does not send its request in time does not hold a process.
        sock.set_read_timeout(Some(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECONDS)))?;
        let mut buffer = Vec::new();
        let Some(fds) = read_request(&sock, &mut buffer)? else {
            return Ok(0);
        };
        let (v, fds) = decode_request(&buffer, fds)?;
        let (v, fds) = if v.hello {
            if !fds.is_empty() {
                bail!("the version request must not contain descriptors");
            }
            // The store root lets a client see which store this daemon serves (layr follow).
            let mut hello = serde_json::to_value(crate::store_version::peer()?)?;
            hello["root"] = root.to_string_lossy().into_owned().into();
            let bytes = serde_json::to_vec(&hello)?;
            let mut packet = vec![b'V'];
            packet.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            packet.extend_from_slice(&bytes);
            (&sock).write_all(&packet)?;
            // A client that reads the version and closes sends no command: that is not an error.
            let Some(fds) = read_request(&sock, &mut buffer)? else {
                return Ok(0);
            };
            decode_request(&buffer, fds)?
        } else {
            (v, fds)
        };
        if let Some(query) = v.registry {
            if !fds.is_empty() {
                bail!("registry request must not contain descriptors");
            }
            crate::oci::registry::serve_backend(sock.try_clone()?, root, uid, query)?;
            return Ok(0);
        }
        if registry_uids.contains(&uid) {
            bail!("registry user may use only registry read requests");
        }
        if fds.len() != 4 {
            bail!("bad request: expected 4 descriptors");
        }
        let argv = v.argv.context("bad request: missing argv")?;
        let env = v.env.context("bad request: missing env")?;
        let client = v.version.with_context(|| {
            format!(
                "incompatible client: unknown store format (unguarded layr); daemon uses {}. upgrade both the CLI and the daemon",
                crate::store_version::description(&crate::store_version::binary())
            )
        })?;
        crate::store_version::require_peer_compatible(&crate::store_version::peer()?, &client, "client")?;
        rustix::process::fchdir(&fds[3]).context("cannot enter the working folder")?;
        drop(fds);
        let cwd = std::env::current_dir()?;
        let command = argv.join(" ");
        // A panic prints its message to the caller's stderr; the caller still gets an exit code.
        let run = std::panic::AssertUnwindSafe(|| run_as(argv, Caller::from_uid(uid), cwd, env, root));
        Ok(std::panic::catch_unwind(run).unwrap_or_else(|_| {
            eprintln!("layr service: `layr {command}` stopped on an internal error (a panic). Please report it.");
            101
        }))
    })();
    let code = match res {
        Ok(c) => c,
        Err(e) => {
            eprintln!("layr service: {e:#}");
            128
        }
    };
    let mut reply = vec![b'R'];
    reply.extend_from_slice(&code.to_be_bytes());
    let _ = (&sock).write_all(&reply);
    code
}

/// Run a command for a caller and return its exit code (errors are printed to stderr).
pub fn run_as(argv: Vec<String>, caller: Caller, cwd: PathBuf, env: HashMap<String, String>, root: &Path) -> i32 {
    crate::trace::set_request_enabled(env.contains_key("LAYR_TRACE"));
    let _trace = crate::trace::span("command");
    crate::repo::begin_command();
    crate::store::set_holder_command(&argv);
    crate::store::panic_point(argv.first().map_or("", String::as_str));
    let store = match Store::open(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: cannot open the layr store {}: {e:#}", root.display());
            return 128;
        }
    };
    let started = std::time::Instant::now();
    let out: Box<dyn Write> = Box::new(std::io::BufWriter::new(std::io::stdout()));
    let ctx = Ctx { store, caller, cwd, env, author: Default::default(), out: std::cell::RefCell::new(out) };
    let r = crate::cmd::run(&ctx, &argv);
    let _ = ctx.out.borrow_mut().flush();
    let code = match r {
        Ok(c) => c,
        Err(e) => {
            if let Some(x) = e.downcast_ref::<crate::cmd::Exit>() {
                if !x.1.is_empty() {
                    eprintln!("{}", x.1);
                }
                x.0
            } else {
                eprintln!("fatal: {e:#}");
                if disk_full(&e) {
                    eprintln!("{}", disk_full_hint(root));
                }
                128
            }
        }
    };
    // The free step: after the commit and the store lock (docs/ownership.md).
    {
        let _t = crate::trace::span("free finish");
        crate::free::finish(&ctx);
    }
    let agent = ctx.session().is_some();
    crate::usage::count(&ctx.store.root, &argv, ctx.caller.uid, agent, code, started.elapsed().as_millis() as u64);
    code
}

/// True when `e` comes from a full disk: ENOSPC, or SQLite that cannot grow its file or its shared
/// memory (a few MB free are enough for neither).
fn disk_full(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        if let Some(io) = c.downcast_ref::<std::io::Error>() {
            return io.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error());
        }
        if let Some(rusqlite::Error::SqliteFailure(f, _)) = c.downcast_ref::<rusqlite::Error>() {
            // SQLITE_IOERR_SHMSIZE: the shared memory of the write-ahead log cannot grow.
            return f.code == rusqlite::ErrorCode::DiskFull || f.extended_code == 4874;
        }
        false
    })
}

/// What to do when the disk of the store is full, with its free space.
fn disk_full_hint(root: &Path) -> String {
    let free = rustix::fs::statvfs(root).map(|s| s.f_bavail.saturating_mul(s.f_frsize) / (1 << 20)).ok();
    let free = free.map(|mb| format!(" ({mb} MB free)")).unwrap_or_default();
    format!(
        "hint: the disk of the layr store {} is full{free}. Free some space first (layr needs about 20 MB to run a command), then run the command again; 'layr gc --repair' frees old caches.",
        root.display()
    )
}

/// The time of the last periodic check of each line that found no change, in the project's meta
/// folder. It is local to this machine and is never sent to other machines.
const PERIODIC_CHECKS: &str = "periodic-checks.json";

/// Periodic work: automatic states of busy lines, access to the git store, person requests and the queue. No repair:
/// the operations that make data dead free it, and the daily garbage report (`garbage_report`) names a gap. A user
/// daemon (`user_mode`, the macOS LaunchAgent) runs the requests and the queue of its own store as its user; the
/// automatic states and the git store access stay with the root service.
fn periodic(root: &Path, hourly: bool, user_mode: Option<u32>, manual_queue: bool) {
    let now = crate::ids::now_ms();
    let store = match Store::open(root) {
        Ok(s) => s,
        Err(_) => return,
    };
    let caller = Caller::from_uid(user_mode.unwrap_or(0));
    let names = store.project_names().unwrap_or_default();
    drop(store);
    for name in names {
        let store = match Store::open(root) {
            Ok(s) => s,
            Err(_) => return,
        };
        let ctx = Ctx {
            store,
            caller: caller.clone(),
            cwd: PathBuf::from("/"),
            env: HashMap::new(),
            author: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let repo = match crate::repo::Repo::open(&ctx, &name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if user_mode.is_none() {
            automatic_states(&ctx, &repo, &name, now);
        }
        // Person requests (approve, comment, queue add) of `requests.remote`: before the queue pass, so a queued
        // entry merges on the same tick.
        if !manual_queue {
            match crate::cmd::requests::daemon_remote(&repo) {
                Ok(Some(remote)) => {
                    if let Err(error) = crate::cmd::requests::apply(&ctx, &repo, &remote, true, true, |_| Ok(())) {
                        eprintln!("layr requests ({name}): {error:#}");
                    }
                }
                Ok(None) => {}
                Err(error) => eprintln!("layr requests ({name}): {error:#}"),
            }
            if let Err(error) = crate::cmd::queue::run_configured(&ctx, &repo, None) {
                eprintln!("layr queue ({name}): {error:#}");
            }
        }
        if hourly && user_mode.is_none() {
            // Also repairs the git store of a project from an earlier layr, soon after a start.
            if let Ok(_lock) = repo.p.lock() {
                let _ = crate::gitbridge::sync_store_access(&repo);
            }
        }
    }
}

/// The automatic states of the busy lines of project `name` (`snapshot.interval`).
fn automatic_states(ctx: &Ctx, repo: &crate::repo::Repo, name: &str, now: i64) {
    {
        let interval = repo.p.config_u64("snapshot.interval", 600) as i64 * 1000;
        // A check that found no change saves no state, so the time of the last automatic state
        // stays old. The time of the check keeps an idle line from a snapshot and a comparison
        // on every tick.
        let checks_path = repo.p.meta_dir().join(PERIODIC_CHECKS);
        let mut checks: HashMap<String, i64> =
            std::fs::read(&checks_path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default();
        let before = checks.clone();
        if let Ok(lines) = repo.lines() {
            checks.retain(|id, _| lines.iter().any(|l| &l.id == id));
            for l in lines {
                if l.machine != repo.p.store.machine.id || !repo.working(&l).exists() {
                    continue;
                }
                let saved = repo.last_auto(&l).ok().flatten().map(|s| s.time).unwrap_or(0);
                let last = saved.max(checks.get(&l.id).copied().unwrap_or(0));
                if now - last >= interval {
                    if let Ok(_lock) = repo.p.lock() {
                        match repo.save(&ctx, &l, "periodic", true, false) {
                            Ok(None) => {
                                checks.insert(l.id.clone(), now);
                            }
                            Ok(Some(_)) => {}
                            // A failed save is compared again on the next tick: say why.
                            Err(e) => eprintln!("layr periodic save ({name}/{}): {e:#}", l.name),
                        }
                    }
                }
            }
        }
        if checks != before {
            if let Ok(data) = serde_json::to_vec(&checks) {
                let _ = crate::fsutil::atomic_write(&checks_path, &data, 0o600);
            }
        }
    }
}

/// Retry trash left by a failed background start, on every available volume.
fn sweep_trash(root: &Path) {
    sweep_trash_with(root, crate::store::spawn_trash_emptier);
}

fn sweep_trash_with(root: &Path, start: impl Fn(&Path) -> bool) {
    let layout = match crate::volumes::Layout::for_root(root) {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("layr trash sweep: {error:#}");
            return;
        }
    };
    for name in layout.names() {
        if let Ok(path) = layout.trash_of(&name) {
            if !start(&path) {
                eprintln!("layr trash sweep ({name}): cannot start trash emptier");
            }
        }
    }
}

/// The daily garbage report: for each project of the store at `root`, one line in the service log when
/// `layr gc` or `layr fsck --caches` would find something. It changes nothing: the operations that make data dead
/// free it, and a report names a gap in them.
fn garbage_report(root: &Path, caller: Caller) {
    let Ok(store) = Store::open(root) else { return };
    let names = store.project_names().unwrap_or_default();
    drop(store);
    for name in names.into_iter().filter(|n| !n.contains('%')) {
        let Ok(store) = Store::open(root) else { return };
        let ctx = Ctx {
            store,
            caller: caller.clone(),
            cwd: PathBuf::from("/"),
            env: HashMap::new(),
            author: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let Ok(repo) = crate::repo::Repo::open(&ctx, &name) else { continue };
        match crate::cmd::admin::garbage_report(&repo) {
            Ok(Some(found)) => eprintln!("layr garbage report ({name}): {found}"),
            Ok(None) => {}
            Err(e) => eprintln!("layr garbage report ({name}): {e:#}"),
        }
    }
}

/// The socket of a daemon that runs as a user (macOS LaunchAgent): in the user's caches folder.
fn user_socket_path() -> PathBuf {
    std::env::var_os("LAYR_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Caches/layr/layr.sock"))
}

/// `layr daemon [--socket <path>] [--tick <seconds>] [--manual-queue]`. Root serves every local user (Linux, or
/// macOS run as root). On macOS a daemon may run as its user (a LaunchAgent): it serves only that
/// uid, switches no user and runs no periodic saves, only the follow tick and the daily garbage report.
/// How long a stopping service waits for its running requests. The service managers allow more
/// (`TimeoutStopSec` of packaging/systemd/layr.service, `ExitTimeOut` of the launchd plist).
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(80);

pub fn daemon(args: &[String]) -> Result<i32> {
    crate::pools::mark_forking_parent();
    let own = rustix::process::geteuid().as_raw();
    let user_mode = own != 0 && cfg!(target_os = "macos");
    if own != 0 && !user_mode {
        bail!("layr daemon must run as root");
    }
    let mut sock: Option<PathBuf> = None;
    let mut tick = if user_mode { 120u64 } else { 60u64 };
    let mut manual_queue = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                sock = Some(PathBuf::from(args.get(i + 1).ok_or_else(|| anyhow!("--socket needs a path"))?));
                i += 1;
            }
            "--tick" => {
                tick = args.get(i + 1).ok_or_else(|| anyhow!("--tick needs seconds"))?.parse()?;
                i += 1;
            }
            "--manual-queue" => manual_queue = true,
            x => bail!("unknown option {x}"),
        }
        i += 1;
    }
    let sock = sock.unwrap_or_else(|| if user_mode { user_socket_path() } else { socket_path() });
    let root = crate::store::default_root();
    #[cfg(target_os = "macos")]
    if !user_mode {
        let existing = root.ancestors().find(|p| p.exists()).context("store has no existing parent")?;
        if !crate::sys::apfs_owners_enabled(existing)? {
            bail!("the APFS service requires a volume with ownership enabled (diskutil enableOwnership <volume>)");
        }
        use std::os::unix::fs::MetadataExt;
        let mut paths = vec![root.clone(), root.join("machine"), root.join("projects")];
        // The projects of the other volumes are store folders too.
        paths.extend(crate::volumes::Layout::for_root(&root)?.ready_projects_dirs().into_iter().map(|(_, p)| p));
        for path in &paths {
            if let Ok(m) = std::fs::symlink_metadata(path) {
                if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
                    bail!(
                        "the APFS service requires root-owned store directories without group or public write access: {}",
                        path.display()
                    );
                }
            }
        }
    }
    let _store_guard = Store::open(&root)?;
    if let Some(d) = sock.parent() {
        std::fs::create_dir_all(d)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
    }
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            bail!("another layr service is running on {}", sock.display());
        }
        std::fs::remove_file(&sock)?;
    }
    // The OCI registry serves other users: a user daemon has none.
    let registry_listeners = if user_mode { Vec::new() } else { crate::oci::registry::listeners(&root)? };
    let registry_uids: Vec<u32> = registry_listeners.iter().map(|(_, _, user)| user.uid).collect();
    let listener = UnixListener::bind(&sock)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(if user_mode { 0o600 } else { 0o666 }))?;
    }
    crate::sys::catch_terminate()?;
    eprintln!("layr service: listening on {} (store {})", sock.display(), root.display());
    let mut last_hourly: i64 = 0;
    let mut last_daily: i64 = 0;
    let mut next = std::time::Instant::now() + std::time::Duration::from_secs(tick);
    // Running request processes, by caller: limits keep one user from exhausting the service.
    let mut children: HashMap<i32, u32> = HashMap::new();
    // One periodic pass at a time: a pass longer than the tick must not start another.
    let mut periodic_child: Option<i32> = None;
    // The check runner of a Layr Cloud gateway (src/runner.rs): one long-lived child while the
    // store has runner settings. A user daemon runs it as its user; it restarts after 10 s.
    let mut runner_child: Option<i32> = None;
    let mut runner_start = std::time::Instant::now();
    // The restart delay: 10 s, doubled after each run shorter than a minute, at most 60 s.
    let mut runner_delay = 10u64;
    let mut runner_started = std::time::Instant::now();
    let (max_total, max_per_user) =
        (limit("LAYR_MAX_REQUESTS", DEFAULT_MAX_REQUESTS), limit("LAYR_MAX_REQUESTS_PER_USER", DEFAULT_MAX_REQUESTS_PER_USER));
    loop {
        if crate::sys::terminate_requested() {
            // Stop: take no new request and let the running ones finish (layr#252). A service
            // manager stops the rest when the service exits.
            drop(listener);
            drop(registry_listeners);
            let _ = std::fs::remove_file(&sock);
            if let Some(pid) = runner_child.and_then(rustix::process::Pid::from_raw) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            eprintln!("layr service: stopping after {} running requests", children.len());
            let deadline = std::time::Instant::now() + STOP_WAIT;
            while std::time::Instant::now() < deadline {
                match rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG) {
                    Ok(Some((pid, _))) => _ = children.remove(&pid.as_raw_nonzero().get()),
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
            if !children.is_empty() {
                eprintln!(
                    "layr service: {} requests still run after {} s; they stop with the service",
                    children.len(),
                    STOP_WAIT.as_secs()
                );
            }
            return Ok(0);
        }
        // Reap finished children.
        while let Ok(Some((pid, _))) = rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG) {
            let pid = pid.as_raw_nonzero().get();
            children.remove(&pid);
            if periodic_child == Some(pid) {
                periodic_child = None;
            }
            if runner_child == Some(pid) {
                runner_child = None;
                runner_delay =
                    if runner_started.elapsed() < std::time::Duration::from_secs(60) { (runner_delay * 2).min(60) } else { 10 };
                runner_start = std::time::Instant::now() + std::time::Duration::from_secs(runner_delay);
            }
        }
        if user_mode && runner_child.is_none() && std::time::Instant::now() >= runner_start {
            runner_start = std::time::Instant::now() + std::time::Duration::from_secs(10);
            if root.join(crate::runner::DIR).join("config.json").exists() {
                runner_started = std::time::Instant::now();
                match crate::sys::fork() {
                    Ok(crate::sys::Fork::Child) => {
                        drop(listener);
                        let code = match crate::runner::serve(&root) {
                            Ok(()) => 0,
                            Err(e) => {
                                eprintln!("layr runner: {e:#}");
                                1
                            }
                        };
                        std::process::exit(code);
                    }
                    Ok(crate::sys::Fork::Parent(pid)) => runner_child = Some(pid),
                    Err(e) => eprintln!("layr service: fork failed: {e}"),
                }
            }
        }
        let wait = next.saturating_duration_since(std::time::Instant::now()).min(std::time::Duration::from_secs(1));
        let timeout = rustix::event::Timespec { tv_sec: wait.as_secs() as _, tv_nsec: wait.subsec_nanos() as _ };
        let mut pfd = vec![rustix::event::PollFd::new(&listener, rustix::event::PollFlags::IN)];
        for (_, l, _) in &registry_listeners {
            pfd.push(rustix::event::PollFd::new(l, rustix::event::PollFlags::IN));
        }
        let ready = rustix::event::poll(&mut pfd, Some(&timeout)).unwrap_or(0);
        if ready > 0 && pfd[0].revents().contains(rustix::event::PollFlags::IN) {
            if let Ok((conn, _)) = listener.accept() {
                let uid = peer_uid(&conn).map(|x| x.0).unwrap_or(u32::MAX);
                if user_mode && uid != own {
                    eprintln!("layr daemon: refused a request from uid {uid}");
                    drop(conn);
                    continue;
                }
                let mine = children.values().filter(|u| **u == uid).count();
                if children.len() >= max_total || (uid != 0 && mine >= max_per_user) {
                    let _ = (&conn).write_all(b"B");
                    drop(conn);
                    continue;
                }
                match crate::sys::fork() {
                    Ok(crate::sys::Fork::Child) => {
                        drop(listener);
                        let code = handle(conn, &root, &registry_uids);
                        std::process::exit(if code >= 0 { 0 } else { 1 });
                    }
                    Err(e) => eprintln!("layr service: fork failed: {e}"),
                    Ok(crate::sys::Fork::Parent(pid)) => {
                        children.insert(pid, uid);
                        drop(conn);
                    }
                }
            }
        }
        for (i, (name, registry, user)) in registry_listeners.iter().enumerate() {
            if !pfd[i + 1].revents().contains(rustix::event::PollFlags::IN) {
                continue;
            }
            if let Ok((conn, _)) = registry.accept() {
                if children.len() >= max_total {
                    drop(conn);
                    continue;
                }
                match crate::sys::fork() {
                    Ok(crate::sys::Fork::Child) => {
                        drop(listener);
                        if let Err(e) = crate::oci::registry::exec_http(conn, &sock, name, *user) {
                            eprintln!("layr registry: {e:#}");
                        }
                        std::process::exit(1);
                    }
                    Ok(crate::sys::Fork::Parent(pid)) => {
                        children.insert(pid, u32::MAX);
                    }
                    Err(e) => eprintln!("layr registry: fork failed: {e}"),
                }
            }
        }
        if std::time::Instant::now() >= next {
            next = std::time::Instant::now() + std::time::Duration::from_secs(tick);
            if periodic_child.is_some() {
                continue;
            }
            // Periodic work in a child, so the service stays responsive and single threaded.
            let now = crate::ids::now_ms();
            let hourly = now - last_hourly >= 3_600_000;
            let daily = now - last_daily >= 86_400_000;
            match crate::sys::fork() {
                Ok(crate::sys::Fork::Child) => {
                    drop(listener);
                    sweep_trash(&root);
                    let caller = Caller::from_uid(if user_mode { own } else { 0 });
                    periodic(&root, hourly, user_mode.then_some(own), manual_queue);
                    crate::follow::tick(&root, caller.clone());
                    // Dead cache states in remotes that nobody pushes to (docs/cache-overlays.md).
                    if hourly {
                        crate::store_remote::sweep_tick(&root, caller.clone());
                    }
                    if daily {
                        garbage_report(&root, caller);
                    }
                    std::process::exit(0);
                }
                Ok(crate::sys::Fork::Parent(pid)) => {
                    periodic_child = Some(pid);
                    if hourly {
                        last_hourly = now;
                    }
                    if daily {
                        last_daily = now;
                    }
                }
                Err(e) => eprintln!("layr service: fork failed: {e}"),
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn peer_uid(sock: &UnixStream) -> Result<(u32, u32, i32)> {
    let (uid, gid) = crate::sys::peer_credentials(sock)?;
    Ok((uid, gid, 0))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn socket_identity_comes_from_the_kernel() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (uid, gid, _) = super::peer_uid(&a).unwrap();
        assert_eq!(uid, rustix::process::geteuid().as_raw());
        assert_eq!(gid, rustix::process::getegid().as_raw());
    }
}

// OverlayFS requires a root-owned registry; t82 and t91 cover its trash emptier in CI.
#[cfg(all(test, not(feature = "overlayfs")))]
mod trash_sweep_tests {
    #[test]
    fn periodic_sweep_removes_deferred_trees_on_all_volumes() {
        let base = std::env::temp_dir().join(format!("layr-trash-sweep-{}", crate::ids::new_id()));
        let root = base.join("store");
        let volume = base.join("volume");
        std::fs::create_dir_all(root.join("machine")).unwrap();
        std::fs::write(root.join("machine/id"), "m1").unwrap();
        for path in [&root, &volume] {
            std::fs::create_dir_all(path.join("trash/deferred")).unwrap();
            std::fs::write(path.join("trash/deferred/file"), b"x").unwrap();
        }
        std::fs::write(volume.join("layr-volume.json"), br#"{"format":1,"store":"m1","volume":"other"}"#).unwrap();
        std::fs::write(
            root.join("volumes.json"),
            serde_json::to_vec(&serde_json::json!({
                "format": 1, "volumes": {"other": {"path": volume}}
            }))
            .unwrap(),
        )
        .unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join("trash.lock")).unwrap();
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        super::sweep_trash_with(&root, |path| crate::store::empty_trash_process(path).is_ok());
        assert!(root.join("trash/deferred").exists(), "a locked trash is left for the next sweep");
        assert!(!volume.join("trash/deferred").exists());
        // A child that another test forked meanwhile shares this lock: unlock it rather than close.
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::Unlock).unwrap();
        drop(lock);
        // Rare on the Linux CI check: the error and trash.errors say why the trash stayed.
        super::sweep_trash_with(&root, |path| {
            crate::store::empty_trash_process(path).map_err(|e| eprintln!("sweep of {}: {e:#}", path.display())).is_ok()
        });
        assert!(
            !root.join("trash/deferred").exists(),
            "trash.errors: {}",
            std::fs::read_to_string(root.join("trash.errors")).unwrap_or_default()
        );
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[cfg(test)]
mod connect_tests {
    use super::connect_error;
    use std::path::Path;

    fn msg(errno: rustix::io::Errno) -> String {
        connect_error(Path::new("/run/layr/layr.sock"), &std::io::Error::from_raw_os_error(errno.raw_os_error())).to_string()
    }

    #[test]
    fn a_refused_connect_names_its_cause() {
        // EPERM: a filter of the client refused connect(); the socket itself is open.
        assert!(msg(rustix::io::Errno::PERM).contains("sandbox or seccomp filter"));
        // EACCES: file modes or a security module.
        assert!(msg(rustix::io::Errno::ACCESS).contains("mode 0666"));
        // No socket or no listener: the service is not running here.
        assert!(msg(rustix::io::Errno::NOENT).contains("Start it as root"));
        assert!(msg(rustix::io::Errno::CONNREFUSED).contains("Start it as root"));
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    fn read_request(sock: &UnixStream) -> Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
        let mut bytes = Vec::new();
        Ok(super::read_request(sock, &mut bytes)?.map(|fds| (bytes, fds)))
    }

    #[test]
    fn typed_command_frames_match_value_encoding() -> Result<()> {
        let peer = crate::store_version::peer()?;
        for argv in [vec![], vec!["rev-parse".into(), "head".into()], vec!["line".into(), "list".into(), "\"\n\\é".into()]] {
            for env in [BTreeMap::new(), BTreeMap::from([("Z".into(), "last".into()), ("A".into(), "\n\"é".into())])] {
                let cwd = Path::new("/tmp/space and é");
                let legacy = serde_json::to_vec(&serde_json::json!({"argv": argv, "cwd": cwd, "env": env, "version": peer}))?;
                let mut expected = (legacy.len() as u32).to_be_bytes().to_vec();
                expected.extend_from_slice(&legacy);
                assert_eq!(command_frame(&argv, cwd, &env, &peer)?, expected);
            }
        }
        Ok(())
    }

    #[test]
    fn hello_and_command_reuse_frame_storage() -> Result<()> {
        let (mut client, server) = UnixStream::pair()?;
        let mut bytes = Vec::new();
        for payload in [br#"{"hello":true}"#.as_slice(), br#"{"argv":["line","list"]}"#.as_slice()] {
            client.write_all(&(payload.len() as u32).to_be_bytes())?;
            client.write_all(payload)?;
            let capacity = bytes.capacity();
            assert!(super::read_request(&server, &mut bytes)?.unwrap().is_empty());
            assert_eq!(&bytes[4..], payload);
            if capacity != 0 {
                assert_eq!(bytes.capacity(), capacity);
            }
        }
        Ok(())
    }

    #[test]
    fn fragmented_headers_and_payloads_are_accepted() -> Result<()> {
        let (mut client, server) = UnixStream::pair()?;
        server.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        let payload = br#"{"hello":true}"#;
        let length = (payload.len() as u32).to_be_bytes();
        client.write_all(&length[..1])?;
        let writer = std::thread::spawn(move || -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(20));
            client.write_all(&length[1..])?;
            for byte in payload {
                client.write_all(&[*byte])?;
            }
            Ok(())
        });
        let result = read_request(&server);
        writer.join().unwrap()?;
        let (request, fds) = result?.context("missing request")?;
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&request[4..])?, serde_json::json!({"hello": true}));
        assert!(fds.is_empty());
        Ok(())
    }

    #[test]
    fn size_limits_and_truncated_payloads_are_preserved() -> Result<()> {
        for size in [MAX_DESCRIPTORLESS_REQUEST_BYTES + 1, MAX_REQ_BYTES + 1] {
            let (mut client, server) = UnixStream::pair()?;
            client.write_all(&(size as u32).to_be_bytes())?;
            assert_eq!(read_request(&server).unwrap_err().to_string(), "request too large");
        }
        let (mut client, server) = UnixStream::pair()?;
        client.write_all(&[0, 0, 0, 10, b'{'])?;
        drop(client);
        assert_eq!(read_request(&server).unwrap_err().to_string(), "short request");
        let (mut client, server) = UnixStream::pair()?;
        client.write_all(&[0, 0, 0, 1, b'{', b'}'])?;
        assert_eq!(read_request(&server).unwrap_err().to_string(), "request exceeds its declared size");
        Ok(())
    }

    #[test]
    fn a_large_fragmented_request_retains_its_descriptors() -> Result<()> {
        let (client, server) = UnixStream::pair()?;
        server.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        let writer = std::thread::spawn(move || -> Result<()> {
            let file = std::fs::File::open("/dev/null")?;
            let size = REQUEST_BUFFER_BYTES * 2;
            send_with_fds(&client, &(size as u32).to_be_bytes(), &[file.as_fd()])?;
            for _ in 0..size / 1024 {
                (&client).write_all(&[b' '; 1024])?;
            }
            Ok(())
        });
        let (bytes, fds) = read_request(&server)?.context("missing request")?;
        writer.join().unwrap()?;
        assert_eq!(bytes.len(), REQUEST_BUFFER_BYTES * 2 + 4);
        assert_eq!(fds.len(), 1);
        assert!(!std::fs::File::from(fds.into_iter().next().unwrap()).metadata()?.is_file());
        Ok(())
    }

    // A fresh process owns stdio and cwd, just like a daemon request child. Never redirect
    // the test runner's shared descriptors or change its process environment.
    #[test]
    fn request_child() -> Result<()> {
        let Some(path) = std::env::var_os("LAYR_TEST_REQUEST_SOCKET") else { return Ok(()) };
        let socket = UnixStream::connect(path)?;
        let code = handle(socket, &crate::store::default_root(), &[]);
        std::thread::spawn(|| {
            drop(crate::trace::named(|| "request worker".into()));
            drop(crate::trace::begin("request worker begin"));
        })
        .join()
        .unwrap();
        std::process::exit(code);
    }

    fn child_request(root: &Path, env: serde_json::Value, daemon_trace: bool) -> Result<(i32, Vec<u8>, String)> {
        child_request_with_peer(root, env, daemon_trace, serde_json::to_value(crate::store_version::peer()?)?)
    }

    fn child_request_with_peer(
        root: &Path,
        env: serde_json::Value,
        daemon_trace: bool,
        peer: serde_json::Value,
    ) -> Result<(i32, Vec<u8>, String)> {
        use std::process::{Command, Stdio};
        // sockaddr_un has a short path limit on macOS. The child inherits this cwd.
        let id = crate::ids::display(&crate::ids::new_id());
        let path = std::path::PathBuf::from(format!(".layr-request-{}", &id[..16]));
        struct SocketPath(std::path::PathBuf);
        impl Drop for SocketPath {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let path = SocketPath(path);
        let listener = UnixListener::bind(&path.0)?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", "service::request_tests::request_child", "--nocapture"])
            .env("LAYR_TEST_REQUEST_SOCKET", &path.0)
            .env("LAYR_ROOT", root.join("store"))
            .env_remove("LAYR_TRACE")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if daemon_trace {
            command.env("LAYR_TRACE", "1");
        }
        let child = command.spawn()?;
        let (socket, _) = listener.accept()?;
        socket.set_read_timeout(Some(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECONDS)))?;
        negotiate(&socket)?;
        let request = serde_json::to_vec(&serde_json::json!({
            "argv": ["volume", "list", "--json"], "cwd": root, "env": env,
            "version": peer
        }))?;
        let input = std::fs::File::open("/dev/null")?;
        let output = std::fs::File::create(root.join("stdout"))?;
        let error = std::fs::File::create(root.join("stderr"))?;
        let directory = std::fs::File::open(root)?;
        let mut packet = (request.len() as u32).to_be_bytes().to_vec();
        packet.extend_from_slice(&request);
        send_with_fds(&socket, &packet, &[input.as_fd(), output.as_fd(), error.as_fd(), directory.as_fd()])?;
        let mut reply = [0; 5];
        (&socket).read_exact(&mut reply)?;
        assert_eq!(reply[0], b'R');
        let code = i32::from_be_bytes(reply[1..].try_into()?);
        let child = child.wait_with_output()?;
        let mut stderr = std::fs::read_to_string(root.join("stderr"))?;
        stderr.push_str(&String::from_utf8_lossy(&child.stderr));
        Ok((code, std::fs::read(root.join("stdout"))?, stderr))
    }

    #[test]
    fn incompatible_client_schemas_are_refused_before_opening_the_store() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ls-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        for (different_format, schema) in [
            (false, None),
            (false, Some(0)),
            (false, Some(crate::db::SCHEMA_VERSION - 1)),
            (false, Some(crate::db::SCHEMA_VERSION + 1)),
            (true, Some(crate::db::SCHEMA_VERSION + 1)),
        ] {
            let mut peer = serde_json::to_value(crate::store_version::binary())?;
            if let Some(schema) = schema {
                peer["schema"] = schema.into();
            }
            if different_format {
                peer["format"] = (crate::store_version::FORMAT + 1).into();
            }
            let (code, stdout, stderr) = child_request_with_peer(&root, serde_json::json!({}), false, peer)?;
            assert_eq!(code, 128, "{stderr}");
            assert!(stdout.is_empty());
            if different_format {
                assert!(stderr.contains("format 1") && stderr.contains("format 2"), "{stderr}");
            }
            assert!(stderr.contains("schema"), "{stderr}");
            assert!(stderr.contains("upgrade both the CLI and the daemon"), "{stderr}");
            assert!(!root.join("store").exists(), "refused client must not initialize or migrate a store");
            if let Some(schema) = schema.filter(|schema| *schema > 0) {
                assert!(stderr.contains(&schema.to_string()), "{stderr}");
                assert!(stderr.contains(&crate::db::SCHEMA_VERSION.to_string()), "{stderr}");
            }
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn client_trace_is_isolated_and_stdout_stays_json() -> Result<()> {
        let root = std::env::temp_dir().join(format!("lt-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        let (code, traced, stderr) = child_request(&root, serde_json::json!({"LAYR_TRACE":"1"}), false)?;
        assert_eq!(code, 0, "{stderr}");
        assert!(stderr.lines().any(|line| line.starts_with("trace ")), "{stderr}");
        assert!(stderr.lines().any(|line| line.ends_with("  request worker")), "named worker span missing: {stderr}");
        assert!(stderr.contains("trace begin request worker begin"), "worker begin missing: {stderr}");
        // Free disk space changes as concurrent builds run; all other JSON fields stay equal.
        let stable_json = |bytes: &[u8]| -> Result<serde_json::Value> {
            let mut rows: Vec<serde_json::Value> = serde_json::from_slice(bytes)?;
            for row in &mut rows {
                let row = row.as_object_mut().context("volume is not an object")?;
                row.remove("used");
                row.remove("free");
            }
            Ok(rows.into())
        };
        let traced = stable_json(&traced)?;
        for daemon_trace in [false, true] {
            let (code, plain, stderr) = child_request(&root, serde_json::json!({}), daemon_trace)?;
            assert_eq!(code, 0, "{stderr}");
            assert_eq!(stable_json(&plain)?, traced);
            assert!(!stderr.lines().any(|line| line.starts_with("trace ")), "{stderr}");
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn malformed_environment_is_an_error() -> Result<()> {
        let root = std::env::temp_dir().join(format!("le-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        for env in [serde_json::json!({"LAYR_TRACE":1}), serde_json::json!([]), serde_json::Value::Null] {
            let (code, stdout, stderr) = child_request(&root, env, false)?;
            assert_eq!(code, 128, "{stderr}");
            assert!(stdout.is_empty());
            assert!(!stderr.is_empty());
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn a_closed_connection_is_no_request_and_a_short_one_is_bad() -> Result<()> {
        let (client, server) = UnixStream::pair()?;
        drop(client);
        assert!(read_request(&server)?.is_none(), "EOF before any byte is a normal close");
        for n in 1..4 {
            let (mut client, server) = UnixStream::pair()?;
            client.write_all(&vec![0u8; n])?;
            drop(client);
            let err = read_request(&server).err().context("a short request must fail")?;
            assert_eq!(err.to_string(), "bad request", "{n} bytes before EOF");
        }
        Ok(())
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn negotiation_refuses_missing_zero_and_different_schemas_before_a_command() -> Result<()> {
        for (different_format, schema) in [
            (false, None),
            (false, Some(0)),
            (false, Some(crate::db::SCHEMA_VERSION - 1)),
            (false, Some(crate::db::SCHEMA_VERSION + 1)),
            (true, Some(crate::db::SCHEMA_VERSION + 1)),
        ] {
            let (client, server) = UnixStream::pair()?;
            let thread = std::thread::spawn(move || -> Result<()> {
                let mut hello = Vec::new();
                let fds = read_request(&server, &mut hello)?.context("no hello")?;
                assert!(fds.is_empty());
                let mut version = serde_json::to_value(crate::store_version::binary())?;
                if let Some(schema) = schema {
                    version["schema"] = schema.into();
                }
                if different_format {
                    version["format"] = (crate::store_version::FORMAT + 1).into();
                }
                let bytes = serde_json::to_vec(&version)?;
                (&server).write_all(&[b'V'])?;
                (&server).write_all(&(bytes.len() as u32).to_be_bytes())?;
                (&server).write_all(&bytes)?;
                let mut next = [0];
                assert_eq!((&server).read(&mut next)?, 0, "no command or descriptors after schema refusal");
                Ok(())
            });
            let result = negotiate(&client);
            drop(client);
            thread.join().unwrap()?;
            let error = format!("{:#}", result.expect_err("unknown or different schema must refuse negotiation"));
            if different_format {
                assert!(error.contains("format 1") && error.contains("format 2"), "{error}");
            }
            assert!(error.contains("schema"), "{error}");
            assert!(error.contains("upgrade both the CLI and the daemon"), "{error}");
            if let Some(schema) = schema.filter(|schema| *schema > 0) {
                assert!(error.contains(&schema.to_string()), "{error}");
                assert!(error.contains(&crate::db::SCHEMA_VERSION.to_string()), "{error}");
            }
        }
        Ok(())
    }

    #[test]
    fn negotiation_checks_the_daemon_before_a_command() -> Result<()> {
        for format in [crate::store_version::FORMAT, crate::store_version::FORMAT + 1] {
            let (client, server) = UnixStream::pair()?;
            let thread = std::thread::spawn(move || -> Result<()> {
                let mut hello = Vec::new();
                let fds = read_request(&server, &mut hello)?.context("no request")?;
                assert_eq!(serde_json::from_slice::<serde_json::Value>(&hello[4..])?, serde_json::json!({"hello": true}));
                assert!(fds.is_empty());
                let mut version = crate::store_version::peer()?;
                version.version.format = format;
                version.version.version = "other-package".into();
                version.version.build = "other-build".into();
                let bytes = serde_json::to_vec(&version)?;
                (&server).write_all(&[b'V'])?;
                (&server).write_all(&(bytes.len() as u32).to_be_bytes())?;
                (&server).write_all(&bytes)?;
                let mut next = [0];
                assert_eq!((&server).read(&mut next)?, 0, "negotiation sends no command");
                Ok(())
            });
            let result = negotiate(&client);
            if format == crate::store_version::FORMAT {
                result?;
            } else {
                let error = result.unwrap_err().to_string();
                for text in ["format 2", "format 1", "other-package", "other-build"] {
                    assert!(error.contains(text), "{error}");
                }
            }
            drop(client);
            thread.join().unwrap()?;
        }
        Ok(())
    }

    #[test]
    fn negotiation_refuses_old_and_busy_daemons_without_a_command() -> Result<()> {
        for answer in [b'R', b'B'] {
            let (client, server) = UnixStream::pair()?;
            let thread = std::thread::spawn(move || -> Result<()> {
                let mut hello = Vec::new();
                let fds = read_request(&server, &mut hello)?.context("no request")?;
                assert_eq!(serde_json::from_slice::<serde_json::Value>(&hello[4..])?, serde_json::json!({"hello": true}));
                assert!(fds.is_empty());
                (&server).write_all(&[answer])?;
                let mut next = [0];
                assert_eq!((&server).read(&mut next)?, 0);
                Ok(())
            });
            let error = negotiate(&client).unwrap_err().to_string();
            assert!(error.contains(if answer == b'B' { "busy" } else { "unguarded layr" }), "{error}");
            drop(client);
            thread.join().unwrap()?;
        }
        Ok(())
    }

    #[test]
    fn a_busy_answer_is_read_when_the_hello_write_fails() -> Result<()> {
        // A busy daemon writes "B" and closes without reading the request. The client's hello
        // write then fails with a broken pipe, but the answer is already queued for reading.
        let (client, server) = UnixStream::pair()?;
        (&server).write_all(b"B")?;
        drop(server);
        let error = negotiate(&client).unwrap_err().to_string();
        assert!(error.contains("busy"), "{error}");
        Ok(())
    }
}

#[cfg(test)]
mod disk_full_tests {
    use super::disk_full;

    #[test]
    fn a_full_disk_is_named_from_io_and_sqlite_errors() {
        let nospc = std::io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error());
        assert!(disk_full(&anyhow::Error::new(nospc).context("clone a tree")));
        let shm = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(4874), None);
        assert!(disk_full(&anyhow::Error::new(shm)));
        let full = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(13), None);
        assert!(disk_full(&anyhow::Error::new(full)));
        let other = std::io::Error::from_raw_os_error(rustix::io::Errno::ACCESS.raw_os_error());
        assert!(!disk_full(&anyhow::Error::new(other)));
    }
}
