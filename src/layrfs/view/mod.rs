//! `layr fs view`: the FUSE view of a line on Linux, served by a normal user (docs/layrfs-fuse.md).
//!
//! One server process per line mounts the view and answers a control socket. The socket speaks
//! the frames of `layr fs serve` (`super::write_frame`): `tool` (`layr tool rg|find` from the
//! index of the view, reading the backing files), `fsmonitor` (git's hook), `status`, `save`,
//! `stats`, `reset` and `stop`. The mount's source name carries the socket path, so a client finds
//! the server of a view from `/proc/self/mountinfo` (`super::viewclient`).

mod fs;

use super::index::Tree;
use super::server::{plan_tool, root_rel};
use super::{read_frame, read_frame_up_to, viewclient, write_frame, FrameWriter, Outcome};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: layr fs view start|serve --objects <dir> --state <id> --line <dir> --mnt <dir> --sock <path>
                [--cache <name>=<folder>]... [--allow-other] [--allow <uid>]... [--owner <uid>] [--group <gid>]
                [--threads <n>] [--ttl <seconds>] [--no-writeback] [--no-open] [--uring]
                [--remote <objects>]
       layr fs view hub --sock <path> [--allow <uid>]...   (lab: one process serves many lines)
       layr fs view ctl <hub sock> new <start options>...
       layr fs view ctl <sock> status|save <state>|stats|reset|stop [remove]
       layr fs view ctl <sock> trace start|stop | goto <state> | rebase <onto> <name>
       layr fs view fsmonitor <version> <token>      (git's core.fsmonitor hook in a view)
Experimental (docs/layrfs-fuse.md).";

pub fn run(args: &[String]) -> Result<i32> {
    let (cmd, rest) = args.split_first().context(USAGE)?;
    match cmd.as_str() {
        "start" => start(rest),
        "serve" => serve(rest),
        "hub" => hub(rest),
        "ctl" => ctl(rest),
        _ => bail!("{USAGE}"),
    }
}

struct ServeArgs {
    objects: PathBuf,
    state: String,
    line: PathBuf,
    mnt: PathBuf,
    sock: PathBuf,
    caches: Vec<(Vec<u8>, PathBuf)>,
    allow_other: bool,
    allowed: Vec<u32>,
    owner: (u32, u32),
    threads: usize,
    ttl: u64,
    writeback: bool,
    no_open: bool,
    uring: bool,
    remote: Option<PathBuf>,
}

fn parse(a: &[String]) -> Result<ServeArgs> {
    let me = (rustix::process::getuid().as_raw(), rustix::process::getgid().as_raw());
    let mut s = ServeArgs {
        objects: PathBuf::new(),
        state: String::new(),
        line: PathBuf::new(),
        mnt: PathBuf::new(),
        sock: PathBuf::new(),
        caches: Vec::new(),
        allow_other: false,
        allowed: Vec::new(),
        owner: me,
        threads: 8,
        ttl: 3600,
        writeback: true,
        no_open: false,
        uring: false,
        remote: None,
    };
    let mut i = 0;
    while i < a.len() {
        let flag = a[i].as_str();
        if flag == "--allow-other"
            || flag == "--no-writeback"
            || flag == "--no-open"
            || flag == "--uring"
            || flag == "--pausable"
        {
            match flag {
                "--allow-other" => s.allow_other = true,
                // Lab: pausable readers as in the service (`fuser::set_pausable`).
                "--pausable" => fuser::set_pausable(true),
                // FUSE over io_uring: a request is served on the CPU of its caller (needs the
                // kernel setting fuse.enable_uring=1).
                "--uring" => s.uring = true,
                "--no-writeback" => s.writeback = false,
                _ => {
                    // Every write reaches the server at once (no writeback cache): the server
                    // sees each change without open and release requests.
                    s.no_open = true;
                    s.writeback = false;
                }
            }
            i += 1;
            continue;
        }
        let v = a.get(i + 1).cloned().context(USAGE)?;
        match flag {
            "--objects" => s.objects = v.into(),
            "--state" => s.state = v,
            "--line" => s.line = v.into(),
            "--mnt" => s.mnt = v.into(),
            "--sock" => s.sock = v.into(),
            "--cache" => {
                let (n, p) = v.split_once('=').context("--cache <name>=<folder>")?;
                s.caches.push((n.as_bytes().to_vec(), p.into()));
            }
            "--allow" => s.allowed.push(v.parse()?),
            // The owner that the view shows (the agent); the group is always the server's.
            "--owner" => s.owner.0 = v.parse()?,
            // The group that the view shows (the owner's group when a service serves the view).
            "--group" => s.owner.1 = v.parse()?,
            "--threads" => s.threads = v.parse()?,
            "--ttl" => s.ttl = v.parse()?,
            "--remote" => s.remote = Some(v.into()),
            other => bail!("unknown option {other}\n{USAGE}"),
        }
        i += 2;
    }
    if s.objects.as_os_str().is_empty() || s.state.is_empty() || s.line.as_os_str().is_empty() {
        bail!("{USAGE}");
    }
    if s.mnt.as_os_str().is_empty() || s.sock.as_os_str().is_empty() {
        bail!("{USAGE}");
    }
    if s.sock.to_string_lossy().contains(',') {
        bail!("the socket path must not contain a comma (it is part of the mount options)");
    }
    if !s.allowed.is_empty() {
        s.allowed.push(me.0);
    }
    Ok(s)
}

/// Run `serve` in a child process and return once it is mounted ("ready" from the child). The
/// child is a fork of this process (no second exec of the binary); it makes the mount folder.
fn start(a: &[String]) -> Result<i32> {
    let s = parse(a)?;
    std::fs::create_dir_all(&s.line)?;
    if let Err(e) = std::fs::create_dir(&s.mnt) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e).with_context(|| format!("mkdir {}", s.mnt.display()));
        }
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(s.line.join("server.log"))?;
    let (read_end, write_end) = rustix::pipe::pipe()?;
    // `start` runs before this process makes any thread: the child only runs `serve`.
    let pid = match crate::sys::fork()? {
        crate::sys::Fork::Parent(pid) => pid,
        crate::sys::Fork::Child => {
            let _ = rustix::process::setsid();
            let _ = rustix::stdio::dup2_stdout(&write_end);
            let _ = rustix::stdio::dup2_stderr(&log);
            if let Ok(null) = std::fs::File::open("/dev/null") {
                let _ = rustix::stdio::dup2_stdin(&null);
            }
            drop(read_end);
            drop(write_end);
            let _ = std::env::set_current_dir("/");
            let code = match serve(a) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("layr fs view: {e:#}");
                    1
                }
            };
            let _ = std::io::stdout().flush();
            std::process::exit(code);
        }
    };
    drop(write_end);
    let mut line = String::new();
    BufReader::new(std::fs::File::from(read_end)).read_line(&mut line)?;
    if line.trim() != "ready" {
        let _ = rustix::process::waitpid(rustix::process::Pid::from_raw(pid), rustix::process::WaitOptions::empty());
        bail!("the server did not start; see {}", s.line.join("server.log").display());
    }
    Ok(0)
}

