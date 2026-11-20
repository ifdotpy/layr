//! Deterministic consumption of candidates grouped by an equality key.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// Candidates retain insertion order within each key. Each candidate is verified at most once.
pub struct Candidates<K, V> {
    by_key: HashMap<K, VecDeque<V>>,
}

impl<K, V> Default for Candidates<K, V> {
    fn default() -> Self {
        Self { by_key: HashMap::new() }
    }
}

impl<K: Eq + Hash, V> Candidates<K, V> {
    /// Append a candidate in expected O(1) time, excluding key hashing.
    pub fn insert(&mut self, key: K, value: V) {
        self.by_key.entry(key).or_default().push_back(value);
    }

    /// Consume candidates until `equal` accepts one, discarding every rejected candidate.
    /// Each candidate is visited at most once across all lookups. This is best-effort matching:
    /// a rejection, including a key collision or failed read, cannot be retried later.
    /// Each consumed candidate costs O(1), excluding key hashing and verification.
    pub fn take_matching(&mut self, key: &K, mut equal: impl FnMut(&V) -> bool) -> Option<V> {
        let values = self.by_key.get_mut(key)?;
        while let Some(value) = values.pop_front() {
            if equal(&value) {
                return Some(value);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_visit_each_candidate_once_in_order() {
        let mut candidates = Candidates::default();
        let count = 50_000;
        for i in 0..count {
            candidates.insert((), i);
        }
        let mut visits = 0;
        for i in 0..count {
            assert_eq!(
                candidates.take_matching(&(), |_| {
                    visits += 1;
                    true
                }),
                Some(i)
            );
        }
        assert_eq!(visits, count);
        assert_eq!(candidates.take_matching(&(), |_| true), None);
    }

    #[test]
    fn unreadable_candidates_are_verified_only_once() {
        let mut candidates = Candidates::default();
        let count = 64;
        for i in 0..count {
            candidates.insert((), i);
        }
        let mut visits = 0;
        for _ in 0..count {
            assert_eq!(
                candidates.take_matching(&(), |_| {
                    visits += 1;
                    false
                }),
                None
            );
        }
        assert_eq!(visits, count);
    }

    #[test]
    fn rejections_are_discarded_without_reordering_remaining_candidates() {
        let mut candidates = Candidates::default();
        for value in ["first", "second", "first", "third"] {
            candidates.insert(0, value);
        }
        candidates.insert(1, "other");
        assert_eq!(candidates.take_matching(&0, |v| *v == "second"), Some("second"));
        assert_eq!(candidates.take_matching(&0, |v| *v == "first"), Some("first"));
        assert_eq!(candidates.take_matching(&0, |_| true), Some("third"));
        assert_eq!(candidates.take_matching(&0, |_| true), None);
        assert_eq!(candidates.take_matching(&1, |_| true), Some("other"));
        assert_eq!(candidates.take_matching(&2, |_| true), None);
    }
}
