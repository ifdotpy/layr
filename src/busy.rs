//! Whether a process uses a folder. A cache folder that a build writes must not be copied and
//! swapped (`repo::replace_folder`): the writes between the copy and the swap would be lost.

use anyhow::Result;
use std::path::Path;

/// A process that uses a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub pid: u32,
    /// The process name (`/proc/<pid>/comm`).
    pub name: String,
}

/// The first process that uses `path` or a path below it: its working directory, its root, an
/// open file or a memory map. Linux reads `/proc` (every process that `/proc` lets this user
/// read; as root, all). Other systems have no such check and report no user.
#[cfg(target_os = "linux")]
pub fn user_of(path: &Path) -> Result<Option<User>> {
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
        if uses(&dir, &path) {
            let name = std::fs::read_to_string(dir.join("comm")).map(|s| s.trim_end().to_string()).unwrap_or_default();
            return Ok(Some(User { pid, name }));
        }
    }
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
pub fn user_of(_path: &Path) -> Result<Option<User>> {
    Ok(None)
}

/// Whether the process at `dir` (its `/proc` entry) uses `path` or a path below it.
#[cfg(target_os = "linux")]
fn uses(dir: &Path, path: &Path) -> bool {
    let at = |link: &Path| {
        std::fs::read_link(link).is_ok_and(|p| {
            let p = p.to_string_lossy();
            Path::new(p.strip_suffix(" (deleted)").unwrap_or(&p)).starts_with(path)
        })
    };
    if at(&dir.join("cwd")) || at(&dir.join("root")) {
        return true;
    }
    if let Ok(fds) = std::fs::read_dir(dir.join("fd")) {
        if fds.flatten().any(|fd| at(&fd.path())) {
            return true;
        }
    }
    // A map line: address, perms, offset, device, inode, then the path (sixth field).
    std::fs::read_to_string(dir.join("maps")).is_ok_and(|maps| {
        maps.lines()
            .filter_map(|l| l.split_whitespace().nth(5))
            .any(|p| Path::new(p.strip_suffix(" (deleted)").unwrap_or(p)).starts_with(path))
    })
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
