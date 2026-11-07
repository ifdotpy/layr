//! LayrFS: a long-running process (`layr fs serve`) that keeps an in-memory index of working
//! trees and answers the tree tools of agents from it.
//!
//! The tools are substituted on the agent's PATH: `rg` and `find` link to the layr binary, which
//! sends the arguments to the server (`layr tool rg ...` does the same). The server answers with
//! the output of the real tool, or with a fallback, and then the client runs the real tool. A
//! fallback is used for unsupported flags, a terminal on stdout, a folder outside a layr line or
//! git work tree, a server that is not running yet (it is started then) or not the user's own,
//! and an index that is still being built. See `docs/layrfs.md`.

pub mod find;
pub mod index;
pub mod rg;
mod server;
pub mod watch;

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// The result of a served tool.
pub enum Outcome {
    /// The output was written; the exit code of the tool.
    Done(i32),
    /// Nothing was written; the client runs the real tool. The text says why.
    Fallback(String),
}

/// The tools that can be substituted.
pub const TOOLS: &[&str] = &["rg", "find"];

/// The version a client and a server must share. Each version has its own socket, so two layr
/// builds on one machine (Studio's and the one on PATH) run side by side.
fn version() -> String {
    format!("{} {}", env!("CARGO_PKG_VERSION"), option_env!("LAYR_BUILD_ID").unwrap_or("dev"))
}

/// The socket of this user's server of this layr version: `LAYR_FS_SOCKET`, or a per-user
/// folder with one socket per version.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LAYR_FS_SOCKET") {
        return PathBuf::from(p);
    }
    let uid = rustix::process::getuid().as_raw();
    let name: String = version().chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect();
    let name = format!("layrfs-{name}.sock");
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Caches/layr").join(name);
        }
    } else if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(run).join("layr").join(name);
    }
    PathBuf::from(format!("/tmp/layr-{uid}")).join(name)
}

/// The uid of the process at the other end of a Unix socket.
fn peer_uid(sock: &UnixStream) -> std::io::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        Ok(crate::sys::peer_credentials(sock)?.0)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(rustix::net::sockopt::socket_peercred(sock)?.uid.as_raw())
    }
}

/// Connect to this user's server, and only to it: the socket's folder must belong to the user
/// and not be writable by others, and the process that answers must run as the user. Another
/// user could otherwise create `/tmp/layr-<uid>/` first and print anything into an agent's
/// context.
fn connect(path: &Path) -> Result<UnixStream, String> {
    use std::os::unix::fs::MetadataExt;
    let me = rustix::process::getuid().as_raw();
    let dir = path.parent().ok_or("socket path without a folder")?;
    let meta = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if meta.uid() != me || meta.mode() & 0o022 != 0 {
        return Err(format!("{} belongs to another user or others may write to it", dir.display()));
    }
    let sock = UnixStream::connect(path).map_err(|e| format!("{}: {e}", path.display()))?;
    match peer_uid(&sock) {
        Ok(uid) if uid == me => Ok(sock),
        Ok(uid) => Err(format!("the process behind {} runs as uid {uid}", path.display())),
        Err(e) => Err(format!("cannot check the process behind {}: {e}", path.display())),
    }
}

// Frames: one kind byte, a 4-byte big-endian length, then the data.
const MAX_FRAME: usize = 64 << 20;

fn write_frame(w: &mut impl Write, kind: u8, data: &[u8]) -> std::io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&(data.len() as u32).to_be_bytes());
    w.write_all(&head)?;
    w.write_all(data)
}

fn read_frame(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok((head[0], data))
}

/// Writes data as frames of one kind (stdout `o` or stderr `e`).
struct FrameWriter<'a> {
    sock: &'a UnixStream,
    kind: u8,
    buf: Vec<u8>,
}

