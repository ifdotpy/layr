//! Project policy and lock lifecycle for object maintenance, shared by CLI and service.

use super::exists;
use crate::store::Project;
use anyhow::{Context, Result};
use layr_objects::{Id, Store};
use std::path::PathBuf;

/// The head and this many total first-parent states stay hot for each line.
const HOT_STATES_DEFAULT: u64 = 5;

/// Automatic object maintenance cadence, in seconds. Zero explicitly disables it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceInterval {
    Disabled,
    Every(std::num::NonZeroU64),
}

impl MaintenanceInterval {
    pub const KEY: &str = "objects.maintenance-interval";
    pub const DEFAULT_SECONDS: u64 = 60;

    pub fn parse(value: &serde_json::Value) -> Result<Self> {
        let seconds = value
            .as_u64()
            .filter(|n| *n <= i64::MAX as u64 / 1000)
            .context("objects.maintenance-interval must be seconds (a nonnegative integer)")?;
        Ok(match std::num::NonZeroU64::new(seconds) {
            Some(seconds) => Self::Every(seconds),
            None => Self::Disabled,
        })
    }

    fn due(self, now: i64, last: i64) -> bool {
        match self {
            Self::Disabled => false,
            Self::Every(seconds) => u64::try_from(now.saturating_sub(last)).is_ok_and(|elapsed| elapsed >= seconds.get().saturating_mul(1000)),
        }
    }
}

/// Serialize maintenance without blocking ordinary saves. Take this before the project lock.
/// The periodic pass tries both locks and leaves its timestamp unchanged when either is busy.
pub(crate) fn maintenance_lock(p: &Project, periodic: bool) -> Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(p.meta_dir().join("objects-maintenance.lock"))?;
    let op =
        if periodic { rustix::fs::FlockOperation::NonBlockingLockExclusive } else { rustix::fs::FlockOperation::LockExclusive };
    match rustix::fs::flock(&file, op) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) if periodic => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The hot window of every line, shared by migration, manual repack and service maintenance.
pub fn hot_states(p: &Project) -> Result<std::collections::BTreeSet<String>> {
    let n = p.config_u64("objects.hot-states", HOT_STATES_DEFAULT);
    let mut out = std::collections::BTreeSet::new();
    for line in p.db.lines()? {
        let mut state = Some(line.head.clone());
        for _ in 0..n {
            let Some(id) = state else { break };
            state = p.db.state(&id)?.and_then(|s| s.parents.first().cloned());
            out.insert(id);
        }
    }
    Ok(out)
}

pub fn hot_trees(p: &Project, store: &Store) -> Result<Vec<Id>> {
    let mut out = Vec::new();
    for id in hot_states(p)? {
        if let Some(tree) = store.state_tree(&id)? {
            out.push(tree);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[derive(Clone, Copy)]
pub enum MaintenanceMode {
    Preview,
    Run,
    Periodic,
}

/// Plan under the project lock, prune before publication resumes, then compress outside it.
/// Each project (including overlays) follows this same path on all filesystems.
pub fn maintain(p: &Project, mode: MaintenanceMode) -> Result<Option<layr_objects::store::MaintenancePlan>> {
    use layr_objects::store::MaintenanceLimit;
    if !exists(p) {
        return Ok(None);
    }
    let periodic = matches!(mode, MaintenanceMode::Periodic);
    let now = crate::ids::now_ms();
    let stamp = p.meta_dir().join("objects-maintained");
    let due = || -> Result<bool> {
        let value =
            p.config_value(MaintenanceInterval::KEY)?.unwrap_or_else(|| serde_json::json!(MaintenanceInterval::DEFAULT_SECONDS));
        let interval = MaintenanceInterval::parse(&value)?;
        let last = std::fs::read_to_string(&stamp).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok(interval.due(now, last))
    };
    if periodic && !due()? {
        return Ok(None);
    }
    let Some(_maintenance) = maintenance_lock(p, periodic || p.holds_lock())? else { return Ok(None) };
    let lock = if periodic {
        let crate::store::LockAttempt::Acquired(lock) = p.try_lock()? else { return Ok(None) };
        Some(lock)
    } else if p.holds_lock() {
        None
    } else {
        Some(p.lock_maintenance()?)
    };
    if periodic && !due()? {
        return Ok(None);
    }
    #[allow(unused_mut)]
    let Some(mut store) = Store::try_open_for_prune(&super::dir(p), layr_objects::Keying::Plain)?
    else {
        if periodic {
            return Ok(None);
        }
        anyhow::bail!("object store is in use; retry gc after its readers/imports finish");
    };
    #[cfg(target_os = "macos")]
    {
        store.clone_hook = Some(crate::sys::apfs_unseal_fd);
    }
    let limit = if periodic { MaintenanceLimit::PERIODIC } else { MaintenanceLimit::UNLIMITED };
    let plan = store.plan_maintenance(&p.db.kept_state_ids()?, &hot_trees(p, &store)?, limit)?;
    if !matches!(mode, MaintenanceMode::Preview) {
        store.prune_planned(&plan)?;
    }
    store.finish_prune()?;
    drop(lock);
    if !matches!(mode, MaintenanceMode::Preview) {
        // Test barrier: prove a user can acquire the project lock during compression.
        if let Some(dir) = std::env::var_os("LAYR_TEST_OBJECTS_REPACK_BARRIER") {
            let dir = PathBuf::from(dir);
            std::fs::write(dir.join("ready"), b"")?;
            let start = std::time::Instant::now();
            while !dir.join("release").exists() && start.elapsed() < std::time::Duration::from_secs(5) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        store.repack_planned(&plan)?;
        crate::fsutil::atomic_write(&stamp, now.to_string().as_bytes(), 0o600)?;
    }
    Ok(Some(plan))
}

/// Nested overlay projects are not returned by Store::project_names.
pub fn periodic(repo: &crate::repo::Repo) {
    let run = |p: &Project| {
        if let Err(error) = maintain(p, MaintenanceMode::Periodic) {
            eprintln!("layr objects ({}): {error:#}", p.name);
        }
    };
    run(&repo.p);
    match repo.maintenance_overlay_projects() {
        Ok(Some(projects)) => {
            for project in projects {
                run(&project);
            }
        }
        Ok(None) => {} // A busy recovery leaves overlays due for the next tick.
        Err(error) => eprintln!("layr objects ({}): {error:#}", repo.p.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_interval_is_typed_and_checked() -> anyhow::Result<()> {
        use serde_json::json;
        assert_eq!(MaintenanceInterval::parse(&json!(0))?, MaintenanceInterval::Disabled);
        let interval = MaintenanceInterval::parse(&json!(60))?;
        assert!(!interval.due(60_999, 1000));
        assert!(interval.due(61_000, 1000));
        assert!(!MaintenanceInterval::Disabled.due(i64::MAX, 0));
        for value in [json!(-1), json!("60"), json!(true), json!(1.5), json!(u64::MAX)] {
            assert!(MaintenanceInterval::parse(&value).is_err(), "{value}");
        }
        Ok(())
    }
}