fn raise_nofile() {
    use rustix::process::{getrlimit, setrlimit, Resource};
    let mut l = getrlimit(Resource::Nofile);
    l.current = l.maximum;
    let _ = setrlimit(Resource::Nofile, l);
}

/// Write the kernel's dirty pages of the mount to the server, so a save, a status or a search
/// sees every write.
fn sync_mount(mnt: &Path) {
    if let Ok(f) = std::fs::File::open(mnt) {
        let _ = rustix::fs::syncfs(&f);
    }
}

/// A server that died leaves its mount behind: unmount it. `stat` cannot tell, because the
/// kernel answers it from its attribute cache for the whole timeout; `statfs` always asks the
/// server, and gives ENOTCONN when the server is gone.
fn clear_dead_mount(mnt: &Path) {
    if let Err(e) = rustix::fs::statfs(mnt) {
        if e == rustix::io::Errno::NOTCONN {
            eprintln!("layr fs view: removing the dead mount at {}", mnt.display());
            let _ = std::process::Command::new("fusermount3").arg("-u").arg("-z").arg(mnt).status();
        }
    }
}

/// The index of the view for `layr tool rg|find`: built from a walk of the mount (the same files
/// and ignore rules that rg sees), reading file contents from the backing files.
type Index = Arc<RwLock<Option<Tree>>>;

fn build_index(index: &Index, inner: &Arc<RwLock<fs::Inner>>, mnt: &Path) {
    let t = Instant::now();
    // git's index first: files loaded from now on show the stat data it recorded.
    inner.write().unwrap().read_git_index();
    let t_git = t.elapsed();
    let known = inner.write().unwrap().index_parts().ok();
    let t_parts = t.elapsed();
    let mut tree = Tree::build_from(mnt, known);
    eprintln!("layr fs view: index: git {:.3} s, parts {:.3} s", t_git.as_secs_f64(), t_parts.as_secs_f64());
    let inner = inner.clone();
    tree.content_of = Some(Arc::new(move |k: &[u8]| inner.read().unwrap().key_content(k)));
    eprintln!("layr fs view: index of {} entries in {:.3} s", tree.all.len(), t.elapsed().as_secs_f64());
    *index.write().unwrap() = Some(tree);
}

struct Shared {
    s: ServeArgs,
    inner: Arc<RwLock<fs::Inner>>,
    counters: Arc<fs::Counters>,
    fds: Arc<fs::Fds>,
    index: Index,
    notifier: fuser::Notifier,
    trace: Arc<fs::Trace>,
    /// fsmonitor queries, and those answered with a list (a token of this server).
    fsm_total: std::sync::atomic::AtomicU64,
    fsm_listed: std::sync::atomic::AtomicU64,
    /// The store of file contents into the kernel's page cache after a full scan of git (`warm`).
    warm: Arc<Warm>,
}

fn serve(a: &[String]) -> Result<i32> {
    let s = parse(a)?;
    raise_nofile();
    run_line(s, |r| {
        if r.is_ok() {
            println!("ready");
            let _ = std::io::stdout().flush();
        }
    })
}

/// Lab: one process that mounts and serves many lines. `ctl <hub> new <start options>` mounts a
/// line in a thread of this process and answers once it is mounted; the line has its own control
/// socket, and `stop` there ends its thread.
fn hub(a: &[String]) -> Result<i32> {
    let mut sock = None;
    let mut allowed = Vec::new();
    let mut i = 0;
    while i < a.len() {
        let v = a.get(i + 1).cloned().context(USAGE)?;
        match a[i].as_str() {
            "--sock" => sock = Some(PathBuf::from(v)),
            "--allow" => allowed.push(v.parse::<u32>()?),
            _ => bail!("{USAGE}"),
        }
        i += 2;
    }
    let sock = sock.context(USAGE)?;
    raise_nofile();
    let listener = bind_hub(&sock, !allowed.is_empty())?;
    println!("ready");
    std::io::stdout().flush()?;
    let lines = Lines::default();
    let me = rustix::process::getuid().as_raw();
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let peer = rustix::net::sockopt::socket_peercred(&conn).ok().map(|c| c.uid.as_raw());
        if !peer.is_some_and(|u| u == me || allowed.contains(&u)) {
            let _ = read_frame(&mut &conn);
            let _ = finish(&conn, "error: permission denied\n", 1);
            continue;
        }
        if hub_request(conn, &lines) {
            break;
        }
    }
    let _ = std::fs::remove_file(&sock);
    Ok(0)
}

/// The lines that a hub serves: their control sockets and their start options.
#[derive(Clone, Default)]
struct Lines(Arc<std::sync::Mutex<std::collections::BTreeMap<PathBuf, Vec<String>>>>);

fn bind_hub(sock: &Path, shared: bool) -> Result<UnixListener> {
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock)?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(if shared { 0o666 } else { 0o600 }))?;
    Ok(listener)
}

/// One request to a hub: `new` mounts a line (answered once it is mounted), `list` names the
/// lines, `stop` ends the hub (true). The caller checked the peer.
fn hub_request(conn: UnixStream, lines: &Lines) -> bool {
    let req = match read_frame(&mut &conn) {
        Ok((b'q', data)) => serde_json::from_slice::<serde_json::Value>(&data).unwrap_or_default(),
        _ => return false,
    };
    hub_dispatch(conn, req, lines)
}

fn hub_dispatch(conn: UnixStream, req: serde_json::Value, lines: &Lines) -> bool {
    match req.get("op").and_then(|v| v.as_str()) {
        Some("new") => {
            let args: Vec<String> = serde_json::from_value(req.get("args").cloned().unwrap_or_default()).unwrap_or_default();
            let lines = lines.clone();
            // A mount waits for the kernel: other requests go on meanwhile.
            std::thread::spawn(move || new_line(conn, args, lines));
            false
        }
        Some("list") => {
            let list: Vec<Vec<String>> = lines.0.lock().unwrap().values().cloned().collect();
            let _ = finish(&conn, &(serde_json::json!(list).to_string() + "\n"), 0);
            false
        }
        Some("stop") => {
            let _ = finish(&conn, "stopped\n", 0);
            true
        }
        _ => {
            let _ = finish(&conn, "error: unknown request\n", 1);
            false
        }
    }
}