impl Write for FrameWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 256 << 10 {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            write_frame(&mut &*self.sock, self.kind, &self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// The tree that holds `dir`: its layr line (a folder `<store>/projects/<project>/lines/<line>`,
/// or a line moved out of the store), or else the git work tree around it (the nearest folder
/// with a `.git` entry).
fn tree_root(dir: &Path) -> Option<PathBuf> {
    for p in dir.ancestors() {
        let lines = p.parent();
        let projects = lines.and_then(Path::parent).and_then(Path::parent);
        if lines.and_then(Path::file_name) == Some(std::ffi::OsStr::new("lines"))
            && projects.and_then(Path::file_name) == Some(std::ffi::OsStr::new("projects"))
        {
            return Some(p.to_path_buf());
        }
    }
    let moved = crate::linepaths::index(&crate::store::default_root())
        .into_iter()
        .filter_map(|e| e.path.canonicalize().ok())
        .filter(|p| dir.starts_with(p))
        .max_by_key(|p| p.components().count());
    if moved.is_some() {
        return moved;
    }
    dir.ancestors().find(|p| p.join(".git").symlink_metadata().is_ok()).map(Path::to_path_buf)
}

/// The lower tree and the upper folder of a virtual line mounted at `root` (docs/views.md): the
/// index reads them directly on APFS, never through the mount. None for other trees.
fn virtual_folders(root: &Path) -> Option<(PathBuf, PathBuf)> {
    #[cfg(target_os = "macos")]
    {
        if crate::sys::fs_type_name(root).ok()? != "layrfs" {
            return None;
        }
        let name = root.file_name()?.to_str()?;
        let meta = root.parent()?.parent()?.join("meta");
        let v = crate::views::all_in(&meta).into_iter().find(|v| v.name == name && v.mounted)?;
        let upper = meta.join("vlines").join(&v.line).join("upper");
        Some((v.lower, upper))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        None
    }
}

/// Standard input that `rg` would search: a file, a pipe or a socket.
fn stdin_readable() -> bool {
    match rustix::fs::fstat(rustix::stdio::stdin()) {
        Ok(st) => {
            let t = rustix::fs::FileType::from_raw_mode(st.st_mode);
            matches!(t, rustix::fs::FileType::RegularFile | rustix::fs::FileType::Fifo | rustix::fs::FileType::Socket)
        }
        Err(_) => false,
    }
}

/// The real tool: `LAYR_REAL_<TOOL>`, or the first one on PATH that is not this binary.
fn real_tool(tool: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(format!("LAYR_REAL_{}", tool.to_uppercase())) {
        return Some(PathBuf::from(p));
    }
    let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let c = dir.join(tool);
        let Ok(meta) = std::fs::metadata(&c) else { continue };
        if !meta.is_file() || std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 == 0 {
            continue;
        }
        if c.canonicalize().ok() != me {
            return Some(c);
        }
    }
    None
}

/// Run the real tool in place of this process.
fn exec_real(tool: &str, args: &[String]) -> i32 {
    let Some(path) = real_tool(tool) else {
        eprintln!("layr tool: {tool}: the real {tool} is not on PATH");
        return 127;
    };
    let err = std::process::Command::new(&path).arg0(tool).args(args).env("LAYR_TOOL_ACTIVE", "1").exec();
    eprintln!("layr tool: cannot run {}: {err}", path.display());
    126
}

/// Start the server in the background, detached from this process.
fn spawn_server() {
    let sock = socket_path();
    let Some(dir) = sock.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("layrfs.log"));
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    // The server outlives the client: it must not keep the client's folder in use (a virtual
    // line's mount could not be unmounted).
    cmd.args(["fs", "serve"]).stdin(std::process::Stdio::null()).env_remove("LAYR_TOOL_ACTIVE").current_dir("/");
    if let Ok(log) = log {
        if let Ok(l2) = log.try_clone() {
            cmd.stdout(l2);
        }
        cmd.stderr(log);
    }
    crate::sys::new_session(&mut cmd);
    let _ = cmd.spawn();
}

/// An environment switch: set and not empty.
fn flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// `layr tool <rg|find> <args>`, or the binary started under the tool's name.
pub fn run_tool(tool: &str, args: &[String]) -> i32 {
    if !TOOLS.contains(&tool) {
        eprintln!("layr tool: unknown tool {tool} (supported: {})", TOOLS.join(", "));
        return 2;
    }
    if std::env::var_os("LAYR_TOOL_ACTIVE").is_some() {
        return exec_real(tool, args); // started by a fallback: never loop
    }
    match serve_tool(tool, args) {
        Ok(code) => code,
        Err(why) => {
            if flag("LAYR_FS_STRICT") {
                eprintln!("layr tool: {tool} not served: {why}");
                return 125;
            }
            exec_real(tool, args)
        }
    }
}

