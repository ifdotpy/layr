//! The LayrFS server: one process per user, one thread per connection, one index and one watcher
//! per work tree. An index is built on the first request for its tree; requests during the build
//! get a fallback. Trees idle for `LAYR_FS_IDLE` seconds (default 1800) are dropped, and the
//! server exits after `LAYR_FS_EXIT` seconds without requests (default 7200).

use super::index::Tree;
use super::watch::{Sink, Watcher};
use super::{find, peer_uid, read_frame, rg, version, write_frame, FrameWriter, Outcome};
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
    /// A virtual line: its lower tree and upper folder (`super::virtual_folders`).
    overlay: Option<(PathBuf, PathBuf)>,
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
        // A virtual line: the watcher reports upper paths; one compare of the upper folder times
        // and the whiteouts finds every change, and never reads the mount.
        if line.overlay.is_some() {
            if let State::Ready(tree) = &mut *line.state.write().unwrap() {
                let st = tree.stale_virtual();
                if !st.is_empty() {
                    tree.apply_stale_virtual(st);
                }
            }
            return;
        }
        // A rebuild (lost events, a changed `.git/info/exclude`) walks the tree without holding
        // the index; queries keep using the old one until the new one replaces it. Events that
        // come meanwhile wait in the watcher's queue and are applied after.
        let rebuild = rescan || paths.iter().any(|p| p.as_os_str().is_empty() || p == Path::new(".git/info/exclude"));
        if rebuild && matches!(&*line.state.read().unwrap(), State::Ready(_)) {
            let fresh = Tree::build(&line.root);
            if let State::Ready(tree) = &mut *line.state.write().unwrap() {
                *tree = fresh;
            }
            return;
        }
        let mut st = line.state.write().unwrap();
        match &mut *st {
            State::Building { pending, rescan: r } => {
                pending.extend(paths);
                *r |= rescan;
            }
            State::Ready(tree) => tree.refresh(&line.root, &paths),
            State::Failed(_) => {}
        }
    }
}

