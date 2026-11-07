//! The LayrFS server: one process per user, one thread per connection, one index and one watcher
//! per work tree. An index is built on the first request for its tree; requests during the build
//! get a fallback. Trees idle for `LAYR_FS_IDLE` seconds (default 1800) are dropped, and the
//! server exits after `LAYR_FS_EXIT` seconds without requests (default 7200).

use super::index::Tree;
use super::watch::{Sink, Watcher};
use super::{find, read_frame, rg, version, write_frame, FrameWriter, Outcome};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

enum State {
    Building { pending: Vec<PathBuf>, rescan: bool },
    Ready(Tree),
    Failed(String),
}

struct Line {
    root: PathBuf,
    state: RwLock<State>,
    ready: (Mutex<bool>, Condvar),
    watcher: Mutex<Option<Arc<Watcher>>>,
    used: AtomicU64,
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct LineSink(Weak<Line>);

impl Sink for LineSink {
    fn changed(&self, paths: Vec<PathBuf>, rescan: bool) {
        let Some(line) = self.0.upgrade() else { return };
        let mut st = line.state.write().unwrap();
        match &mut *st {
            State::Building { pending, rescan: r } => {
                pending.extend(paths);
                *r |= rescan;
            }
            State::Ready(tree) => {
                if rescan {
                    *tree = Tree::build(&line.root);
                } else {
                    tree.refresh(&line.root, &paths);
                }
            }
            State::Failed(_) => {}
        }
    }
}

impl Line {
    fn build(self: &Arc<Line>) {
        let started = Instant::now();
        let sink: Arc<dyn Sink> = Arc::new(LineSink(Arc::downgrade(self)));
        // The watcher starts before the walk, so that no change during the walk is lost.
        match Watcher::start(&self.root, sink) {
            Ok(w) => *self.watcher.lock().unwrap() = Some(Arc::new(w)),
            Err(e) => {
                *self.state.write().unwrap() = State::Failed(format!("{e:#}"));
                self.mark_ready();
                eprintln!("layrfs: {}: {e:#}", self.root.display());
                return;
            }
        }
        let mut tree = Tree::build(&self.root);
        {
            let mut st = self.state.write().unwrap();
            if let State::Building { pending, rescan } = &mut *st {
                if *rescan {
                    tree = Tree::build(&self.root);
                } else if !pending.is_empty() {
                    tree.refresh(&self.root, pending);
                }
            }
            eprintln!(
                "layrfs: indexed {} ({} entries, {} rg files) in {:.1} s",
                self.root.display(),
                tree.all.len(),
                tree.visible.len(),
                started.elapsed().as_secs_f64()
            );
            *st = State::Ready(tree);
        }
        self.mark_ready();
    }

    fn mark_ready(&self) {
        *self.ready.0.lock().unwrap() = true;
        self.ready.1.notify_all();
    }

    fn wait_ready(&self) {
        let mut done = self.ready.0.lock().unwrap();
        while !*done {
            done = self.ready.1.wait(done).unwrap();
        }
    }
}

struct Server {
    lines: Mutex<HashMap<PathBuf, Arc<Line>>>,
    used: AtomicU64,
}

impl Server {
    /// The line of `root`, created (and its build started) on first use.
    fn line(&self, root: &Path) -> Arc<Line> {
        let mut lines = self.lines.lock().unwrap();
        if let Some(l) = lines.get(root) {
            l.used.store(now_s(), Ordering::Relaxed);
            return l.clone();
        }
        let line = Arc::new(Line {
            root: root.to_path_buf(),
            state: RwLock::new(State::Building { pending: Vec::new(), rescan: false }),
            ready: (Mutex::new(false), Condvar::new()),
            watcher: Mutex::new(None),
            used: AtomicU64::new(now_s()),
        });
        lines.insert(root.to_path_buf(), line.clone());
        let l = line.clone();
        std::thread::spawn(move || l.build());
        line
    }
}

fn peer_uid(sock: &UnixStream) -> Result<u32> {
    #[cfg(target_os = "macos")]
    {
        Ok(crate::sys::peer_credentials(sock)?.0)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(rustix::net::sockopt::socket_peercred(sock)?.uid.as_raw())
    }
}

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn serve(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let dir = path.parent().context("socket path without a folder")?;
    let me = rustix::process::getuid().as_raw();
    // A folder made here is private. An existing folder is not changed; it must be ours.
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, PermissionsExt::from_mode(0o700))?;
    }
    let meta = std::fs::metadata(dir).with_context(|| format!("cannot open {}", dir.display()))?;
    if meta.uid() != me {
        bail!("{} belongs to another user; set LAYR_FS_SOCKET", dir.display());
    }
    if UnixStream::connect(path).is_ok() {
        eprintln!("layrfs: a server already listens on {}", path.display());
        return Ok(());
    }
    // Only a stale socket is removed, never another kind of file.
    if let Ok(old) = std::fs::symlink_metadata(path) {
        if !old.file_type().is_socket() {
            bail!("{} exists and is not a socket", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(path, PermissionsExt::from_mode(0o600))?;
    eprintln!("layrfs: {} listening on {} (pid {})", version(), path.display(), std::process::id());
    let server = Arc::new(Server { lines: Mutex::new(HashMap::new()), used: AtomicU64::new(now_s()) });

    let s = server.clone();
    let socket = path.to_path_buf();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let now = now_s();
        let idle = env_secs("LAYR_FS_IDLE", 1800);
        s.lines.lock().unwrap().retain(|root, l| {
            let keep = now.saturating_sub(l.used.load(Ordering::Relaxed)) < idle;
            if !keep {
                eprintln!("layrfs: dropped the idle index of {}", root.display());
            }
            keep
        });
        if now.saturating_sub(s.used.load(Ordering::Relaxed)) >= env_secs("LAYR_FS_EXIT", 7200) {
            eprintln!("layrfs: no request for a long time, exiting");
            let _ = std::fs::remove_file(&socket);
            std::process::exit(0);
        }
    });

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        match peer_uid(&conn) {
            Ok(uid) if uid == me => {}
            _ => continue, // another user: no answer
        }
        server.used.store(now_s(), Ordering::Relaxed);
        let s = server.clone();
        let socket = path.to_path_buf();
        std::thread::spawn(move || {
            if let Err(e) = handle(&s, &conn, &socket) {
                eprintln!("layrfs: request failed: {e:#}");
            }
        });
    }
    Ok(())
}

