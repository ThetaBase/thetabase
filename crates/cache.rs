//! The read cache, with the one rule that cannot be got wrong.
//!
//! `03-data-model-consistency.md` §3.1: a write through this Scribe invalidates
//! the key locally *before* the write is acknowledged, so a read that follows a
//! write cannot serve the value the write replaced. Another writer's update may
//! be served stale for up to the TTL — that is a real weakening, stated rather
//! than implied away, and a zero TTL disables the cache entirely.
//!
//! It lives in the WASM core rather than in each host because it is a
//! correctness rule, not a convenience. Three implementations would be three
//! chances to acknowledge a write before dropping the key.

use std::collections::HashMap;

#[derive(Debug, Clone)]
struct Entry {
    value: String,
    stored_at_ms: i64,
}

/// A bounded, TTL'd read cache.
#[derive(Debug)]
pub struct ReadCache {
    entries: HashMap<String, Entry>,
    capacity: usize,
    ttl_ms: i64,
    hits: u64,
    misses: u64,
}

impl ReadCache {
    pub fn new(capacity: usize, ttl_ms: i64) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            ttl_ms,
            hits: 0,
            misses: 0,
        }
    }

    /// Whether caching is on at all. A zero TTL means no.
    pub fn enabled(&self) -> bool {
        self.ttl_ms > 0 && self.capacity > 0
    }

    pub fn get(&mut self, key: &str, now_ms: i64) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        match self.entries.get(key) {
            Some(entry) if now_ms - entry.stored_at_ms < self.ttl_ms => {
                self.hits += 1;
                Some(entry.value.clone())
            }
            Some(_) => {
                // Expired. Dropped now rather than left to be stepped over
                // again on the next read.
                self.entries.remove(key);
                self.misses += 1;
                None
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    pub fn put(&mut self, key: &str, value: String, now_ms: i64) {
        if !self.enabled() {
            return;
        }
        if self.entries.len() >= self.capacity && !self.entries.contains_key(key) {
            // Evict the oldest. Not an LRU: an LRU needs a recency list, which
            // is more machinery than a bounded edge cache earns, and the TTL
            // already bounds how wrong any entry can be.
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.stored_at_ms)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key.to_string(),
            Entry {
                value,
                stored_at_ms: now_ms,
            },
        );
    }

    /// Drop one key. Called before a write is acknowledged, never after.
    pub fn invalidate(&mut self, key: &str) {
        self.entries.remove(key);
    }

    /// Drop everything, for a change that can move rows this cache cannot name.
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_drops_the_key_so_a_later_read_cannot_serve_the_old_value() {
        // Read-your-writes. The one rule that cannot be got wrong.
        let mut cache = ReadCache::new(16, 60_000);
        cache.put("user:1", "\"before\"".into(), 0);
        cache.invalidate("user:1");

        assert_eq!(cache.get("user:1", 1), None);
    }

    #[test]
    fn a_zero_ttl_disables_the_cache_entirely() {
        // Opt-in, and off means off — not "off but still holds one entry".
        let mut cache = ReadCache::new(16, 0);
        cache.put("k", "\"v\"".into(), 0);

        assert!(!cache.enabled());
        assert_eq!(cache.get("k", 0), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn an_entry_past_its_ttl_is_a_miss() {
        let mut cache = ReadCache::new(16, 1_000);
        cache.put("k", "\"v\"".into(), 0);

        assert_eq!(cache.get("k", 999), Some("\"v\"".into()));
        assert_eq!(cache.get("k", 1_000), None, "the TTL boundary is exclusive");
    }

    #[test]
    fn the_cache_stays_within_its_capacity() {
        let mut cache = ReadCache::new(2, 60_000);
        cache.put("a", "1".into(), 0);
        cache.put("b", "2".into(), 1);
        cache.put("c", "3".into(), 2);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a", 3), None, "the oldest entry should have gone");
        assert_eq!(cache.get("c", 3), Some("3".into()));
    }

    #[test]
    fn a_merge_drops_everything_because_it_can_move_rows_nobody_can_name() {
        let mut cache = ReadCache::new(16, 60_000);
        cache.put("a", "1".into(), 0);
        cache.put("b", "2".into(), 0);
        cache.invalidate_all();

        assert!(cache.is_empty());
    }
}