/// Take a line over from the process that serves it now: its control socket answers `handoff`
/// with the view's state and the /dev/fuse descriptor. The kernel keeps the mount meanwhile.
fn take_line(args: Vec<String>, lines: Lines) -> Result<()> {
    let s = parse(&args)?;
    let conn = UnixStream::connect(&s.sock).with_context(|| format!("connect {}", s.sock.display()))?;
    // `ack`: this process confirms before it serves (`confirm_take`); an older server ignores it.
    write_frame(&mut &conn, b'q', serde_json::json!({"op": "handoff", "ack": true}).to_string().as_bytes())?;
    // The state of the view: one frame of any size that its length can name (it comes from the
    // server's own user, `handoff`).
    let dump = match read_frame_up_to(&mut &conn, u32::MAX as usize)? {
        (b'h', data) => serde_json::from_slice::<serde_json::Value>(&data)?,
        (_, data) => bail!("no handoff: {}", String::from_utf8_lossy(&data)),
    };
    let mut one = [0u8; 1];
    let (_, mut fds) = crate::service::recv_with_fds(&conn, &mut one)?;
    let fd = fds.pop().context("the handoff gave no descriptor")?;
    // `a`: the old server waits for the confirmation and serves the line again without it.
    let ack = (one[0] == b'a').then_some(conn);
    let sock = s.sock.clone();
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
    let registry = lines.clone();
    let key = sock.clone();
    std::thread::spawn(move || {
        let line = s.line.clone();
        let tx2 = tx.clone();
        let r = run_line_take(s, Some((dump, fd, ack)), move |r| {
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });
        registry.0.lock().unwrap().remove(&key);
        if let Err(e) = r {
            eprintln!("layr fs view hub: {}: {e:#}", line.display());
            let _ = tx2.send(Err(format!("{e:#}")));
        }
    });
    match rx.recv() {
        Ok(Ok(())) => {
            lines.0.lock().unwrap().insert(sock, args);
            Ok(())
        }
        Ok(Err(e)) => bail!("{e}"),
        Err(_) => bail!("the line thread ended"),
    }
}

/// A new `layr daemon` takes the lines of the running one: it asks the old hub, whose service loop
/// then stops (it waits for its requests and removes the service socket). The lines move when
/// the new hub starts (`host`). Empty when no hub answers.
pub fn takeover(service_sock: &Path) -> Result<Vec<Vec<String>>> {
    let Some(hub) = service_sock.parent().map(|d| d.join("layrfs-hub.sock")) else { return Ok(Vec::new()) };
    if UnixStream::connect(&hub).is_err() {
        return Ok(Vec::new());
    }
    let (code, text) = request(&hub, &serde_json::json!({"op": "takeover"}))?;
    if code != 0 {
        bail!("the running hub refused the takeover: {}", text.trim());
    }
    let list: Vec<Vec<String>> = serde_json::from_str(text.trim())?;
    // The old service loop removes its socket when it stops.
    let deadline = Instant::now() + Duration::from_secs(120);
    while service_sock.exists() && UnixStream::connect(service_sock).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("layr service: taking over {} fast lines", list.len());
    Ok(list)
}

fn new_line(conn: UnixStream, args: Vec<String>, lines: Lines) {
    let t = Instant::now();
    match start_line(args, lines) {
        Ok(()) => {
            eprintln!("layr fs view hub: line ready in {:.4} s", t.elapsed().as_secs_f64());
            let _ = finish(&conn, "ready\n", 0);
        }
        Err(e) => {
            let _ = finish(&conn, &format!("error: {e}\n"), 1);
        }
    }
}

/// Mount the line of `args` in this hub and serve it; Ok once it is up.
fn start_line(args: Vec<String>, lines: Lines) -> std::result::Result<(), String> {
    let s = parse(&args)
        .and_then(|s| {
            std::fs::create_dir_all(&s.line)?;
            match std::fs::create_dir(&s.mnt) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e.into()),
                _ => Ok(s),
            }
        })
        .map_err(|e| format!("{e:#}"))?;
    let sock = s.sock.clone();
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
    let registry = lines.clone();
    let key = sock.clone();
    std::thread::spawn(move || {
        let line = s.line.clone();
        let tx2 = tx.clone();
        let r = run_line(s, move |r| {
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });
        registry.0.lock().unwrap().remove(&key);
        if let Err(e) = r {
            eprintln!("layr fs view hub: {}: {e:#}", line.display());
            let _ = tx2.send(Err(format!("{e:#}")));
        }
    });
    match rx.recv() {
        Ok(Ok(())) => {
            lines.0.lock().unwrap().insert(sock, args);
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the line thread ended".into()),
    }
}

