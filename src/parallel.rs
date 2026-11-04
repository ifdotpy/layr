//! Shared process-wide parallelism policy.
use std::sync::OnceLock;

// Metadata walks and APFS operations contend for filesystem resources; retain the existing I/O cap.
const IO_LIMIT: usize = 4;
// Btrfs snapshot submissions share transaction commits but have a bounded I/O queue.
const SNAPSHOT_IO_LIMIT: usize = 8;
/// Minimum number of filesystem entries before the existing APFS batch loops use workers.
#[cfg(target_os = "macos")]
pub const MIN_IO_ITEMS: usize = 64;

/// Minimum decoded records per worker, measured on small and large replay inputs.
pub const MIN_RECORDS_PER_WORKER: usize = 100_000;

static THREADS: OnceLock<usize> = OnceLock::new();

fn parse(value: Option<&str>) -> Option<usize> {
    value.and_then(|s| s.parse::<usize>().ok()).filter(|&n| n > 0)
}

/// Take `LAYR_THREADS` from the caller of a command. The daemon runs each request in its own
/// forked process with the caller's environment in `Ctx.env`, not in the process environment.
pub fn configure(caller_value: Option<&str>) {
    if let Some(n) = parse(caller_value) {
        let _ = THREADS.set(n);
    }
}

/// Worker count: `LAYR_THREADS` of the caller (see `configure`) or of this process, else the
/// CPUs available to the process (affinity and container limits included).
pub fn threads() -> usize {
    *THREADS.get_or_init(|| {
        parse(std::env::var("LAYR_THREADS").ok().as_deref())
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
    })
}

/// Choose workers in proportion to the amount of work, capped by the caller's limit.
/// `min_units_per_worker` must be positive and uses the same units as `work_units`.
pub fn workers_for(work_units: usize, min_units_per_worker: usize) -> usize {
    assert!(min_units_per_worker > 0, "minimum work per worker must be positive");
    (work_units / min_units_per_worker).clamp(1, threads())
}

/// Create a pool only when a job has enough work to run in parallel.
/// A failed pool creation leaves callers free to execute serially.
pub fn pool(workers: usize) -> Option<rayon::ThreadPool> {
    if workers <= 1 {
        return None;
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    tune_parallel_allocator(workers);
    rayon::ThreadPoolBuilder::new().num_threads(workers).build().ok()
}

/// Glibc's default allocator settings can contend heavily on allocation-heavy jobs.
/// Daemon requests run in their own child process, so the settings apply to that command only.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
fn tune_parallel_allocator(workers: usize) {
    // SAFETY: mallopt accepts integer parameters and synchronizes allocator configuration itself.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 32 * 1024 * 1024);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 512 * 1024 * 1024);
        libc::mallopt(libc::M_ARENA_MAX, workers.min(i32::MAX as usize) as i32);
    }
}

pub fn io_threads() -> usize {
    threads().min(IO_LIMIT)
}
pub fn snapshot_threads() -> usize {
    threads().min(SNAPSHOT_IO_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_and_caps_workers() {
        assert_eq!(workers_for(0, 100), 1);
        assert_eq!(workers_for(199, 100), 1);
        assert_eq!(workers_for(200, 100), 2.min(threads()));
        assert_eq!(workers_for(usize::MAX, 1), threads());
    }

    #[test]
    #[should_panic(expected = "minimum work per worker must be positive")]
    fn rejects_zero_minimum() {
        workers_for(10, 0);
    }
}
