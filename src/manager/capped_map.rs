//! A session-keyed side map with a size cap and least-recently-touched eviction.
//!
//! The manager keeps several maps keyed per session or per connection (pins,
//! connection pins, the divert ledger, session stats, the extended-cache set).
//! Each is bounded the same way: once an insert takes it past its cap, the
//! entry touched longest ago is removed, unless that entry is the one just
//! touched. [`CappedMap`] holds that rule once; each map keeps its own cap and
//! its own idea of "touched".

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

/// A `HashMap` that never grows more than one past `cap` for long: every
/// [`Self::insert`] and every [`Self::evict_sparing`] removes the
/// least-recently-touched entry once the map is over the cap.
///
/// Reads and in-place edits go through `Deref`/`DerefMut` to the inner map.
/// An insert through the `entry` API does not evict on its own; the caller
/// follows it with [`Self::evict_sparing`].
#[derive(Debug)]
pub(super) struct CappedMap<K, V> {
    map: HashMap<K, V>,
    cap: usize,
    touched: fn(&V) -> i64,
}

impl<K: Hash + Eq + Copy, V> CappedMap<K, V> {
    /// An empty map bounded at `cap`, ordered for eviction by `touched`.
    pub(super) fn new(cap: usize, touched: fn(&V) -> i64) -> Self {
        Self {
            map: HashMap::new(),
            cap,
            touched,
        }
    }

    /// Insert `value` at `key`, then evict as [`Self::evict_sparing`] does
    /// for `key`. Returns the value `key` held before, as `HashMap::insert`.
    pub(super) fn insert(&mut self, key: K, value: V) -> Option<V> {
        let prior = self.map.insert(key, value);
        self.evict_sparing(key);
        prior
    }

    /// When the map is over its cap, remove the entry with the oldest
    /// `touched`, unless that entry is `key` (the one the caller just
    /// touched), in which case nothing is removed this time.
    ///
    /// One entry per call: the scan is O(n), and a map only ever grows by one
    /// entry per insert. Returns the key it removed.
    pub(super) fn evict_sparing(&mut self, key: K) -> Option<K> {
        if self.map.len() <= self.cap {
            return None;
        }
        let touched = self.touched;
        let oldest = self
            .map
            .iter()
            .min_by_key(|(_, value)| touched(value))
            .map(|(&oldest, _)| oldest)
            .filter(|&oldest| oldest != key)?;
        self.map.remove(&oldest);
        Some(oldest)
    }
}

impl<K, V> Deref for CappedMap<K, V> {
    type Target = HashMap<K, V>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl<K, V> DerefMut for CappedMap<K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}

/// A set with the same cap and eviction as [`CappedMap`], ordered by when
/// each key was last inserted.
///
/// Reads go through `Deref` to a `HashSet`, so a reader sees an ordinary set
/// (`contains`, `iter`, `clone`). Every write goes through the inherent
/// methods below, which keep the set and its insert order in step.
#[derive(Debug)]
pub(super) struct CappedSet<K> {
    set: HashSet<K>,
    order: CappedMap<K, i64>,
    next: i64,
}

impl<K: Hash + Eq + Copy> CappedSet<K> {
    /// An empty set bounded at `cap`.
    pub(super) fn new(cap: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: CappedMap::new(cap, |&inserted| inserted),
            next: 0,
        }
    }

    /// Insert `key`, or mark it the most recently inserted if already in, and
    /// evict the least recently inserted other key when over the cap. Returns
    /// whether `key` is new, as `HashSet::insert`.
    pub(super) fn insert(&mut self, key: K) -> bool {
        self.next += 1;
        self.order.map.insert(key, self.next);
        if let Some(evicted) = self.order.evict_sparing(key) {
            self.set.remove(&evicted);
        }
        self.set.insert(key)
    }

    /// Remove `key`; whether it was in, as `HashSet::remove`.
    pub(super) fn remove(&mut self, key: &K) -> bool {
        self.order.remove(key);
        self.set.remove(key)
    }

    /// Keep only the keys `keep` answers `true` for.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.set.retain(|key| keep(key));
        let set = &self.set;
        self.order.retain(|key, _| set.contains(key));
    }

    /// Remove every key.
    pub(super) fn clear(&mut self) {
        self.set.clear();
        self.order.clear();
    }
}

impl<K> Deref for CappedSet<K> {
    type Target = HashSet<K>;

    fn deref(&self) -> &Self::Target {
        &self.set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(value: &(usize, i64)) -> i64 {
        value.1
    }

    #[test]
    fn an_insert_over_the_cap_evicts_the_oldest_other_entry() {
        let mut map = CappedMap::new(2, touch);
        map.insert(1u64, (0, 10));
        map.insert(2, (0, 20));
        map.insert(3, (0, 30));
        assert_eq!(map.len(), 2);
        assert!(!map.contains_key(&1), "the oldest touch goes");
        assert!(map.contains_key(&2) && map.contains_key(&3));
    }

    #[test]
    fn the_key_just_touched_is_never_its_own_victim() {
        let mut map = CappedMap::new(2, touch);
        map.insert(1u64, (0, 10));
        map.insert(2, (0, 20));
        // Inserted with the oldest stamp of all: the map stays one over.
        map.insert(3, (0, 5));
        assert_eq!(map.len(), 3);
        assert!(map.contains_key(&3));
    }

    #[test]
    fn evict_sparing_bounds_an_entry_api_insert() {
        let mut map = CappedMap::new(1, touch);
        map.insert(1u64, (0, 10));
        map.entry(2).or_insert((0, 20));
        assert_eq!(map.len(), 2, "the entry API alone does not evict");
        map.evict_sparing(2);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn a_capped_set_holds_its_cap_and_keeps_the_latest_inserts() {
        let mut set = CappedSet::new(3);
        for key in 0u64..10 {
            set.insert(key);
        }
        assert_eq!(set.len(), 3);
        let mut kept: Vec<u64> = set.iter().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, vec![7, 8, 9]);
        // Re-inserting an old key makes it the newest: 7 now outlives 8.
        set.insert(7);
        set.insert(10);
        assert!(set.contains(&7) && !set.contains(&8));
    }

    #[test]
    fn a_capped_set_remove_retain_and_clear_keep_the_order_in_step() {
        let mut set = CappedSet::new(2);
        set.insert(1u64);
        set.insert(2);
        assert!(set.remove(&1));
        set.insert(3);
        // 1 left the order with the set, so 2 and 3 fit with no eviction.
        assert!(set.contains(&2) && set.contains(&3));
        set.retain(|&key| key == 3);
        set.insert(4);
        assert!(set.contains(&3) && set.contains(&4));
        set.clear();
        assert!(set.is_empty());
        set.insert(5);
        assert_eq!(set.len(), 1);
    }
}