/// The FUSE side of `layr daemon` (Linux): the hub of every fast line, in the process that the
/// service manager started. `zygote` is the single-threaded service loop that this process forked
/// before any thread: it takes the commands and forks one process per request. On SIGTERM the
/// zygote stops first (it waits for its requests), then every line is unmounted.
pub fn host(sock: &Path, zygote: i32, take: Vec<Vec<String>>) -> Result<i32> {
    raise_nofile();
    // Lines can move to the next `layr daemon` (`takeover`).
    fuser::set_pausable(true);
    let listener = bind_hub(sock, false)?;
    listener.set_nonblocking(true)?;
    eprintln!("layr service: LayrFS hub on {}", sock.display());
    let lines = Lines::default();
    let t = Instant::now();
    let n = take.len();
    let mut moved = 0;
    for args in take {
        match take_line(args.clone(), lines.clone()) {
            Ok(()) => moved += 1,
            Err(e) => {
                // The old server may serve the line again: it stops (its journal is on disk) and
                // this hub mounts the line, so the line stays in the list of the next takeover.
                eprintln!("layr service: a fast line could not move, mounting it here: {e:#}");
                if let Ok(s) = parse(&args) {
                    let _ = request(&s.sock, &serde_json::json!({"op": "stop"}));
                }
                if let Err(e) = start_line(args, lines.clone()) {
                    eprintln!("layr service: a fast line could not mount again: {e}");
                }
            }
        }
    }
    if n > 0 {
        eprintln!("layr service: took {moved} of {n} fast lines in {:.3} s", t.elapsed().as_secs_f64());
    }
    let zpid = rustix::process::Pid::from_raw(zygote).context("zygote pid")?;
    let mut code = 0;
    // After a takeover request: the lines leave one by one; then this process ends.
    let mut leaving = false;
    loop {
        if leaving {
            if lines.0.lock().unwrap().is_empty() {
                eprintln!("layr service: every fast line moved to the new service");
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if crate::sys::terminate_requested() {
            let _ = rustix::process::kill_process(zpid, rustix::process::Signal::TERM);
            if let Ok(Some((_, st))) = rustix::process::waitpid(Some(zpid), rustix::process::WaitOptions::empty()) {
                code = st.exit_status().unwrap_or(0) as i32;
            }
            break;
        }
        if let Ok(Some((_, st))) = rustix::process::waitpid(Some(zpid), rustix::process::WaitOptions::NOHANG) {
            eprintln!("layr service: the service loop ended ({st:?}); stopping the LayrFS hub");
            code = st.exit_status().map(|c| c as i32).unwrap_or(1);
            break;
        }
        let mut pfd = [rustix::event::PollFd::new(&listener, rustix::event::PollFlags::IN)];
        let timeout = rustix::event::Timespec { tv_sec: 0, tv_nsec: 200_000_000 };
        if rustix::event::poll(&mut pfd, Some(&timeout)).unwrap_or(0) == 0 {
            continue;
        }
        let Ok((conn, _)) = listener.accept() else { continue };
        let _ = conn.set_nonblocking(false);
        // Only root (the service's request processes) asks the hub.
        let peer = rustix::net::sockopt::socket_peercred(&conn).ok().map(|c| c.uid.as_raw());
        if peer != Some(0) {
            let _ = read_frame(&mut &conn);
            let _ = finish(&conn, "error: permission denied\n", 1);
            continue;
        }
        let req = match read_frame(&mut &conn) {
            Ok((b'q', data)) => serde_json::from_slice::<serde_json::Value>(&data).unwrap_or_default(),
            _ => continue,
        };
        if req.get("op").and_then(|v| v.as_str()) == Some("takeover") {
            // The service loop stops first: no new command while the lines move.
            let _ = rustix::process::kill_process(zpid, rustix::process::Signal::TERM);
            let _ = rustix::process::waitpid(Some(zpid), rustix::process::WaitOptions::empty());
            let list: Vec<Vec<String>> = lines.0.lock().unwrap().values().cloned().collect();
            let _ = finish(&conn, &(serde_json::json!(list).to_string() + "\n"), 0);
            leaving = true;
            continue;
        }
        let _ = hub_dispatch(conn, req, &lines);
    }
    let _ = std::fs::remove_file(sock);
    let socks: Vec<PathBuf> = lines.0.lock().unwrap().keys().cloned().collect();
    for s in socks {
        let _ = request(&s, &serde_json::json!({"op": "stop"}));
    }
    eprintln!("layr service: LayrFS hub stopped");
    Ok(code)
}

/// Mount the line of `s` and serve its control socket until `stop`. `ready` runs once the mount
/// and the socket are up (with Ok), or on an error before that.
fn run_line(s: ServeArgs, ready: impl FnOnce(Result<(), &anyhow::Error>)) -> Result<i32> {
    run_line_take(s, None, ready)
}

/// The state and the /dev/fuse descriptor of a line that another process served (`handoff`), and
/// the connection to that process when it waits for a confirmation.
type Taken = (serde_json::Value, std::os::fd::OwnedFd, Option<UnixStream>);

/// The two-step end of a handoff: this process is ready (`r`), the old one stops serving and
/// says go (`g`). Without the go (the old server gave up waiting and serves the line again),
/// this process must not serve: two servers would read the same descriptor.
fn confirm_take(conn: &UnixStream) -> Result<()> {
    write_frame(&mut &*conn, b'r', b"")?;
    conn.set_read_timeout(Some(HANDOFF_WAIT))?;
    match read_frame(&mut &*conn) {
        Ok((b'g', _)) => Ok(()),
        _ => bail!("the old server serves the line again"),
    }
}

/// How long each side of a handoff waits for the other.
const HANDOFF_WAIT: Duration = Duration::from_secs(120);

fn run_line_take(s: ServeArgs, take: Option<Taken>, ready: impl FnOnce(Result<(), &anyhow::Error>)) -> Result<i32> {
    let mut ready = Some(ready);
    let r = run_line_inner(s, take, &mut ready);
    if let (Err(e), Some(f)) = (&r, ready.take()) {
        f(Err(e));
    }
    r
}

fn run_line_inner<F: FnOnce(Result<(), &anyhow::Error>)>(mut s: ServeArgs, take: Option<Taken>, ready: &mut Option<F>) -> Result<i32> {
    let t = Instant::now();
    // No prune lease for the server's lifetime: `gc` runs while the line is mounted. Each write
    // of objects takes the lease (`Inner::snapshot`, `save`, `rebase`).
    let store = layr_objects::Store::open_existing_unleased(&s.objects, layr_objects::Keying::Plain)?;
    let t_store = t.elapsed();
    // A line that can move to another process reads /dev/fuse (no io_uring: a ring's requests
    // cannot stop between two of them).
    if fuser::PAUSABLE.load(std::sync::atomic::Ordering::Relaxed) && s.uring {
        s.uring = false;
    }
    let (mut inner, fd, ack) = match take {
        Some((dump, fd, ack)) => (fs::Inner::restore(store, &s.line, s.caches.clone(), &dump)?, Some(fd), ack),
        None => (fs::Inner::open(store, &s.line, &s.state, s.caches.clone())?, None, None),
    };
    let mut taken = fd.is_some();
    inner.lazy = s.no_open;
    if let Some(r) = &s.remote {
        inner.remote = Some(layr_objects::Store::open_existing(r, layr_objects::Keying::Plain)?);
    }
    let inner = Arc::new(RwLock::new(inner));
    let t_inner = t.elapsed();
    let counters = Arc::new(fs::Counters::default());
    let cfg = fs::Config {
        ttl: Duration::from_secs(s.ttl),
        uid: s.owner.0,
        gid: s.owner.1,
        allowed: s.allowed.clone(),
        writeback: s.writeback,
        no_open: s.no_open,
        uring: s.uring,
    };
    let fds = Arc::new(fs::Fds::default());
    let trace = Arc::new(fs::Trace::default());
    let open = Arc::new(fs::OpenFiles::new());
    let view = fs::View::new(inner.clone(), counters.clone(), fds.clone(), open.clone(), cfg.clone(), trace.clone());
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName(format!("{}{}", viewclient::FSNAME_PREFIX, s.sock.display())),
        fuser::MountOption::Subtype("layrfs".into()),
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::NoAtime,
        fuser::MountOption::NoDev,
        fuser::MountOption::NoSuid,
    ];
    // No auto_unmount: its watcher process is not reliable across sessions, and fuser then
    // cannot unmount. The server unmounts on stop; after a crash the next start removes the dead
    // mount.
    if s.allow_other {
        config.acl = fuser::SessionACL::All;
    }
    config.n_threads = Some(s.threads);
    if s.uring {
        fuser::set_uring(2 << 20);
    }
    config.clone_fd = true;
    let mut session = match fd {
        Some(fd) => {
            if let Some(conn) = &ack {
                confirm_take(conn)?;
            }
            let acl = config.acl;
            fuser::Session::from_fd_ready(view, fd, acl, config.clone()).spawn().context("serve the taken line")?
        }
        None => {
            clear_dead_mount(&s.mnt);
            fuser::spawn_mount(view, &s.mnt, &config).context("mount")?
        }
    };
    let mnt_dev = std::os::unix::fs::MetadataExt::dev(&std::fs::metadata(&s.mnt)?);
    let _ = std::fs::remove_file(&s.sock);
    let listener = UnixListener::bind(&s.sock)?;
    std::fs::set_permissions(&s.sock, std::fs::Permissions::from_mode(if s.allow_other { 0o666 } else { 0o600 }))?;
    eprintln!(
        "layr fs view: mounted {} in {:.3} s (store {:.3}, journal {:.3})",
        s.mnt.display(),
        t.elapsed().as_secs_f64(),
        t_store.as_secs_f64(),
        t_inner.as_secs_f64()
    );
    let shared = Arc::new(Shared {
        notifier: session.notifier(),
        s,
        inner,
        counters,
        fds,
        index: Arc::new(RwLock::new(None)),
        fsm_total: Default::default(),
        fsm_listed: Default::default(),
        warm: Default::default(),
        trace,
    });
    {
        let sh = shared.clone();
        std::thread::spawn(move || build_index(&sh.index, &sh.inner, &sh.s.mnt));
    }
    if let Some(f) = ready.take() {
        f(Ok(()));
    }
    let me = rustix::process::getuid().as_raw();
    let mut stopper = None;
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        // Only the owner of the server and the allowed users may use the socket.
        let peer = rustix::net::sockopt::socket_peercred(&conn).ok().map(|c| c.uid.as_raw());
        let ok = peer
            .is_some_and(|u| u == me || shared.s.allowed.contains(&u) || (shared.s.allowed.is_empty() && !shared.s.allow_other));
        if !ok {
            let _ = finish(&conn, "error: permission denied\n", 1);
            continue;
        }
        // One request frame, in time: a client that stops in the middle of its request must not
        // hold the other clients of the line.
        let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
        let req = match read_frame(&mut &conn) {
            Ok((b'q', data)) => serde_json::from_slice::<serde_json::Value>(&data).unwrap_or_default(),
            _ => continue,
        };
        let _ = conn.set_read_timeout(None);
        // A save or a rebase writes a state of the project, and a goto or a reset moves the line
        // to another state: only the server's own user (the service) asks.
        if matches!(req.get("op").and_then(|v| v.as_str()), Some("save" | "rebase" | "goto" | "reset")) && peer != Some(me) {
            let _ = finish(&conn, "error: permission denied\n", 1);
            continue;
        }
        if req.get("op").and_then(|v| v.as_str()) == Some("stop") {
            // The answer comes after the unmount; `stop remove` also removes the line folder
            // and the mount folder.
            let remove = req.get("state").and_then(|v| v.as_str()) == Some("remove");
            stopper = Some((conn, remove));
            break;
        }
        // Another process takes the line over (only the server's own user asks).
        if req.get("op").and_then(|v| v.as_str()) == Some("handoff") && peer == Some(me) {
            // A state too large for one frame: no handoff, this server keeps the line.
            if shared.inner.read().unwrap().dump().to_string().len() > (u32::MAX as usize) / 2 {
                let _ = finish(&conn, "error: the view is too large for a handoff\n", 1);
                continue;
            }
            // The handles of open files live in this process: the other one would not know them.
            if !open.is_empty() {
                let _ = finish(&conn, "error: files are open through the view, no handoff\n", 1);
                continue;
            }
            // The socket stays: the new server binds its own at the same path.
            let t = Instant::now();
            let fd = session.pause_and_take().context("pause the line")?;
            let ack = req.get("ack").and_then(|v| v.as_bool()) == Some(true);
            let sent = (|| -> Result<()> {
                // A file opened before the pause: the handoff stops here too.
                anyhow::ensure!(open.is_empty(), "files are open through the view");
                let dump = shared.inner.read().unwrap().dump().to_string();
                anyhow::ensure!(dump.len() <= u32::MAX as usize, "the view grew too large for a handoff");
                // A taker that stops reading must not hold the paused line.
                conn.set_write_timeout(Some(HANDOFF_WAIT))?;
                write_frame(&mut &conn, b'h', dump.as_bytes())?;
                use std::os::fd::AsFd;
                crate::service::send_with_fds(&conn, if ack { b"a" } else { b"f" }, &[fd.as_fd()])?;
                if ack {
                    // The taker serves only after the go: until then this server can serve again.
                    conn.set_read_timeout(Some(HANDOFF_WAIT))?;
                    match read_frame(&mut &conn) {
                        Ok((b'r', _)) => {}
                        Ok((_, data)) => bail!("the taker failed: {}", String::from_utf8_lossy(&data)),
                        Err(e) => bail!("the taker gave no answer: {e}"),
                    }
                    write_frame(&mut &conn, b'g', b"")?;
                }
                Ok(())
            })();
            match sent {
                Ok(()) => {
                    eprintln!("layr fs view: {} handed over in {:.4} s", shared.s.mnt.display(), t.elapsed().as_secs_f64());
                    return Ok(0);
                }
                Err(e) => {
                    // The other process did not take the descriptor: this server serves the
                    // line again (the kernel kept the requests that came meanwhile).
                    eprintln!("layr fs view: handoff of {} failed, serving it again: {e:#}", shared.s.mnt.display());
                    let _ = finish(&conn, &format!("error: no handoff: {e:#}\n"), 1);
                    let view = fs::View::new(
                        shared.inner.clone(),
                        shared.counters.clone(),
                        shared.fds.clone(),
                        open.clone(),
                        cfg.clone(),
                        shared.trace.clone(),
                    );
                    session = fuser::Session::from_fd_ready(view, fd, config.acl, config.clone()).spawn().context("serve the line again")?;
                    taken = true;
                    continue;
                }
            }
        }
        let sh = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(&sh, &conn, &req) {
                let _ = finish(&conn, &format!("error: {e:#}\n"), 1);
            }
        });
    }
    drop(listener);
    // The index of `layr tool` is built by a walk of the mount, which holds folders open: an
    // unmount then fails with EBUSY. A view that was mounted just now (a delete after a restart
    // mounts it first) waits for the walk, at most 10 s.
    let walk_until = Instant::now() + Duration::from_secs(10);
    while shared.index.read().unwrap().is_none() && Instant::now() < walk_until {
        std::thread::sleep(Duration::from_millis(10));
    }
    let t = Instant::now();
    let _ = std::fs::remove_file(&shared.s.sock);
    // Lab: the unmount runs in a thread. The kernel takes the mount out of the folder tree at once,
    // then frees the nodes it holds (200 ms for 97,000 nodes after a walk of the whole tree). The
    // answer comes once the folder no longer shows the view, as a Btrfs delete leaves its cleanup
    // to a kernel thread.
    let (tx, rx) = std::sync::mpsc::channel();
    let mnt = shared.s.mnt.clone();
    let mut umounter = Some(std::thread::spawn(move || {
        let mut session = session;
        // A taken session has no mount object: the unmount goes to the kernel directly.
        let r = if taken {
            rustix::mount::unmount(&mnt, rustix::mount::UnmountFlags::DETACH).map_err(std::io::Error::from).or_else(|_| {
                std::process::Command::new("fusermount3").arg("-u").arg("-z").arg(&mnt).status().map(|_| ())
            })
        } else {
            session.umount()
        };
        let _ = tx.send(());
        (session, r)
    }));
    let detached = || std::fs::metadata(&shared.s.mnt).is_ok_and(|m| std::os::unix::fs::MetadataExt::dev(&m) != mnt_dev);
    while rx.recv_timeout(Duration::from_millis(1)).is_err() && !detached() {}
    let mut joined = None;
    let unmounted: std::io::Result<()> = if detached() {
        eprintln!("layr fs view: detached in {:.3} s", t.elapsed().as_secs_f64());
        Ok(())
    } else {
        let (s2, r) = umounter.take().unwrap().join().map_err(|_| anyhow::anyhow!("the unmount thread panicked"))?;
        joined = Some(s2);
        r.and(Err(std::io::Error::other("the mount is still there")))
    };
    if let Some((conn, remove)) = stopper {
        match &unmounted {
            Ok(()) => {
                if remove {
                    let _ = std::fs::remove_dir_all(&shared.s.line);
                    let _ = std::fs::remove_dir(&shared.s.mnt);
                    eprintln!("layr fs view: removed in {:.3} s", t.elapsed().as_secs_f64());
                }
                let _ = finish(&conn, "stopped\n", 0);
            }
            Err(e) => {
                let _ = finish(&conn, &format!("error: unmount: {e}\n"), 1);
            }
        }
    }
    unmounted?;
    let session = match (umounter, joined) {
        (Some(th), _) => {
            let (s2, r) = th.join().map_err(|_| anyhow::anyhow!("the unmount thread panicked"))?;
            r?;
            eprintln!("layr fs view: unmounted in {:.3} s", t.elapsed().as_secs_f64());
            s2
        }
        (None, Some(s2)) => s2,
        (None, None) => unreachable!(),
    };
    session.join()?;
    Ok(0)
}

