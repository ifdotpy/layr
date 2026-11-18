//! Bounded blocking downloads. A result keeps its byte reservation until the consumer returns.

use anyhow::{anyhow, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Condvar, Mutex,
};

pub(super) const JOBS: usize = 8;
pub(super) const BYTE_BUDGET: u64 = 64 << 20;

/// A read proof that the gateway refused even when new. The optional batch routes treat it as "no such route".
#[derive(Debug)]
pub(super) struct ProofRejected(pub(super) String);
impl std::fmt::Display for ProofRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProofRejected {}

pub(super) fn run<T: Send>(
    count: usize,
    jobs: usize,
    budget: u64,
    weight: impl Fn(usize) -> u64 + Sync,
    fetch: impl Fn(usize) -> Result<T> + Sync,
    mut consume: impl FnMut(usize, T) -> Result<()>,
) -> Result<()> {
    let quick = super::gateway::is_quick();
    let next = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let used = Mutex::new(0u64);
    let available = Condvar::new();
    let (tx, rx) = mpsc::sync_channel(0);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..count.min(jobs) {
            let (tx, next, cancelled, used, available, weight, fetch) =
                (tx.clone(), &next, &cancelled, &used, &available, &weight, &fetch);
            handles.push(scope.spawn(move || {
                while !cancelled.load(Ordering::SeqCst) {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= count {
                        break;
                    }
                    let charge = weight(i).max(1).min(budget);
                    let mut active = used.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    while *active + charge > budget && !cancelled.load(Ordering::SeqCst) {
                        active = available.wait(active).unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if cancelled.load(Ordering::SeqCst) {
                        break;
                    }
                    *active += charge;
                    drop(active);
                    // Catch panics as well: cancellation must wake and join every waiting worker.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if quick {
                            super::gateway::quick(|| fetch(i))
                        } else {
                            fetch(i)
                        }
                    }))
                    .unwrap_or_else(|_| Err(anyhow!("download worker panicked")));
                    let (ack, received) = mpsc::sync_channel::<()>(0);
                    if tx.send((i, result, ack)).is_ok() {
                        let _ = received.recv();
                    }
                    *used.lock().unwrap_or_else(std::sync::PoisonError::into_inner) -= charge;
                    available.notify_all();
                }
            }));
        }
        drop(tx);
        let mut result = Ok(());
        for (i, item, ack) in rx {
            if result.is_ok() {
                result = item.and_then(|value| consume(i, value));
                if result.is_err() {
                    cancelled.store(true, Ordering::SeqCst);
                    available.notify_all();
                }
            }
            // On error, drain already running work before returning to the caller.
            drop(ack);
        }
        for handle in handles {
            if handle.join().is_err() && result.is_ok() {
                result = Err(anyhow!("download worker panicked"));
            }
        }
        result
    })
}

/// Application HTTP bodies consumed/supplied by the gateway transport, excluding headers and TLS framing.
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
    use std::sync::{Arc, Barrier};

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
        assert_eq!(result.unwrap_err().to_string(), "download worker panicked");
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
