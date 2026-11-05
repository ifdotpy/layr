//! Whether a process uses a folder. A cache folder that a build writes must not be copied and
//! swapped (`repo::replace_folder`): the writes between the copy and the swap would be lost. A
//! process that only reads the folder keeps its old files after the swap and loses nothing, so the
//! cache check looks for writers only (`writer_of`).

use anyhow::Result;
use std::path::Path;

/// A process that uses a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub pid: u32,
    /// The process name (`/proc/<pid>/comm`).
    pub name: String,
    /// How it uses the folder: "working directory", "root", "writes <path>", "reads <path>" or
    /// "maps <path>".
    pub how: String,
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid {} ({}, {})", self.pid, self.name, self.how)
    }
}

/// The first process that uses `path` or a path below it: its working directory, its root, an
/// open file or a memory map. Linux reads `/proc` (every process that `/proc` lets this user
/// read; as root, all). Only Linux has a caller (the snapshot reset of a line).
#[cfg(target_os = "linux")]
pub fn user_of(path: &Path) -> Result<Option<User>> {
    find(path, false)
}

/// The first process that can write into `path` or a path below it: its working directory (a
/// `make` writes relative paths between the compiler runs), its root, a file open for writing (a
/// build holds its lock file open read-write) or a shared writable map. A process that only reads
/// files below `path` or runs a program from it is no writer. Other systems than Linux have no
/// such check and report no writer.
pub fn writer_of(path: &Path) -> Result<Option<User>> {
    find(path, true)
}

#[cfg(target_os = "linux")]
fn find(path: &Path, writers: bool) -> Result<Option<User>> {
    let Ok(path) = path.canonicalize() else {
        return Ok(None);
    };
    let mut pids: Vec<u32> =
        std::fs::read_dir("/proc")?.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok())).collect();
    pids.sort_unstable();
    // LAYR_TRACE shows the canonical path that the /proc links are compared with.
    let _t = crate::trace::named(|| format!("busy check {} ({} processes)", path.display(), pids.len()));
    for pid in pids {
        let dir = std::path::PathBuf::from("/proc").join(pid.to_string());
        if let Some(how) = uses(&dir, &path, writers) {
            let name = std::fs::read_to_string(dir.join("comm")).map(|s| s.trim_end().to_string()).unwrap_or_default();
            return Ok(Some(User { pid, name, how }));
        }
    }
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
fn find(_path: &Path, _writers: bool) -> Result<Option<User>> {
    Ok(None)
}

/// How the process at `dir` (its `/proc` entry) uses `path` or a path below it, or None. With
/// `writers`, a read-only open file and a map that does not write to its file do not count.
#[cfg(target_os = "linux")]
fn uses(dir: &Path, path: &Path, writers: bool) -> Option<String> {
    let below = |p: &str| Path::new(p.strip_suffix(" (deleted)").unwrap_or(p)).starts_with(path);
    let at = |link: &Path| std::fs::read_link(link).ok().map(|p| p.to_string_lossy().into_owned()).filter(|p| below(p));
    if at(&dir.join("cwd")).is_some() {
        return Some("working directory".into());
    }
    if at(&dir.join("root")).is_some() {
        return Some("root".into());
    }
    if let Ok(fds) = std::fs::read_dir(dir.join("fd")) {
        for fd in fds.flatten() {
            let Some(p) = at(&fd.path()) else { continue };
            let writes = open_for_write(&dir.join("fdinfo").join(fd.file_name()));
            if writes {
                return Some(format!("writes {p}"));
            }
            if !writers {
                return Some(format!("reads {p}"));
            }
        }
    }
    // A map line: address, perms, offset, device, inode, then the path (sixth field). Only a
    // shared writable map ("rw-s") writes to its file.
    let maps = std::fs::read_to_string(dir.join("maps")).ok()?;
    maps.lines().find_map(|l| {
        let mut fields = l.split_whitespace();
        let perms = fields.nth(1)?;
        let p = fields.nth(3)?;
        (below(p) && (!writers || perms.as_bytes().get(1) == Some(&b'w') && perms.as_bytes().get(3) == Some(&b's')))
            .then(|| format!("maps {p}"))
    })
}

/// Whether the open file that `fdinfo` describes (`/proc/<pid>/fdinfo/<fd>`) was opened for
/// writing: its access mode (the low two bits of the octal "flags:" line) is not read-only. An
/// unreadable entry counts as a writer.
#[cfg(target_os = "linux")]
fn open_for_write(fdinfo: &Path) -> bool {
    let Ok(info) = std::fs::read_to_string(fdinfo) else {
        return true;
    };
    info.lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .and_then(|f| u32::from_str_radix(f.trim(), 8).ok())
        .is_none_or(|flags| flags & 0o3 != 0)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn temp_folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("layr-busy-{name}-{}", crate::ids::new_id()));
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        dir
    }

    /// Wait until `path` is busy (a child process needs a moment to start), up to a few seconds.
    fn wait_busy(path: &Path) -> Option<User> {
        for _ in 0..100 {
            if let Some(u) = user_of(path).unwrap() {
                return Some(u);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }

    #[test]
    fn folder_with_no_process_is_not_busy() {
        let dir = temp_folder("none");
        assert_eq!(user_of(&dir.join("target")).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_file_below_the_folder_makes_it_busy() {
        let dir = temp_folder("fd");
        let lock = dir.join("target/debug/.cargo-lock");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exec 3>\"$1\"; sleep 30")
            .arg("sh")
            .arg(&lock)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let user = wait_busy(&dir.join("target"));
        child.kill().unwrap();
        child.wait().unwrap();
        let user = user.expect("the open file makes the folder busy");
        assert_eq!(user.pid, child.id());
        assert!(user.how.starts_with("writes "), "{user}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Wait until `path` has a user (any kind), then return the writer of it.
    fn writer_when_used(path: &Path) -> Option<User> {
        wait_busy(path).expect("the child uses the folder");
        writer_of(path).unwrap()
    }

    #[test]
    fn file_open_for_writing_makes_a_writer() {
        let dir = temp_folder("wfd");
        let lock = dir.join("target/debug/.cargo-lock");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exec 3<>\"$1\"; sleep 30")
            .arg("sh")
            .arg(&lock)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let user = writer_when_used(&dir.join("target"));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(user.map(|u| u.pid), Some(child.id()), "a read-write lock file makes a writer");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reader_is_no_writer() {
        let dir = temp_folder("rfd");
        let out = dir.join("target/debug/out");
        std::fs::write(&out, "built").unwrap();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exec 3<\"$1\"; sleep 30")
            .arg("sh")
            .arg(&out)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let user = writer_when_used(&dir.join("target"));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(user, None, "a process that only reads is no writer");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn working_directory_below_the_folder_makes_it_busy() {
        let dir = temp_folder("cwd");
        let mut child = Command::new("sleep").arg("30").current_dir(dir.join("target/debug")).spawn().unwrap();
        let user = wait_busy(&dir.join("target"));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(user.is_some(), "a working directory below the folder makes it busy");
        let mut child = Command::new("sleep").arg("30").current_dir(dir.join("target/debug")).spawn().unwrap();
        let writer = writer_when_used(&dir.join("target"));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(writer.map(|u| u.how), Some("working directory".to_string()), "a make writes relative paths");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