#[derive(Default)]
struct Warm {
    armed: std::sync::atomic::AtomicBool,
    running: std::sync::atomic::AtomicBool,
    last: std::sync::Mutex<Option<Instant>>,
}

/// After a full scan: `warm` only when git then reads many files (it hashes them: an index
/// without stat data). A `git status` with a full scan reads no file; it needs no warm-up.
fn arm_warm(w: &Arc<Warm>, counters: &Arc<fs::Counters>, inner: &Arc<RwLock<fs::Inner>>, notifier: &fuser::Notifier) {
    use std::sync::atomic::Ordering;
    if w.armed.swap(true, Ordering::AcqRel) {
        return;
    }
    // Counted from before the answer: git may read a small tree before the thread starts.
    let base = counters.read.load(Ordering::Relaxed);
    let (w, counters, inner, notifier) = (w.clone(), counters.clone(), inner.clone(), notifier.clone());
    std::thread::spawn(move || {
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            if counters.read.load(Ordering::Relaxed).saturating_sub(base) >= 200 {
                warm(&w, &inner, &notifier);
                break;
            }
        }
        w.armed.store(false, Ordering::Release);
    });
}

/// Put the content of unchanged files into the kernel's page cache (FUSE_NOTIFY_STORE), so a git
/// command that reads every file one after the other (a read request per file through the mount)
/// finds them there. A store makes no read request, so the kernel also keeps the attributes (a
/// read request drops the cached access time, and the next lstat asks the server). The kernel
/// takes a store only for a node it knows: git looked up every path before it reads (the preload
/// of the index); a second pass a second later takes the nodes the kernel learned meanwhile. At
/// most once in 10 minutes and 2 GB per view.
fn warm(w: &Arc<Warm>, inner: &Arc<RwLock<fs::Inner>>, notifier: &fuser::Notifier) {
    use std::sync::atomic::{AtomicU64, Ordering};
    const EVERY: Duration = Duration::from_secs(600);
    const MAX_BYTES: u64 = 2 << 30;
    if w.last.lock().unwrap().is_some_and(|t| t.elapsed() < EVERY) || w.running.swap(true, Ordering::AcqRel) {
        return;
    }
    *w.last.lock().unwrap() = Some(Instant::now());
    let (w, inner, notifier) = (w.clone(), inner.clone(), notifier.clone());
    std::thread::spawn(move || {
        let t = Instant::now();
        let (bytes, files, unknown, other) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
        let mut done = std::collections::HashSet::new();
        for pass in 0..2 {
            if pass > 0 {
                std::thread::sleep(Duration::from_millis(1000));
            }
            let todo: Vec<u64> = inner.read().unwrap().unchanged_files().into_iter().filter(|i| !done.contains(i)).collect();
            let next = AtomicU64::new(0);
            let stored = std::sync::Mutex::new(Vec::new());
            std::thread::scope(|sc| {
                for _ in 0..4 {
                    sc.spawn(|| {
                        let mut buf = vec![0u8; 1 << 17];
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed) as usize;
                            let Some(&ino) = todo.get(k) else { return };
                            if bytes.load(Ordering::Relaxed) >= MAX_BYTES {
                                return;
                            }
                            let Some(path) = inner.read().unwrap().unchanged_content(ino) else { continue };
                            let Ok(mut f) = std::fs::File::open(&path) else { continue };
                            let mut off = 0u64;
                            let mut ok = true;
                            while let Ok(n) = std::io::Read::read(&mut f, &mut buf) {
                                if n == 0 {
                                    break;
                                }
                                if let Err(e) = notifier.store(fuser::INodeNo(ino), off, &buf[..n]) {
                                    if e.raw_os_error() == Some(libc::ENOENT) {
                                        unknown.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        other.fetch_add(1, Ordering::Relaxed);
                                    }
                                    ok = false;
                                    break;
                                }
                                off += n as u64;
                            }
                            // A write that came meanwhile: the stored pages may be older than it.
                            if inner.read().unwrap().unchanged_content(ino).is_none() {
                                let _ = notifier.inval_inode(fuser::INodeNo(ino), 0, 0);
                            } else if ok {
                                files.fetch_add(1, Ordering::Relaxed);
                                bytes.fetch_add(off, Ordering::Relaxed);
                                stored.lock().unwrap().push(ino);
                            }
                        }
                    });
                }
            });
            done.extend(stored.into_inner().unwrap());
        }
        eprintln!(
            "layr fs view: stored {} files, {} MB in {:.3} s ({} nodes unknown to the kernel, {} other errors)",
            files.load(Ordering::Relaxed),
            bytes.load(Ordering::Relaxed) >> 20,
            t.elapsed().as_secs_f64(),
            unknown.load(Ordering::Relaxed),
            other.load(Ordering::Relaxed)
        );
        w.running.store(false, Ordering::Release);
    });
}