impl Line {
    fn build(self: &Arc<Line>) {
        let started = Instant::now();
        let sink: Arc<dyn Sink> = Arc::new(LineSink(Arc::downgrade(self)));
        // The watcher starts before the walk, so that no change during the walk is lost.
        // `LAYR_FS_NO_WATCH` leaves it out: every query then compares folder times (tests).
        let watch = std::env::var_os("LAYR_FS_NO_WATCH").is_none_or(|v| v.is_empty());
        let watched = match &self.overlay {
            Some((_, upper)) => upper.clone(),
            None => self.root.clone(),
        };
        match if watch { Watcher::start(&watched, sink).map(Some) } else { Ok(None) } {
            Ok(w) => *self.watcher.lock().unwrap() = w.map(Arc::new),
            Err(e) => {
                *self.state.write().unwrap() = State::Failed(format!("{e:#}"));
                self.mark_ready();
                eprintln!("layrfs: {}: {e:#}", self.root.display());
                return;
            }
        }
        let mut tree = match &self.overlay {
            Some((lower, upper)) => Tree::build_virtual(lower, upper),
            None => Tree::build(&self.root),
        };
        {
            let mut st = self.state.write().unwrap();
            if self.overlay.is_some() {
                // Changes during the build: the upper folder times tell them.
                let stale = tree.stale_virtual();
                tree.apply_stale_virtual(stale);
            } else if let State::Building { pending, rescan } = &mut *st {
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
        // A line changes between a folder and a virtual view (`layr line show`): its index is
        // made again for the new kind.
        let overlay = super::virtual_folders(root);
        let mut lines = self.lines.lock().unwrap();
        if let Some(l) = lines.get(root).filter(|l| l.overlay == overlay) {
            l.used.store(now_s(), Ordering::Relaxed);
            return l.clone();
        }
        let line = Arc::new(Line {
            root: root.to_path_buf(),
            overlay,
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

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn serve(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let dir = path.parent().context("socket path without a folder")?;
    // Keep no folder of the caller in use (a virtual line's mount could not be unmounted).
    let _ = std::env::set_current_dir("/");
    let me = rustix::process::getuid().as_raw();
    // A folder made here is private. An existing folder is not changed; it must be ours.
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, PermissionsExt::from_mode(0o700))?;
    }
    let meta = std::fs::metadata(dir).with_context(|| format!("cannot open {}", dir.display()))?;
    if meta.uid() != me || meta.mode() & 0o022 != 0 {
        bail!("{} belongs to another user or others may write to it; set LAYR_FS_SOCKET", dir.display());
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
        // Each version has its own socket; a client reaches another version's server only
        // through `LAYR_FS_SOCKET`. It runs the real tool, and this server keeps running.
        return reply(sock, serde_json::json!({"fallback": "the layrfs server runs another layr build"}));
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
            if let Some(w) = &watcher {
                w.sync();
            }
            let args: Vec<String> = serde_json::from_value(req.get("args").cloned().unwrap_or_default())?;
            let stdin_readable = req.get("stdin_readable").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut out = FrameWriter { sock, kind: b'o', buf: Vec::new() };
            let mut err = FrameWriter { sock, kind: b'e', buf: Vec::new() };
            // FSEvents can deliver a change after the flush, so on macOS a query compares folder
            // times in its scope first. inotify queues events when the change happens, so its
            // drain is exact; but folders that rg does not enter are not watched and `find`
            // reaches them, and without a watcher every query compares.
            let is_find = req.get("tool").and_then(|v| v.as_str()) == Some("find");
            let verify = cfg!(target_os = "macos") || watcher.is_none() || is_find;
            let scope = super::index::key(&cwd);
            // The comparison and the reading of the index happen under one lock: a rebuild
            // that replaces the tree in between would answer from a walk older than the change.
            let plan = |tree: &Tree, out: &mut FrameWriter| -> (Outcome, Option<rg::Search>) {
                match req.get("tool").and_then(|v| v.as_str()) {
                    Some("rg") => match rg::plan(&root, &cwd, stdin_readable, &args, tree, out) {
                        Ok(rg::Step::Done(code)) => (Outcome::Done(code), None),
                        Ok(rg::Step::Search(s)) => (Outcome::Done(0), Some(s)),
                        Err(why) => (Outcome::Fallback(why), None),
                    },
                    Some("find") => (find::run(&cwd, &args, tree, out), None),
                    _ => (Outcome::Fallback("unknown tool".into()), None),
                }
            };
            let not_ready = |st: &State| match st {
                State::Ready(_) => None,
                State::Building { .. } => Some("the index is being built".to_string()),
                State::Failed(e) => Some(format!("no index: {e}")),
            };
            let (outcome, search) = {
                let st = line.state.read().unwrap();
                if let Some(why) = not_ready(&st) {
                    return reply(sock, serde_json::json!({"fallback": why}));
                }
                let State::Ready(tree) = &*st else { unreachable!() };
                if line.overlay.is_some() {
                    // A virtual line: compare the upper folder only, never the mount.
                    if tree.stale_virtual().is_empty() {
                        plan(tree, &mut out)
                    } else {
                        drop(st);
                        let mut st = line.state.write().unwrap();
                        if let Some(why) = not_ready(&st) {
                            return reply(sock, serde_json::json!({"fallback": why}));
                        }
                        let State::Ready(tree) = &mut *st else { unreachable!() };
                        let stale = tree.stale_virtual();
                        tree.apply_stale_virtual(stale);
                        plan(tree, &mut out)
                    }
                } else if !verify || tree.stale(&root, &scope).is_empty() {
                    plan(tree, &mut out)
                } else {
                    drop(st);
                    let mut st = line.state.write().unwrap();
                    if let Some(why) = not_ready(&st) {
                        return reply(sock, serde_json::json!({"fallback": why}));
                    }
                    let State::Ready(tree) = &mut *st else { unreachable!() };
                    let stale = tree.stale(&root, &scope);
                    tree.apply_stale(&root, stale);
                    plan(tree, &mut out)
                }
            };
            let outcome = match (outcome, search) {
                (Outcome::Done(_), Some(s)) => Outcome::Done(s.run(&mut out, &mut err)),
                (o, _) => o,
            };
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
