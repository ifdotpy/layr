//! Scoped pools keep worker threads out of the service's forking parent.
use std::cell::Cell;

thread_local! {
    static IN_POOL: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy)]
pub enum Priority {
    Foreground,
    Background,
}

#[derive(Debug)]
pub struct PoolError(rayon::ThreadPoolBuildError);
impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot start worker threads: {}", self.0)
    }
}
impl std::error::Error for PoolError {}

/// The outermost operation chooses the pool and priority; nested calls run on that pool.
/// A foreground pool inherits its creator's priority, so its caller must not be lowered.
/// All workers are joined before returning; never fork inside this operation.
pub fn run<R: Send>(priority: Priority, work: impl FnOnce() -> R + Send) -> Result<R, PoolError> {
    if IN_POOL.with(Cell::get) {
        return Ok(work());
    }
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(move |_| {
            IN_POOL.with(|inside| inside.set(true));
            if matches!(priority, Priority::Background) {
                crate::sys::lower_thread_priority();
            }
        })
        .build_scoped(|thread| thread.run(), |pool| pool.install(work))
        .map_err(PoolError)
}

#[cfg(test)]
mod tests {
    use super::{run, Priority};
    use rayon::prelude::*;

    #[test]
    fn nested_work_stays_in_background_pool() {
        run(Priority::Background, || {
            let outer = rayon::current_num_threads();
            run(Priority::Foreground, || {
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
    fn build_errors_are_reported() {
        let error = match rayon::ThreadPoolBuilder::new()
            .spawn_handler(|_| Err(std::io::Error::other("test failure")))
            .build() {
                Ok(_) => panic!("test builder unexpectedly succeeded"),
                Err(error) => error,
            };
        assert!(super::PoolError(error).to_string().starts_with("cannot start worker threads:"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pool_threads_keep_their_priorities() {
        let caller = crate::sys::test_thread_priority();
        run(Priority::Foreground, || {
            assert_eq!(crate::sys::test_thread_priority(), caller);
        }).unwrap();
        run(Priority::Background, || {
            assert_eq!(crate::sys::test_thread_priority(), (19, (2 << 13) | 7));
        }).unwrap();
    }
}
