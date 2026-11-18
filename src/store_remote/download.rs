//! The one bounded worker pool of the transport: downloads, listings and uploads. A job keeps its byte reservation
//! until the caller has consumed its result.

use anyhow::{anyhow, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};

pub(super) const JOBS: usize = 8;
pub(super) const BYTE_BUDGET: u64 = 64 << 20;
/// Resident bytes of the uploads of one state (parts and chunk packs in each representation), and of the chunk packs
/// that a reader decodes ahead.
pub(super) const TRANSFER_BUFFER_BUDGET: usize = 256 << 20;

/// A read proof that the gateway refused even when new. The optional batch routes treat it as "no such route".
#[derive(Debug)]
pub(super) struct ProofRejected(pub(super) String);
impl std::fmt::Display for ProofRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProofRejected {}

/// Fetch `count` indexed jobs on at most `jobs` workers and consume them on the calling thread.
pub(super) fn run<T: Send>(
    count: usize,
    jobs: usize,
    budget: u64,
    weight: impl Fn(usize) -> u64 + Sync,
    fetch: impl Fn(usize) -> Result<T> + Sync,
    mut consume: impl FnMut(usize, T) -> Result<()>,
) -> Result<()> {
    std::thread::scope(|scope| {
        let mut pool = Pool::new(scope, jobs.min(count), budget, &fetch);
        for i in 0..count {
            pool.submit(i, weight(i), &mut consume)?;
        }
        pool.finish(&mut consume)
    })
}

type Job<J> = (usize, u64, J);
type Done<T> = (usize, u64, Result<T>);

/// Workers on scoped threads, started as the submissions need them. The calling thread submits jobs with a byte
/// weight and consumes their results. A submission first consumes finished results until its weight fits the
/// budget; a weight above the budget waits until nothing else is reserved, then runs alone. After a failed job or
/// consumer, queued jobs are skipped; dropping the pool also skips them, and the scope joins the running ones.
pub(super) struct Pool<'scope, 'env, J, T> {
    scope: &'scope std::thread::Scope<'scope, 'env>,
    work: Arc<dyn Fn(J) -> Result<T> + Send + Sync + 'scope>,
    jobs: Option<mpsc::Sender<Job<J>>>,
    queue: Arc<Mutex<mpsc::Receiver<Job<J>>>>,
    done: mpsc::Sender<Done<T>>,
    results: mpsc::Receiver<Done<T>>,
    cancelled: Arc<AtomicBool>,
    quick: bool,
    workers: usize,
    started: usize,
    budget: u64,
    used: u64,
    outstanding: usize,
    submitted: usize,
}

impl<'scope, 'env, J: Send + 'scope, T: Send + 'scope> Pool<'scope, 'env, J, T> {
    pub fn new(
        scope: &'scope std::thread::Scope<'scope, 'env>,
        workers: usize,
        budget: u64,
        work: impl Fn(J) -> Result<T> + Send + Sync + 'scope,
    ) -> Self {
        let (jobs, queue) = mpsc::channel();
        let (done, results) = mpsc::channel();
        Self {
            scope,
            work: Arc::new(work),
            jobs: Some(jobs),
            queue: Arc::new(Mutex::new(queue)),
            done,
            results,
            cancelled: Arc::default(),
            quick: super::gateway::is_quick(),
            workers: workers.max(1),
            started: 0,
            budget: budget.max(1),
            used: 0,
            outstanding: 0,
            submitted: 0,
        }
    }

    /// Queue `job`; returns its index, the first argument of `consume`.
    pub fn submit(&mut self, job: J, weight: u64, consume: &mut dyn FnMut(usize, T) -> Result<()>) -> Result<usize> {
        let weight = weight.clamp(1, self.budget);
        self.wait_for(weight, consume)?;
        if self.outstanding >= self.started && self.started < self.workers {
            self.start_worker();
        }
        let index = self.submitted;
        let jobs = self.jobs.as_ref().ok_or_else(|| anyhow!("the transfer pool is finished"))?;
        jobs.send((index, weight, job)).map_err(|_| anyhow!("transfer workers stopped"))?;
        self.submitted += 1;
        self.outstanding += 1;
        self.used += weight;
        Ok(index)
    }

    /// Consume finished results until `weight` more bytes fit the budget (or nothing is reserved).
    pub fn wait_for(&mut self, weight: u64, consume: &mut dyn FnMut(usize, T) -> Result<()>) -> Result<()> {
        while self.outstanding > 0 && self.used + weight.min(self.budget) > self.budget {
            self.next(consume)?;
        }
        Ok(())
    }

    /// Consume the next result to finish, if a job is outstanding.
    pub fn next_result(&mut self, consume: &mut dyn FnMut(usize, T) -> Result<()>) -> Result<()> {
        if self.outstanding > 0 {
            self.next(consume)?;
        }
        Ok(())
    }

    /// Consume every outstanding result.
    pub fn finish(mut self, consume: &mut dyn FnMut(usize, T) -> Result<()>) -> Result<()> {
        self.jobs = None;
        while self.outstanding > 0 {
            self.next(consume)?;
        }
        Ok(())
    }

