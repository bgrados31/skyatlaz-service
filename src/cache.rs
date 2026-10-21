//! A tiny in-memory cache with a per-entry time to live.
//!
//! The service caches the slimmed down Hypixel profiles so that the mod can poll
//! as often as it wants without burning the API rate limit.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A cache entry together with the instant at which it stops being valid.
struct Entry<V> {
    value: V,
    expires_at: Instant,
}

/// A map of values that expire after a fixed amount of time.
pub struct TtlCache<K, V> {
    entries: Mutex<HashMap<K, Entry<V>>>,
    ttl: Duration,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Creates a cache whose entries live for `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Returns the cached value for `key`, or `None` when it is missing or expired.
    pub fn get(&self, key: &K) -> Option<V> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match entries.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Stores a value under `key`, replacing any previous entry.
    pub fn insert(&self, key: K, value: V) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() > 64 {
            let now = Instant::now();
            entries.retain(|_, entry| entry.expires_at > now);
        }
        entries.insert(
            key,
            Entry {
                value,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }

    /// Returns the number of entries that are still valid.
    pub fn len(&self) -> usize {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        entries.values().filter(|e| e.expires_at > now).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_the_stored_value() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(60));
        cache.insert("a".to_string(), 7);
        assert_eq!(cache.get(&"a".to_string()), Some(7));
        assert_eq!(cache.get(&"b".to_string()), None);
    }

    #[test]
    fn expired_entries_are_dropped() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_millis(1));
        cache.insert("a".to_string(), 7);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.get(&"a".to_string()), None);
        assert_eq!(cache.len(), 0);
    }
}
