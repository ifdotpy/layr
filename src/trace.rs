//! `LAYR_TRACE=1` prints how long the expensive steps take (to standard error). Without it the
//! steps of a store-lock hold are still kept, so that `meta/lock-slow.log` names the slow ones.

use std::cell::RefCell;
use std::time::Instant;

/// Steps shorter than this are not kept for the lock log (milliseconds).
const KEEP_MS: f64 = 100.0;

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

impl Drop for Span {
    fn drop(&mut self) {
        let ms = self.start.elapsed().as_secs_f64() * 1000.0;
        if std::env::var_os("LAYR_TRACE").is_some() {
            eprintln!("trace {ms:>8.1} ms  {}", self.name);
        }
        if ms >= KEEP_MS {
            STEPS.with(|s| s.borrow_mut().push((std::mem::take(&mut self.name), ms)));
        }
    }
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