/// Text on standard output and the exit code.
fn finish(conn: &UnixStream, text: &str, code: i32) -> std::io::Result<()> {
    if !text.is_empty() {
        write_frame(&mut &*conn, b'o', text.as_bytes())?;
    }
    write_frame(&mut &*conn, b'x', serde_json::json!({ "code": code }).to_string().as_bytes())
}

fn fallback(conn: &UnixStream, why: &str) -> Result<()> {
    write_frame(&mut &*conn, b'x', serde_json::json!({ "fallback": why }).to_string().as_bytes())?;
    Ok(())
}

fn handle(sh: &Shared, conn: &UnixStream, req: &serde_json::Value) -> Result<()> {
    let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
    match op {
        "tool" => tool(sh, conn, req),
        "fsmonitor" => {
            let token = req.get("token").and_then(|v| v.as_str()).unwrap_or("");
            let (answer, listed) = sh.inner.write().unwrap().fsmonitor(token);
            sh.fsm_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if listed {
                sh.fsm_listed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else if std::env::var_os("LAYRFS_NO_WARM").is_none() {
                // Git checks every path: an index without stat data (`read-tree` into a temporary
                // index, as a checkpoint of T3 Code makes it) then hashes every file.
                arm_warm(&sh.warm, &sh.counters, &sh.inner, &sh.notifier);
            }
            write_frame(&mut &*conn, b'o', &answer)?;
            finish(conn, "", 0)?;
            Ok(())
        }
        "status" => {
            sync_mount(&sh.s.mnt);
            let t = Instant::now();
            let lines = sh.inner.write().unwrap().status()?;
            let mut out = lines.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("# {} changes in {:.4} s\n", lines.len(), t.elapsed().as_secs_f64()));
            Ok(finish(conn, &out, 0)?)
        }
        "save" => {
            let name = req.get("state").and_then(|v| v.as_str()).context("save needs a state name")?;
            sync_mount(&sh.s.mnt);
            let json = {
                let mut inner = sh.inner.write().unwrap();
                let r = inner.save(name);
                // A save removes the upper files of saved nodes: no cached descriptor may write there.
                sh.fds.clear();
                r?
            };
            Ok(finish(conn, &(json + "\n"), 0)?)
        }
        "stats" => {
            let (head, copy_ups, journal, dirs, fetched) = {
                let i = sh.inner.read().unwrap();
                let g = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
                (
                    i.head.clone(),
                    i.stats.copy_ups,
                    i.stats.journal_lines,
                    i.stats.loaded_dirs,
                    format!(
                        "{{\"files\":{},\"bytes\":{},\"ms\":{}}}",
                        g(&i.fetched_files),
                        g(&i.fetched_bytes),
                        g(&i.fetch_ns) / 1_000_000
                    ),
                )
            };
            let indexed = sh.index.read().unwrap().as_ref().map(|t| t.all.len());
            let text = format!(
                "{{\"head\":\"{head}\",\"copy_ups\":{copy_ups},\"journal_lines\":{journal},\"loaded_dirs\":{dirs},\"index_entries\":{},\"fsmonitor\":{},\"fsmonitor_listed\":{},\"fetched\":{fetched},\"requests\":{}}}\n",
                indexed.map(|n| n.to_string()).unwrap_or("null".into()),
                sh.fsm_total.load(std::sync::atomic::Ordering::Relaxed),
                sh.fsm_listed.load(std::sync::atomic::Ordering::Relaxed),
                sh.counters.json()
            );
            Ok(finish(conn, &text, 0)?)
        }
        "reset" => {
            sync_mount(&sh.s.mnt);
            let (inodes, entries) = {
                let mut inner = sh.inner.write().unwrap();
                let r = inner.reset();
                sh.fds.clear();
                r?
            };
            // The kernel keeps names and attributes for the entry timeout: drop what it may hold.
            for (parent, name) in &entries {
                let _ = sh.notifier.inval_entry(fuser::INodeNo(*parent), std::ffi::OsStr::from_bytes(name));
            }
            for ino in &inodes {
                let _ = sh.notifier.inval_inode(fuser::INodeNo(*ino), 0, 0);
            }
            *sh.index.write().unwrap() = None;
            build_index(&sh.index, &sh.inner, &sh.s.mnt);
            Ok(finish(
                conn,
                &format!(
                    "reset to {}: {} names and {} nodes invalidated\n",
                    sh.inner.write().unwrap().head,
                    entries.len(),
                    inodes.len()
                ),
                0,
            )?)
        }
        "trace" => trace(sh, conn, req),
        // The line saved state `state` from the last snapshot (`Inner::adopt`).
        "adopt" => {
            let state = req.get("state").and_then(|v| v.as_str()).context("needs a state")?.to_string();
            let done = sh.inner.write().unwrap().adopt(&state)?;
            Ok(finish(conn, &format!("{{\"adopted\":{done}}}\n"), 0)?)
        }
        // The links to cache trees in the view (`fast_cache`): [path, target] pairs.
        "links" => {
            let list = sh.inner.read().unwrap().cache_links();
            Ok(finish(conn, &(serde_json::to_string(&list)? + "\n"), 0)?)
        }
        "snapshot" => {
            sync_mount(&sh.s.mnt);
            let json = {
                let mut inner = sh.inner.write().unwrap();
                inner.snapshot()?
            };
            Ok(finish(conn, &(json + "\n"), 0)?)
        }
        "goto" | "rebase" => {
            sync_mount(&sh.s.mnt);
            let t = Instant::now();
            let state = req.get("state").and_then(|v| v.as_str()).context("needs a state")?.to_string();
            let (text, inodes, entries) = {
                let mut inner = sh.inner.write().unwrap();
                let r = if op == "goto" {
                    inner.goto(&state).map(|(i, e)| (format!("{{\"state\":\"{state}\"}}"), i, e))
                } else {
                    let name = req.get("name").and_then(|v| v.as_str()).context("rebase <onto> <name>")?;
                    inner.rebase(&state, name)
                };
                sh.fds.clear();
                r?
            };
            let t_view = t.elapsed();
            invalidate(sh, &inodes, &entries);
            let t_inval = t.elapsed();
            // The index of `layr tool` is built again in the background.
            *sh.index.write().unwrap() = None;
            let index = sh.index.clone();
            let inner = sh.inner.clone();
            let mnt = sh.s.mnt.clone();
            std::thread::spawn(move || build_index(&index, &inner, &mnt));
            Ok(finish(
                conn,
                &format!(
                    "{text}\n# view {:.4} s, invalidate {} names and {} nodes {:.4} s\n",
                    t_view.as_secs_f64(),
                    entries.len(),
                    inodes.len(),
                    (t_inval - t_view).as_secs_f64()
                ),
                0,
            )?)
        }
        _ => Ok(finish(conn, "error: unknown request\n", 1)?),
    }
}