fn reply(sock: &UnixStream, v: serde_json::Value) -> Result<()> {
    write_frame(&mut &*sock, b'x', &serde_json::to_vec(&v)?)?;
    Ok(())
}

fn handle(server: &Arc<Server>, sock: &UnixStream, socket: &Path) -> Result<()> {
    let (kind, data) = read_frame(&mut &*sock)?;
    if kind != b'q' {
        bail!("unexpected frame {kind}");
    }
    let req: serde_json::Value = serde_json::from_slice(&data)?;
    if req.get("version").and_then(|v| v.as_str()) != Some(version().as_str()) {
        // A client of another build: it runs the real tool, and the next call starts a
        // server of its own build.
        reply(sock, serde_json::json!({"fallback": "the layrfs server runs another layr build; it exits"}))?;
        let _ = std::fs::remove_file(socket);
        std::process::exit(0);
    }
    let root = |key: &str| -> Result<PathBuf> {
        let p = req.get(key).and_then(|v| v.as_str()).context("missing path")?;
        Ok(PathBuf::from(p))
    };
    match req.get("op").and_then(|v| v.as_str()) {
        Some("tool") => {
            let root = root("root")?;
            let cwd = root_rel(&root, &root_cwd(&req)?)?;
            let line = server.line(&root);
            let watcher = line.watcher.lock().unwrap().clone();
            if let Some(w) = watcher {
                w.sync();
            }
            let st = line.state.read().unwrap();
            let tree = match &*st {
                State::Ready(t) => t,
                State::Building { .. } => return reply(sock, serde_json::json!({"fallback": "the index is being built"})),
                State::Failed(e) => return reply(sock, serde_json::json!({"fallback": format!("no index: {e}")})),
            };
            let args: Vec<String> = serde_json::from_value(req.get("args").cloned().unwrap_or_default())?;
            let stdin_readable = req.get("stdin_readable").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut out = FrameWriter { sock, kind: b'o', buf: Vec::new() };
            let mut err = FrameWriter { sock, kind: b'e', buf: Vec::new() };
            let outcome = match req.get("tool").and_then(|v| v.as_str()) {
                Some("rg") => rg::run(&root, &cwd, stdin_readable, &args, tree, &mut out, &mut err),
                Some("find") => find::run(&cwd, &args, tree, &mut out),
                _ => Outcome::Fallback("unknown tool".into()),
            };
            drop(st);
            match outcome {
                Outcome::Fallback(why) => reply(sock, serde_json::json!({"fallback": why})),
                Outcome::Done(code) => {
                    std::io::Write::flush(&mut out)?;
                    std::io::Write::flush(&mut err)?;
                    reply(sock, serde_json::json!({"code": code}))
                }
            }
        }
        Some("warm") => {
            let line = server.line(&root("root")?);
            if req.get("wait").and_then(|v| v.as_bool()).unwrap_or(false) {
                line.wait_ready();
            }
            let state = match &*line.state.read().unwrap() {
                State::Ready(_) => "ready".to_string(),
                State::Building { .. } => "building".to_string(),
                State::Failed(e) => format!("failed: {e}"),
            };
            reply(sock, serde_json::json!({"state": state}))
        }
        Some("status") => {
            let now = now_s();
            let lines: Vec<serde_json::Value> = server
                .lines
                .lock()
                .unwrap()
                .values()
                .map(|l| {
                    let idle = now.saturating_sub(l.used.load(Ordering::Relaxed));
                    match &*l.state.read().unwrap() {
                        State::Ready(t) => serde_json::json!({
                            "root": l.root, "state": "ready", "entries": t.all.len(),
                            "visible": t.visible.len(), "bytes": t.key_bytes(), "idle_s": idle,
                        }),
                        State::Building { .. } => serde_json::json!({"root": l.root, "state": "building", "idle_s": idle}),
                        State::Failed(e) => serde_json::json!({"root": l.root, "state": format!("failed: {e}"), "idle_s": idle}),
                    }
                })
                .collect();
            reply(sock, serde_json::json!({"version": version(), "pid": std::process::id(), "lines": lines}))
        }
        Some("stop") => {
            reply(sock, serde_json::json!({"stopped": true}))?;
            let _ = std::fs::remove_file(socket);
            std::process::exit(0);
        }
        _ => reply(sock, serde_json::json!({"fallback": "unknown request"})),
    }
}

fn root_cwd(req: &serde_json::Value) -> Result<PathBuf> {
    Ok(PathBuf::from(req.get("cwd").and_then(|v| v.as_str()).context("missing cwd")?))
}

fn root_rel(root: &Path, cwd: &Path) -> Result<PathBuf> {
    Ok(cwd.strip_prefix(root).context("the working folder is outside the tree")?.to_path_buf())
}
