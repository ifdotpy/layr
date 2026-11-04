//! Shared process-wide parallelism policy.
use std::sync::OnceLock;

// Metadata walks and APFS operations contend for filesystem resources; retain the existing I/O cap.
const IO_LIMIT: usize = 4;
// Btrfs snapshot submissions share transaction commits but have a bounded I/O queue.
const SNAPSHOT_IO_LIMIT: usize = 8;
/// Minimum number of filesystem entries before the existing APFS batch loops use workers.
#[cfg(target_os = "macos")]
pub const MIN_IO_ITEMS: usize = 64;

pub fn threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        std::env::var("LAYR_THREADS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
    })
}

pub fn io_threads() -> usize {
    threads().min(IO_LIMIT)
}
pub fn snapshot_threads() -> usize {
    threads().min(SNAPSHOT_IO_LIMIT)
}
