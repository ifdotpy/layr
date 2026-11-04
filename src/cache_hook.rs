//! The cache-adapter hook of core (issue #167). Core stores the overlay property `prune`, and runs the
//! adapter that the binary registered under that name. Core names no adapter: `register` gets them from
//! `main`. An adapter runs in its own thread as the line owner, with a time limit. Nothing here fails a
//! command: the outcome is returned as a log line for the caller to print.

use crate::privs;
use layr_adapter::{CacheAdapter, Host, Outcome};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

/// The time limit of one hook: the adapter stops its processes at it and skips the folder.
pub const HOOK_LIMIT: Duration = Duration::from_secs(120);
/// How long core waits past the limit for an adapter that does not return by itself.
const GRACE: Duration = Duration::from_secs(10);

static ADAPTERS: OnceLock<Vec<Box<dyn CacheAdapter>>> = OnceLock::new();

/// Registers the adapters of the binary. `main` calls it once, before any command runs.
pub fn register(adapters: Vec<Box<dyn CacheAdapter>>) {
    let _ = ADAPTERS.set(adapters);
}

/// The registered adapters.
fn registered() -> &'static [Box<dyn CacheAdapter>] {
    ADAPTERS.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// What one hook run gets from core: the commands of the line owner and the time limit.
struct OwnerHost {
    uid: u32,
    path: String,
    until: Instant,
}

impl Host for OwnerHost {
    fn command(&self, program: &str) -> std::io::Result<Command> {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(program);
        // A clean environment: the owner's home, and the tool folders of the owner (`path`).
        privs::command_as(&mut cmd, self.uid, &[("PATH".to_string(), self.path.clone())])
            .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        cmd.process_group(0);
        Ok(cmd)
    }

    fn deadline(&self) -> Instant {
        self.until
    }
}

/// The search path of the owner's tools: its own bin folders, then the system folders.
fn owner_path(uid: u32) -> String {
    let home = privs::user(uid).map(|u| u.home).unwrap_or_else(|_| "/".into());
    format!("{home}/.cargo/bin:{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
}

/// Runs the hook for `folder` of a line of owner `uid` (group `gid`). `setting` is the overlay property:
/// `None` detects the adapter, `none` turns the hook off. Returns the log line: the adapter, and its outcome
/// (pruned, skipped with the reason, or timed out). Never fails.
pub fn run(setting: Option<&str>, folder: &Path, uid: u32, gid: u32, limit: Duration) -> String {
    run_with(registered(), setting, folder, uid, gid, limit, limit + GRACE)
}

/// `run` with the adapters given, and the time core waits for the adapter (`wait`; tests use fake adapters).
pub fn run_with(
    adapters: &'static [Box<dyn CacheAdapter>],
    setting: Option<&str>,
    folder: &Path,
    uid: u32,
    gid: u32,
    limit: Duration,
    wait: Duration,
) -> String {
    if setting == Some("none") {
        return format!("{}: prune is off", folder.display());
    }
    if uid == 0 {
        return format!("{}: not pruned: the line owner is root", folder.display());
    }
    let found = match setting {
        Some(name) => match adapters.iter().find(|a| a.name() == name) {
            Some(a) => a,
            None => return format!("{}: not pruned: unknown prune adapter '{name}'", folder.display()),
        },
        None => match adapters.iter().find(|a| a.detects(folder)) {
            Some(a) => a,
            None => return format!("{}: not pruned: no adapter matches", folder.display()),
        },
    };
    let host = OwnerHost { uid, path: owner_path(uid), until: Instant::now() + limit };
    let (tx, rx) = mpsc::channel::<Outcome>();
    let folder_owned: PathBuf = folder.to_path_buf();
    let name = found.name();
    std::thread::spawn(move || {
        let outcome = privs::as_user_fs(uid, gid, || Ok(found.prune(&host, &folder_owned)))
            .unwrap_or_else(|e| Outcome::Skipped { reason: format!("{e:#}") });
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(wait) {
        Ok(Outcome::Pruned { freed }) => format!("{}: pruned with {name}, freed {freed} bytes", folder.display()),
        Ok(Outcome::Skipped { reason }) => format!("{}: not pruned with {name}: {reason}", folder.display()),
        Err(_) => format!("{}: not pruned with {name}: timed out after {} s", folder.display(), limit.as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake adapter: `slow` sleeps past the limit, `fail` skips, `ok` prunes.
    struct Fake(&'static str);

    impl CacheAdapter for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn detects(&self, _folder: &Path) -> bool {
            false
        }
        fn prune(&self, _host: &dyn Host, _folder: &Path) -> Outcome {
            match self.0 {
                "slow" => {
                    std::thread::sleep(Duration::from_secs(3));
                    Outcome::Pruned { freed: 0 }
                }
                "fail" => Outcome::Skipped { reason: "stale".into() },
                _ => Outcome::Pruned { freed: 7 },
            }
        }
    }

    fn adapters() -> &'static [Box<dyn CacheAdapter>] {
        Box::leak(Box::new(vec![Box::new(Fake("ok")) as Box<dyn CacheAdapter>, Box::new(Fake("fail")), Box::new(Fake("slow"))]))
    }

    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    #[test]
    fn a_named_adapter_prunes_and_its_outcome_is_logged() {
        let log = run_with(adapters(), Some("ok"), Path::new("/x/target"), me(), me(), Duration::from_secs(5), Duration::from_secs(10));
        assert!(log.contains("pruned with ok, freed 7 bytes"), "{log}");
    }

    #[test]
    fn none_keeps_the_folder_and_an_unknown_name_is_skipped() {
        let log = run_with(adapters(), Some("none"), Path::new("/x/target"), me(), me(), Duration::from_secs(5), Duration::from_secs(10));
        assert!(log.contains("prune is off"), "{log}");
        let log = run_with(adapters(), Some("maven"), Path::new("/x/target"), me(), me(), Duration::from_secs(5), Duration::from_secs(10));
        assert!(log.contains("unknown prune adapter 'maven'"), "{log}");
    }

    #[test]
    fn a_failing_adapter_only_logs_its_reason() {
        let log = run_with(adapters(), Some("fail"), Path::new("/x/target"), me(), me(), Duration::from_secs(5), Duration::from_secs(10));
        assert!(log.contains("not pruned with fail: stale"), "{log}");
    }

    #[test]
    fn a_slow_adapter_is_cut_off_at_the_time_limit() {
        let started = Instant::now();
        let log = run_with(adapters(), Some("slow"), Path::new("/x/target"), me(), me(), Duration::from_millis(100), Duration::from_millis(300));
        assert!(log.contains("timed out"), "{log}");
        assert!(started.elapsed() < Duration::from_secs(2), "the hook waited for the adapter");
    }

    #[test]
    fn the_root_owner_is_never_pruned() {
        let log = run_with(adapters(), Some("ok"), Path::new("/x/target"), 0, 0, Duration::from_secs(5), Duration::from_secs(10));
        assert!(log.contains("the line owner is root"), "{log}");
    }
}