fn invalidate(sh: &Shared, inodes: &[u64], entries: &[(u64, Vec<u8>)]) {
    for (parent, name) in entries {
        let _ = sh.notifier.inval_entry(fuser::INodeNo(*parent), std::ffi::OsStr::from_bytes(name));
    }
    for ino in inodes {
        let _ = sh.notifier.inval_inode(fuser::INodeNo(*ino), 0, 0);
    }
}

/// Lab: `trace start` drops the kernel's names, attributes and pages of the view, so that every
/// use of a file reaches the server; `trace stop` prints what was used, one path per line:
/// `R` read, `S` stat or lookup, `L` listed folder, `M` missing name, `W` written.
fn trace(sh: &Shared, conn: &UnixStream, req: &serde_json::Value) -> Result<()> {
    match req.get("state").and_then(|v| v.as_str()) {
        Some("start") => {
            sync_mount(&sh.s.mnt);
            let t = Instant::now();
            sh.trace.start();
            let (inodes, entries) = sh.inner.read().unwrap().known();
            invalidate(sh, &inodes, &entries);
            Ok(finish(
                conn,
                &format!("# trace on; invalidated {} nodes in {:.4} s\n", inodes.len(), t.elapsed().as_secs_f64()),
                0,
            )?)
        }
        Some("stop") => {
            let sets = sh.trace.stop();
            let inner = sh.inner.read().unwrap();
            let mut lines = Vec::new();
            let mut add = |tag: char, set: &std::collections::HashSet<u64>| {
                for ino in set {
                    lines.push(format!("{tag} {}", String::from_utf8_lossy(&inner.path_of(*ino))));
                }
            };
            add('R', &sets.read);
            add('S', &sets.stat);
            add('L', &sets.listed);
            add('W', &sets.wrote);
            for (parent, name) in &sets.missing {
                let p = inner.path_of(*parent);
                let full = if p.is_empty() { name.clone() } else { [&p[..], b"/", &name[..]].concat() };
                lines.push(format!("M {}", String::from_utf8_lossy(&full)));
            }
            lines.sort();
            let mut out = lines.join("\n");
            out.push('\n');
            Ok(finish(conn, &out, 0)?)
        }
        _ => bail!("trace start|stop"),
    }
}

