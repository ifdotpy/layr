//! `LAYR_TRACE=1` prints how long the expensive steps take (to standard error). Without it the
//! steps of a store-lock hold are still kept, so that `meta/lock-slow.log` names the slow ones.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

/// Steps shorter than this are not kept for the lock log (milliseconds).
const KEEP_MS: f64 = 100.0;

// The daemon parent inherits its own environment. Its forked request child overrides
// tracing for all command threads without changing the process environment.
const INHERIT_ENV: u8 = 0;
const DISABLED: u8 = 1;
const ENABLED: u8 = 2;
static REQUEST_TRACE: AtomicU8 = AtomicU8::new(INHERIT_ENV);

pub fn set_request_enabled(enabled: bool) {
    REQUEST_TRACE.store(if enabled { ENABLED } else { DISABLED }, Ordering::Relaxed);
}

fn enabled() -> bool {
    match REQUEST_TRACE.load(Ordering::Relaxed) {
        INHERIT_ENV => std::env::var_os("LAYR_TRACE").is_some(),
        ENABLED => true,
        _ => false,
    }
}

thread_local! {
    /// Steps of this thread since the store lock was taken: name and milliseconds.
    static STEPS: RefCell<Vec<(String, f64)>> = const { RefCell::new(Vec::new()) };
}

pub struct Span {
    name: String,
    start: Instant,
}

pub fn span(name: impl Into<String>) -> Option<Span> {
    Some(Span { name: name.into(), start: Instant::now() })
}

/// A span whose name is built only when `LAYR_TRACE` is set (a name that needs a `format!` on a hot path).
pub fn named(name: impl FnOnce() -> String) -> Option<Span> {
    enabled().then(|| Span { name: name(), start: Instant::now() })
}

impl Drop for Span {
    fn drop(&mut self) {
        let ms = self.start.elapsed().as_secs_f64() * 1000.0;
        if enabled() {
            eprintln!("trace {ms:>8.1} ms  {}", self.name);
        }
        if ms >= KEEP_MS {
            STEPS.with(|s| s.borrow_mut().push((std::mem::take(&mut self.name), ms)));
        }
    }
}

/// Like `span`, and with `LAYR_TRACE` also prints the start: a step that never ends shows its
/// start line, which `span` alone cannot show.
pub fn begin(name: impl Into<String>) -> Option<Span> {
    let name = name.into();
    if enabled() {
        eprintln!("trace begin {name}");
    }
    span(name)
}

/// Forget the kept steps (the store lock was just taken).
pub fn reset() {
    STEPS.with(|s| s.borrow_mut().clear());
}

/// The slowest kept steps, longest first, as `name 1234ms; name 567ms`.
pub fn slowest(n: usize) -> String {
    STEPS.with(|s| {
        let mut v = s.borrow().clone();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.iter().take(n).map(|(name, ms)| format!("{name} {ms:.0}ms")).collect::<Vec<_>>().join("; ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slowest_steps_are_kept_longest_first_and_reset() {
        reset();
        STEPS.with(|s| s.borrow_mut().extend([("a".to_string(), 150.0), ("b".to_string(), 900.0), ("c".to_string(), 300.0)]));
        drop(span("short"));
        assert_eq!(slowest(2), "b 900ms; c 300ms");
        reset();
        assert_eq!(slowest(3), "");
    }
}
