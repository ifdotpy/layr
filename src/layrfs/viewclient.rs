//! The client side of the FUSE view of a line on Linux (docs/layrfs-fuse.md): find the view
//! around a folder and talk to its server. Built without the `layrfs-fuse` feature too, so every
//! layr binary uses a view that another binary serves.
//!
//! The server of a view runs as the service user and mounts with the source name
//! `layrfs-view:<socket>`. A client trusts the process behind that socket only when it runs as
//! the user that owns the mount (`user_id=` in `/proc/self/mountinfo`): that user serves every
//! byte of the view anyway.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// The start of the source name of a view's mount; the socket path follows.
pub const FSNAME_PREFIX: &str = "layrfs-view:";

pub struct Mount {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub owner: u32,
}

/// Undo the octal escapes of mountinfo (`\040` for a space).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(((b[i + 1] - b'0') << 6) | ((b[i + 2] - b'0') << 3) | (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The view mounted at `dir` or above it, if any.
pub fn find(dir: &Path) -> Option<Mount> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<Mount> = None;
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let l: Vec<&str> = left.split(' ').collect();
        let r: Vec<&str> = right.split(' ').collect();
        // A mount by root (the service's hub) shows the type without its subtype.
        if l.len() < 5 || r.len() < 3 || !(r[0] == "fuse.layrfs" || r[0] == "fuse") {
            continue;
        }
        let source = unescape(r[1]);
        let Some(socket) = source.strip_prefix(FSNAME_PREFIX) else { continue };
        let root = PathBuf::from(unescape(l[4]));
        if !dir.starts_with(&root) {
            continue;
        }
        let Some(owner) = r[2].split(',').find_map(|o| o.strip_prefix("user_id=")).and_then(|u| u.parse().ok()) else {
            continue;
        };
        if best.as_ref().is_none_or(|b| root.components().count() > b.root.components().count()) {
            best = Some(Mount { root, socket: PathBuf::from(socket), owner });
        }
    }
    best
}

/// Connect to the server of a view; refuse a process that is not the mount's owner.
pub fn connect(m: &Mount) -> Result<UnixStream, String> {
    let sock = UnixStream::connect(&m.socket).map_err(|e| format!("{}: {e}", m.socket.display()))?;
    match rustix::net::sockopt::socket_peercred(&sock) {
        Ok(c) if c.uid.as_raw() == m.owner => Ok(sock),
        Ok(c) => Err(format!(
            "the process behind {} runs as uid {}, the view belongs to uid {}",
            m.socket.display(),
            c.uid.as_raw(),
            m.owner
        )),
        Err(e) => Err(format!("cannot check the process behind {}: {e}", m.socket.display())),
    }
}

/// True when this user's git has global excludes. rg applies them, but the view's server (another
/// user) cannot read them, so its answer could differ: the real tool runs then.
pub fn global_excludes() -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".config")));
    if xdg.as_ref().is_some_and(|x| x.join("git/ignore").exists()) {
        return true;
    }
    let mut configs: Vec<PathBuf> = vec![PathBuf::from("/etc/gitconfig")];
    if let Some(h) = &home {
        configs.push(h.join(".gitconfig"));
    }
    if let Some(x) = &xdg {
        configs.push(x.join("git/config"));
    }
    configs.iter().any(|c| std::fs::read_to_string(c).is_ok_and(|t| t.to_ascii_lowercase().contains("excludesfile")))
}

/// `layr fs view fsmonitor <version> <token>`: git's fsmonitor hook (protocol version 2). git
/// runs it in the top folder of the work tree. Exit code 1 makes git scan everything.
pub fn fsmonitor(args: &[String]) -> i32 {
    let (Some(version), token) = (args.first(), args.get(1).map(String::as_str).unwrap_or("")) else {
        eprintln!("usage: layr fs view fsmonitor <version> <token>");
        return 1;
    };
    // Quiet: git runs the hook of the shared config in every worktree of the folder, and one that
    // is not a view (`git worktree add` to a plain folder) gets a full scan without a message.
    // Git tries version 1 after a failed version 2.
    if version != "2" {
        return 1;
    }
    let Ok(cwd) = std::env::current_dir().and_then(|d| d.canonicalize()) else { return 1 };
    let Some(m) = find(&cwd) else { return 1 };
    let sock = match connect(&m) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("layr fs view fsmonitor: {e}");
            return 1;
        }
    };
    let req = serde_json::json!({ "op": "fsmonitor", "token": token });
    if super::write_frame(&mut &sock, b'q', req.to_string().as_bytes()).is_err() {
        return 1;
    }
    let mut out = std::io::stdout().lock();
    loop {
        match super::read_frame(&mut &sock) {
            Ok((b'o', data)) => {
                if out.write_all(&data).is_err() {
                    return 1;
                }
            }
            Ok((b'x', data)) => {
                let _ = out.flush();
                let v: serde_json::Value = serde_json::from_slice(&data).unwrap_or_default();
                return v.get("code").and_then(|c| c.as_i64()).unwrap_or(1) as i32;
            }
            _ => return 1,
        }
    }
}