    fn next(&mut self, consume: &mut dyn FnMut(usize, T) -> Result<()>) -> Result<()> {
        let (index, weight, result) = self.results.recv().map_err(|_| anyhow!("transfer workers stopped"))?;
        let consumed = result.and_then(|value| consume(index, value));
        self.outstanding -= 1;
        self.used -= weight;
        if consumed.is_err() {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        consumed
    }

    fn start_worker(&mut self) {
        let (queue, done, cancelled, work, quick) =
            (self.queue.clone(), self.done.clone(), self.cancelled.clone(), self.work.clone(), self.quick);
        self.started += 1;
        self.scope.spawn(move || loop {
            let Ok((index, weight, job)) = queue.lock().map_err(drop).and_then(|queue| queue.recv().map_err(drop)) else {
                return;
            };
            let result = if cancelled.load(Ordering::SeqCst) {
                Err(anyhow!("cancelled after an earlier transfer failed"))
            } else {
                // Catch panics as well: every job must answer, or the caller would wait for it forever.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if quick {
                        super::gateway::quick(|| work(job))
                    } else {
                        work(job)
                    }
                }))
                .unwrap_or_else(|_| Err(anyhow!("transfer worker panicked")))
            };
            // Answer before cancelling: the failure reaches the caller ahead of the jobs that it makes skip.
            let failed = result.is_err();
            let _ = done.send((index, weight, result));
            if failed {
                cancelled.store(true, Ordering::SeqCst);
            }
        });
    }
}

impl<J, T> Drop for Pool<'_, '_, J, T> {
    fn drop(&mut self) {
        // An abandoned pool runs nothing more: queued jobs are skipped, and the scope joins the running ones.
        self.cancelled.store(true, Ordering::SeqCst);
        self.jobs = None;
    }
}

/// Application HTTP bodies consumed/supplied by the gateway transport, excluding headers and TLS framing. A buffered
/// request body counts in full at each attempt; streamed request bodies and responses count as HTTP reads them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferStats {
    pub requests: u64,
    pub downloaded: u64,
    pub uploaded: u64,
    pub control: u64,
}

#[derive(Default)]
pub(super) struct Counters {
    pub requests: std::sync::atomic::AtomicU64,
    pub downloaded: std::sync::atomic::AtomicU64,
    pub uploaded: std::sync::atomic::AtomicU64,
    pub control: std::sync::atomic::AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> TransferStats {
        TransferStats {
            requests: self.requests.load(Ordering::Relaxed),
            downloaded: self.downloaded.load(Ordering::Relaxed),
            uploaded: self.uploaded.load(Ordering::Relaxed),
            control: self.control.load(Ordering::Relaxed),
        }
    }
}

pub(super) struct Counted<R> {
    pub inner: R,
    pub counters: std::sync::Arc<Counters>,
    pub payload: bool,
    pub upload: bool,
}
impl<R: std::io::Read> std::io::Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let counter = if self.upload {
            &self.counters.uploaded
        } else if self.payload {
            &self.counters.downloaded
        } else {
            &self.counters.control
        };
        counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, Barrier};

    #[test]
    fn scheduler_holds_byte_reservations_until_consumed() {
        for limit in [4, 8] {
            let active = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);
            let together = Barrier::new(limit);
            let mut seen = vec![false; 32];
            run(
                32,
                8,
                limit as u64 * 4,
                |_| 4,
                |i| {
                    let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    together.wait();
                    Ok(i)
                },
                |i, body| {
                    assert_eq!(body, i);
                    assert!(!seen[i]);
                    seen[i] = true;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .unwrap();
            assert!(seen.into_iter().all(|b| b));
            assert_eq!(peak.load(Ordering::SeqCst), limit);
            assert_eq!(active.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn scheduler_cancellation_drains_in_flight_workers() {
        let started = Arc::new(AtomicUsize::new(0));
        let together = Barrier::new(2);
        let mut consumed = 0;
        let result = run(
            100,
            2,
            2,
            |_| 1,
            |_| {
                started.fetch_add(1, Ordering::SeqCst);
                together.wait();
                Ok(vec![1])
            },
            |_, _| {
                consumed += 1;
                Err(anyhow!("consumer cancelled"))
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "consumer cancelled");
        assert_eq!(consumed, 1);
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }

    #[test]
    #[allow(clippy::panic)] // This test intentionally exercises panic-to-error conversion.
    fn scheduler_panics_become_errors_and_release_workers() {
        let result = run(100, 2, 1, |_| 1, |_| -> Result<()> { panic!("failed download") }, |_, _| Ok(()));
        assert_eq!(result.unwrap_err().to_string(), "transfer worker panicked");
    }

    #[test]
    fn counters_include_bytes_before_a_body_read_failure() {
        struct Partial(bool);
        impl std::io::Read for Partial {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::Error::other("broken body"));
                }
                self.0 = true;
                buf[..3].copy_from_slice(b"abc");
                Ok(3)
            }
        }
        let counters = Arc::new(Counters::default());
        let mut reader = Counted { inner: Partial(false), counters: counters.clone(), payload: true, upload: false };
        let mut bytes = Vec::new();
        assert!(std::io::Read::read_to_end(&mut reader, &mut bytes).is_err());
        assert_eq!(counters.snapshot().downloaded, 3);
    }
}