/// The served tool's exit code, or why the real tool must run (nothing was printed then).
fn serve_tool(tool: &str, args: &[String]) -> Result<i32, String> {
    if flag("LAYR_FS_OFF") {
        return Err("LAYR_FS_OFF is set".into());
    }
    if rustix::termios::isatty(rustix::stdio::stdout()) {
        return Err("stdout is a terminal".into());
    }
    if tool == "rg"
        && std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|v| !v.is_empty())
        && !args.iter().any(|a| a == "--no-config")
    {
        return Err("RIPGREP_CONFIG_PATH adds arguments".into());
    }
    let cwd = std::env::current_dir().and_then(|d| d.canonicalize()).map_err(|e| format!("working folder: {e}"))?;
    let root = tree_root(&cwd).ok_or("not in a layr line or a git work tree")?;
    let path = socket_path();
    let sock = match connect(&path) {
        Ok(s) => s,
        Err(e) => {
            if UnixStream::connect(&path).is_err() {
                spawn_server(); // nothing listens yet; a foreign listener is never replaced here
            }
            return Err(format!("no layrfs server to use ({e})"));
        }
    };
    let req = serde_json::json!({
        "op": "tool",
        "version": version(),
        "tool": tool,
        "args": args,
        "root": root,
        "cwd": cwd,
        "stdin_readable": stdin_readable(),
    });
    write_frame(&mut &sock, b'q', &serde_json::to_vec(&req).unwrap()).map_err(|e| format!("request: {e}"))?;
    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr().lock());
    let mut wrote = false;
    loop {
        let (kind, data) = match read_frame(&mut &sock) {
            Ok(f) => f,
            Err(e) if !wrote => return Err(format!("the layrfs server did not answer: {e}")),
            Err(e) => {
                let _ = writeln!(err, "layr tool: the layrfs server stopped during {tool}: {e}");
                return Ok(2);
            }
        };
        match kind {
            b'o' => {
                wrote = true;
                if out.write_all(&data).is_err() {
                    return Ok(0); // the reader went away (`rg ... | head`)
                }
            }
            b'e' => {
                wrote = true;
                let _ = err.write_all(&data);
            }
            b'x' => {
                let _ = out.flush();
                let v: serde_json::Value = serde_json::from_slice(&data).unwrap_or_default();
                if let Some(why) = v.get("fallback") {
                    if !wrote {
                        return Err(why.as_str().unwrap_or("fallback").to_string());
                    }
                }
                return Ok(v.get("code").and_then(|c| c.as_i64()).unwrap_or(2) as i32);
            }
            other => return Err(format!("unexpected frame {other}")),
        }
    }
}

/// `layr fs serve|warm|status|stop`.
pub fn run_fs(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("serve") => {
            server::serve(&socket_path())?;
            Ok(0)
        }
        Some("warm") => {
            let wait = args.iter().any(|a| a == "--wait");
            let dir = match args.iter().skip(1).find(|a| !a.starts_with("--")) {
                Some(d) => PathBuf::from(d),
                None => std::env::current_dir()?,
            };
            let dir = dir.canonicalize().with_context(|| format!("cannot open {}", dir.display()))?;
            let root = tree_root(&dir).with_context(|| format!("{} is not in a layr line or a git work tree", dir.display()))?;
            let v = request(serde_json::json!({"op": "warm", "root": root, "wait": wait}), true)?;
            println!("{}", v.get("state").and_then(|s| s.as_str()).unwrap_or("unknown"));
            Ok(0)
        }
        Some("status") => {
            let v = request(serde_json::json!({"op": "status"}), false)?;
            let lines = v.get("lines").and_then(|l| l.as_array()).cloned().unwrap_or_default();
            println!(
                "server {} (pid {})",
                v.get("version").and_then(|s| s.as_str()).unwrap_or("?"),
                v.get("pid").unwrap_or(&0.into())
            );
            for l in lines {
                println!(
                    "{}  {}  entries {}  rg files {}  index ~{} MiB  idle {} s",
                    l["root"].as_str().unwrap_or("?"),
                    l["state"].as_str().unwrap_or("?"),
                    l["entries"],
                    l["visible"],
                    l["bytes"].as_u64().unwrap_or(0) >> 20,
                    l["idle_s"],
                );
            }
            Ok(0)
        }
        Some("stop") => {
            match request(serde_json::json!({"op": "stop"}), false) {
                Ok(_) | Err(_) => {}
            }
            Ok(0)
        }
        _ => bail!("usage: layr fs serve | warm [<dir>] [--wait] | status | stop"),
    }
}

/// Send a control request; `start` starts the server when it is not running.
fn request(req: serde_json::Value, start: bool) -> Result<serde_json::Value> {
    let path = socket_path();
    let sock = match connect(&path) {
        Ok(s) => s,
        Err(e) if start && UnixStream::connect(&path).is_err() => {
            spawn_server();
            let mut tries = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Ok(s) = connect(&path) {
                    break s;
                }
                tries += 1;
                if tries > 100 {
                    bail!("the layrfs server did not start ({e}); see {}", path.with_file_name("layrfs.log").display());
                }
            }
        }
        Err(e) => bail!("no layrfs server to use: {e}"),
    };
    let mut req = req;
    req["version"] = version().into();
    write_frame(&mut &sock, b'q', &serde_json::to_vec(&req)?)?;
    loop {
        let (kind, data) = read_frame(&mut &sock)?;
        if kind == b'x' {
            let v: serde_json::Value = serde_json::from_slice(&data)?;
            if let Some(f) = v.get("fallback") {
                bail!("{}", f.as_str().unwrap_or("the server refused the request"));
            }
            return Ok(v);
        }
    }
}
