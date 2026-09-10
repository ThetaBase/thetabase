//! Local read cache.
//!
//! A bounded, TTL'd map from key to last-seen value. It exists to keep `get`
//! inside the 5ms p50 budget (`09-sla-performance.md` §2) by not crossing the
//! network for a repeated read.
//!
//! Time is injected rather than read from the clock, so eviction is
//! deterministically testable.

use std::collections::HashMap;

use theta_core::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub invalidations: u64,
    pub evictions: u64,
}

#[derive(Debug, Clone)]
struct Entry {
    value: Option<Value>,
    stored_at_ms: i64,
    /// Insertion order, used to evict the oldest entry when full.
    sequence: u64,
}

#[derive(Debug)]
pub struct ReadCache {
    entries: HashMap<String, Entry>,
    capacity: usize,
    /// How long an entry may be served. Zero disables the cache entirely.
    ttl_ms: i64,
    sequence: u64,
    stats: CacheStats,
}

impl ReadCache {
    pub fn new(capacity: usize, ttl_ms: i64) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            ttl_ms,
            sequence: 0,
            stats: CacheStats::default(),
        }
    }

    /// A cache that never serves anything. What a caller gets when they set a
    /// zero TTL, and the default for anything that cannot tolerate staleness.
    pub fn disabled() -> Self {
        Self::new(0, 0)
    }

    pub fn is_enabled(&self) -> bool {
        self.ttl_ms > 0 && self.capacity > 0
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up `key`. A cached miss (the key is known not to exist) is a real
    /// answer and is cached as `Some(None)`.
    pub fn get(&mut self, key: &str, now_ms: i64) -> Option<Option<Value>> {
        if !self.is_enabled() {
            self.stats.misses += 1;
            return None;
        }

        match self.entries.get(key) {
            Some(entry) if now_ms.saturating_sub(entry.stored_at_ms) < self.ttl_ms => {
                self.stats.hits += 1;
                Some(entry.value.clone())
            }
            Some(_) => {
                // Expired. Drop it rather than leaving it to be re-checked on
                // every subsequent lookup.
                self.entries.remove(key);
                self.stats.misses += 1;
                None
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    pub fn put(&mut self, key: &str, value: Option<Value>, now_ms: i64) {
        if !self.is_enabled() {
            return;
        }
        if self.entries.len() >= self.capacity && !self.entries.contains_key(key) {
            self.evict_oldest();
        }
        self.sequence += 1;
        self.entries.insert(
            key.to_string(),
            Entry {
                value,
                stored_at_ms: now_ms,
                sequence: self.sequence,
            },
        );
    }

    /// Drop a key. Called before a write is acknowledged, so a read that follows
    /// the caller's own write can never be served the pre-write value.
    pub fn invalidate(&mut self, key: &str) {
        if self.entries.remove(key).is_some() {
            self.stats.invalidations += 1;
        }
    }

    /// Drop everything. Used after a merge or a branch switch, where reasoning
    /// about which individual keys moved is not worth the risk of getting it
    /// wrong.
    pub fn clear(&mut self) {
        self.stats.invalidations += self.entries.len() as u64;
        self.entries.clear();
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.sequence)
            .map(|(k, _)| k.clone());
        if let Some(key) = oldest {
            self.entries.remove(&key);
            self.stats.evictions += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(n: i64) -> Option<Value> {
        Some(Value::Int(n))
    }

    #[test]
    fn a_fresh_entry_is_served_and_a_stale_one_is_not() {
        let mut cache = ReadCache::new(10, 1_000);
        cache.put("k", value(1), 0);
        assert_eq!(cache.get("k", 999), Some(value(1)));
        assert_eq!(
            cache.get("k", 1_000),
            None,
            "an entry at exactly the TTL is stale"
        );
    }

    #[test]
    fn a_write_invalidates_before_it_can_be_read_back_stale() {
        let mut cache = ReadCache::new(10, 60_000);
        cache.put("k", value(1), 0);
        cache.invalidate("k");
        // Read-your-writes: the pre-write value must be gone, not merely old.
        assert_eq!(cache.get("k", 1), None);
        assert_eq!(cache.stats().invalidations, 1);
    }

    #[test]
    fn a_known_absence_is_cached_as_an_answer() {
        let mut cache = ReadCache::new(10, 1_000);
        cache.put("missing", None, 0);
        assert_eq!(
            cache.get("missing", 1),
            Some(None),
            "a cached miss is a hit, not a miss"
        );
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn the_cache_stays_within_its_capacity() {
        let mut cache = ReadCache::new(3, 60_000);
        for i in 0..10 {
            cache.put(&format!("k{i}"), value(i), i);
        }
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.stats().evictions, 7);
        // The most recent survive; the oldest were evicted.
        assert_eq!(cache.get("k9", 10), Some(value(9)));
        assert_eq!(cache.get("k0", 10), None);
    }

    #[test]
    fn a_zero_ttl_disables_the_cache_entirely() {
        let mut cache = ReadCache::new(100, 0);
        assert!(!cache.is_enabled());
        cache.put("k", value(1), 0);
        assert_eq!(cache.get("k", 0), None, "a disabled cache must never serve");
        assert!(cache.is_empty());
    }

    #[test]
    fn clearing_drops_everything() {
        let mut cache = ReadCache::new(10, 60_000);
        cache.put("a", value(1), 0);
        cache.put("b", value(2), 0);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.get("a", 1), None);
    }
}