use std::os::unix::ffi::OsStrExt;

/// `layr tool rg|find` in the view: the answer of the index, or a fallback (the real tool runs
/// through the mount then).
fn tool(sh: &Shared, conn: &UnixStream, req: &serde_json::Value) -> Result<()> {
    let root = sh.s.mnt.clone();
    let cwd = PathBuf::from(req.get("cwd").and_then(|v| v.as_str()).context("missing cwd")?);
    let Ok(cwd) = root_rel(&root, &cwd) else { return fallback(conn, "the working folder is outside the view") };
    let args: Vec<String> = serde_json::from_value(req.get("args").cloned().unwrap_or_default())?;
    let stdin_readable = req.get("stdin_readable").and_then(|v| v.as_bool()).unwrap_or(true);
    let name = req.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    // A search reads the backing files: the kernel's writeback cache must reach them first, and
    // patched files get their upper copy.
    sync_mount(&root);
    sh.inner.write().unwrap().materialize_patched();
    let scope = super::index::key(&cwd);
    let mut out = FrameWriter { sock: conn, kind: b'o', buf: Vec::new() };
    let mut err = FrameWriter { sock: conn, kind: b'e', buf: Vec::new() };
    let (outcome, search) = {
        let mut guard = sh.index.write().unwrap();
        let Some(tree) = guard.as_mut() else { return fallback(conn, "the index of the view is being built") };
        // Folder times through the mount: the kernel answers from its attribute cache, and asks
        // the server only for folders that changed. A change made the cached time invalid.
        let stale = tree.stale(&root, &scope);
        if !stale.is_empty() {
            tree.apply_stale(&root, stale);
        }
        if let Some(why) = tree.incomplete(&root) {
            return fallback(conn, &why);
        }
        plan_tool(name, &root, &cwd, stdin_readable, &args, tree, &mut out)
    };
    let outcome = match (outcome, search) {
        (Outcome::Done(_), Some(s)) => Outcome::Done(s.run(&mut out, &mut err)),
        (o, _) => o,
    };
    match outcome {
        Outcome::Fallback(why) => fallback(conn, &why),
        Outcome::Done(code) => {
            out.flush()?;
            err.flush()?;
            write_frame(&mut &*conn, b'x', serde_json::json!({ "code": code }).to_string().as_bytes())?;
            Ok(())
        }
    }
}

/// Send one request to a view or hub socket: the exit code and the standard output text.
pub fn request(sock: &Path, req: &serde_json::Value) -> Result<(i32, String)> {
    let s = UnixStream::connect(sock).with_context(|| format!("connect {}", sock.display()))?;
    write_frame(&mut &s, b'q', req.to_string().as_bytes())?;
    let mut out = Vec::new();
    loop {
        let (kind, data) = read_frame(&mut &s)?;
        match kind {
            b'o' | b'e' => out.extend_from_slice(&data),
            b'x' => {
                let v: serde_json::Value = serde_json::from_slice(&data).unwrap_or_default();
                return Ok((
                    v.get("code").and_then(|c| c.as_i64()).unwrap_or(1) as i32,
                    String::from_utf8_lossy(&out).into_owned(),
                ));
            }
            _ => bail!("unexpected frame"),
        }
    }
}

/// Start the hub at `sock` unless one answers there. The hub runs on in its own session; its
/// errors go to `log`.
pub fn ensure_hub(sock: &Path, log: &Path) -> Result<()> {
    if UnixStream::connect(sock).is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let err = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let mut child = std::process::Command::new(exe)
        .args(["fs", "view", "hub", "--sock"])
        .arg(sock)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(err)
        .process_group(0)
        .spawn()
        .context("start the LayrFS hub")?;
    let mut line = String::new();
    BufReader::new(child.stdout.take().context("hub output")?).read_line(&mut line)?;
    if line.trim() != "ready" {
        let _ = child.wait();
        bail!("the LayrFS hub did not start; see {}", log.display());
    }
    Ok(())
}

/// `layr fs view ctl <sock> <op> [<state>]`: print the answer, exit with its code.
fn ctl(a: &[String]) -> Result<i32> {
    let (sock, words) = a.split_first().context(USAGE)?;
    let op = words.first().context(USAGE)?;
    let mut req = serde_json::json!({ "op": op });
    if op == "new" {
        req["args"] = serde_json::json!(words[1..]);
    } else if let Some(state) = words.get(1) {
        req["state"] = state.clone().into();
    }
    if let Some(name) = words.get(2) {
        req["name"] = name.clone().into();
    }
    let s = UnixStream::connect(sock).with_context(|| format!("connect {sock}"))?;
    write_frame(&mut &s, b'q', req.to_string().as_bytes())?;
    let mut stdout = std::io::stdout().lock();
    loop {
        let (kind, data) = read_frame(&mut &s)?;
        match kind {
            b'o' => stdout.write_all(&data)?,
            b'e' => std::io::stderr().write_all(&data)?,
            b'x' => {
                stdout.flush()?;
                let v: serde_json::Value = serde_json::from_slice(&data).unwrap_or_default();
                return Ok(v.get("code").and_then(|c| c.as_i64()).unwrap_or(1) as i32);
            }
            _ => bail!("unexpected frame"),
        }
    }
}
