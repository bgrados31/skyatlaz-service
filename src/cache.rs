//! A small in-memory cache with a per-entry time to live and a hard size limit.
//!
//! The service caches the slimmed down Hypixel profiles so that the mod can poll
//! as often as it wants without burning the API rate limit. Both bounds matter:
//! the ttl protects the upstream rate limit, the capacity protects this process.
//! The cache is shared by every caller, so it is guarded by a plain mutex and no
//! lock is ever held across an await point.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A cache entry together with the instant it stops being valid and the logical
/// time it was last read or written.
struct Entry<V> {
    value: V,
    expires_at: Instant,
    last_used: u64,
}

/// The state behind the mutex.
struct State<K, V> {
    entries: HashMap<K, Entry<V>>,
    /// A monotonic counter instead of `Instant`, so recency is well defined even
    /// when the clock is too coarse to tell two back to back operations apart.
    clock: u64,
}

impl<K, V> State<K, V> {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

/// A map of values that expire after a fixed amount of time and never grows
/// past a fixed number of entries.
pub struct TtlCache<K, V> {
    state: Mutex<State<K, V>>,
    ttl: Duration,
    capacity: usize,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Creates a cache whose entries live for `ttl` and which holds at most
    /// `capacity` of them. The capacity is never zero: a cache that cannot hold
    /// anything would only cost work.
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                entries: HashMap::new(),
                clock: 0,
            }),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// Returns the cached value for `key`, or `None` when it is missing or expired.
    ///
    /// A hit counts as a use, so the entry is the last one to be evicted.
    pub fn get(&self, key: &K) -> Option<V> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let tick = state.tick();

        let (expired, value) = match state.entries.get(key) {
            Some(entry) => (entry.expires_at <= now, Some(entry.value.clone())),
            None => (false, None),
        };

        if expired {
            state.entries.remove(key);
            return None;
        }

        if value.is_some() {
            if let Some(entry) = state.entries.get_mut(key) {
                entry.last_used = tick;
            }
        }

        value
    }

    /// Stores a value under `key`, replacing any previous entry.
    ///
    /// When the cache is full the least recently used entry makes room. Expired
    /// entries are dropped on the way, before anything live is evicted.
    pub fn insert(&self, key: K, value: V) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();

        if !state.entries.contains_key(&key) && state.entries.len() >= self.capacity {
            state.entries.retain(|_, entry| entry.expires_at > now);
            while state.entries.len() >= self.capacity {
                let victim = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(key, _)| key.clone());
                match victim {
                    Some(victim) => {
                        state.entries.remove(&victim);
                    }
                    None => break,
                }
            }
        }

        let tick = state.tick();
        state.entries.insert(
            key,
            Entry {
                value,
                expires_at: now + self.ttl,
                last_used: tick,
            },
        );
    }

    /// Returns the number of entries that are still valid.
    pub fn len(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        state
            .entries
            .values()
            .filter(|entry| entry.expires_at > now)
            .count()
    }

    /// Returns whether the cache holds no valid entry.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns how many entries the cache may hold at once.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> String {
        value.to_string()
    }

    #[test]
    fn returns_the_stored_value() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(60), 8);
        cache.insert(key("a"), 7);
        assert_eq!(cache.get(&key("a")), Some(7));
        assert_eq!(cache.get(&key("b")), None);
    }

    #[test]
    fn expired_entries_are_dropped() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_millis(1), 8);
        cache.insert(key("a"), 7);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.get(&key("a")), None);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn the_cache_never_grows_past_its_capacity() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(60), 10);
        for index in 0..1_000 {
            cache.insert(key(&index.to_string()), index);
        }
        assert_eq!(cache.len(), 10);
        assert_eq!(cache.capacity(), 10);
    }

    #[test]
    fn evicts_the_least_recently_used_entry() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(60), 2);
        cache.insert(key("a"), 1);
        cache.insert(key("b"), 2);
        // A read makes "a" newer than "b", so "b" is the one that has to go.
        assert_eq!(cache.get(&key("a")), Some(1));
        cache.insert(key("c"), 3);

        assert_eq!(cache.get(&key("a")), Some(1));
        assert_eq!(cache.get(&key("b")), None);
        assert_eq!(cache.get(&key("c")), Some(3));
    }

    #[test]
    fn refreshing_an_entry_does_not_grow_the_cache() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(60), 2);
        for value in 0..100 {
            cache.insert(key("a"), value);
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&key("a")), Some(99));
    }

    #[test]
    fn an_expired_entry_frees_its_slot_before_a_live_one_is_evicted() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_millis(20), 1);
        cache.insert(key("old"), 1);
        std::thread::sleep(Duration::from_millis(30));
        cache.insert(key("new"), 2);

        assert_eq!(cache.get(&key("new")), Some(2));
        assert!(!cache.is_empty());
    }
}
