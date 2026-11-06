//! Scoped pools keep worker threads out of the service's forking parent.
use std::cell::Cell;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};

static FORKING_PARENT: AtomicBool = AtomicBool::new(false);

pub fn mark_forking_parent() { FORKING_PARENT.store(true, Ordering::Relaxed); }
pub fn mark_worker() { FORKING_PARENT.store(false, Ordering::Relaxed); }
pub fn is_forking_parent() -> bool { FORKING_PARENT.load(Ordering::Relaxed) }

#[derive(Clone, Copy)]
pub enum Width {
    Items(NonZeroUsize),
    #[cfg_attr(all(target_os = "linux", feature = "overlayfs"), allow(dead_code))]
    Unbounded,
}

thread_local! {
    static IN_POOL: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy)]
pub enum Priority {
    Foreground,
    Background,
}

#[derive(Debug)]
pub enum PoolError {
    ForkingParent,
    Build(rayon::ThreadPoolBuildError),
}
impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForkingParent => f.write_str("cannot start worker threads in the forking parent"),
            Self::Build(error) => write!(f, "cannot start worker threads: {error}"),
        }
    }
}
impl std::error::Error for PoolError {}

/// The outermost operation chooses the pool and priority; nested calls run on that pool.
/// A foreground pool inherits its creator's priority, so its caller must not be lowered.
/// All workers are joined before returning; never fork inside this operation.
pub fn run<R: Send>(priority: Priority, width: Width, work: impl FnOnce() -> R + Send) -> Result<R, PoolError> {
    run_for_role(priority, width, is_forking_parent(), work)
}

fn run_for_role<R: Send>(priority: Priority, width: Width, forking_parent: bool, work: impl FnOnce() -> R + Send) -> Result<R, PoolError> {
    if forking_parent { return Err(PoolError::ForkingParent); }
    if IN_POOL.with(Cell::get) {
        return Ok(work());
    }
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let threads = match width { Width::Items(items) => threads.min(items.get()), Width::Unbounded => threads };
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(move |_| {
            IN_POOL.with(|inside| inside.set(true));
            if matches!(priority, Priority::Background) {
                crate::sys::lower_thread_priority();
            }
        })
        .build_scoped(|thread| thread.run(), |pool| pool.install(work))
        .map_err(PoolError::Build)
}

#[cfg(test)]
mod tests {
    use super::{run, Priority, Width};
    use std::num::NonZeroUsize;
    use rayon::prelude::*;

    #[test]
    fn nested_work_stays_in_background_pool() {
        run(Priority::Background, Width::Unbounded, || {
            let outer = rayon::current_num_threads();
            run(Priority::Foreground, Width::Unbounded, || {
                (0..outer * 4).into_par_iter().for_each(|_| {
                    assert!(super::IN_POOL.with(std::cell::Cell::get));
                    assert!(rayon::current_thread_index().is_some());
                    #[cfg(target_os = "linux")]
                    assert_eq!(crate::sys::test_thread_priority(), (19, (2 << 13) | 7));
                });
            }).unwrap();
        }).unwrap();
    }

    #[test]
    fn width_limits_the_pool_to_available_work() {
        let threads = run(Priority::Foreground, Width::Items(NonZeroUsize::new(2).unwrap()), rayon::current_num_threads).unwrap();
        assert_eq!(threads, std::thread::available_parallelism().unwrap().get().min(2));
    }

    #[test]
    fn forking_parent_cannot_start_a_pool() {
        let result = super::run_for_role(Priority::Foreground, Width::Items(NonZeroUsize::new(2).unwrap()), true, || panic!("must not run"));
        assert!(matches!(result, Err(super::PoolError::ForkingParent)));
    }

    #[test]
    fn build_errors_are_reported() {
        let error = match rayon::ThreadPoolBuilder::new()
            .spawn_handler(|_| Err(std::io::Error::other("test failure")))
            .build() {
                Ok(_) => panic!("test builder unexpectedly succeeded"),
                Err(error) => error,
            };
        assert!(super::PoolError::Build(error).to_string().starts_with("cannot start worker threads:"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pool_threads_keep_their_priorities() {
        let caller = crate::sys::test_thread_priority();
        run(Priority::Foreground, Width::Unbounded, || {
            assert_eq!(crate::sys::test_thread_priority(), caller);
        }).unwrap();
        run(Priority::Background, Width::Unbounded, || {
            assert_eq!(crate::sys::test_thread_priority(), (19, (2 << 13) | 7));
        }).unwrap();
    }
}
