//! In-memory LRU cache for artwork bytes, keyed by content hash.
//!
//! Avoids repeated disk reads for frequently requested album covers,
//! which is critical at 500K+ tracks where the same artwork is served
//! hundreds of times per UI session.

use std::collections::HashMap;
use std::time::Instant;

/// Default maximum number of cached artwork entries.
const DEFAULT_MAX_SIZE: usize = 256;

pub struct ArtworkCache {
    entries: HashMap<String, CacheEntry>,
    max_size: usize,
}

struct CacheEntry {
    data: Vec<u8>,
    last_access: Instant,
}

impl ArtworkCache {
    /// Create a new cache with the default capacity (256 entries).
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            max_size: DEFAULT_MAX_SIZE,
        }
    }

    /// Create a new cache with a custom capacity.
    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_size: max_size.max(1),
        }
    }

    /// Retrieve cached artwork bytes, updating last-access time.
    /// Returns `None` if the hash is not in the cache.
    pub fn get(&mut self, hash: &str) -> Option<Vec<u8>> {
        if let Some(entry) = self.entries.get_mut(hash) {
            entry.last_access = Instant::now();
            Some(entry.data.clone())
        } else {
            None
        }
    }

    /// Insert artwork bytes into the cache. Evicts the least recently
    /// accessed entry if the cache is at capacity.
    pub fn put(&mut self, hash: &str, data: Vec<u8>) {
        if self.entries.len() >= self.max_size && !self.entries.contains_key(hash) {
            self.evict_oldest();
        }
        self.entries.insert(
            hash.to_string(),
            CacheEntry {
                data,
                last_access: Instant::now(),
            },
        );
    }

    /// Remove the least recently accessed entry.
    fn evict_oldest(&mut self) {
        if let Some(oldest_key) = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.last_access)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
        }
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for ArtworkCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_put_get() {
        let mut cache = ArtworkCache::new();
        cache.put("abc123", vec![1, 2, 3]);
        assert_eq!(cache.get("abc123"), Some(vec![1, 2, 3]));
        assert_eq!(cache.get("missing"), None);
    }

    #[test]
    fn evicts_oldest_when_full() {
        let mut cache = ArtworkCache::with_capacity(2);
        cache.put("a", vec![1]);
        // Small delay so timestamps differ
        cache.put("b", vec![2]);

        // Access "a" to make it more recent
        cache.get("a");

        // Insert "c" — should evict "b" (the oldest)
        cache.put("c", vec![3]);
        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_none());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn overwrite_existing_key() {
        let mut cache = ArtworkCache::with_capacity(2);
        cache.put("a", vec![1]);
        cache.put("b", vec![2]);
        // Overwrite "a" should NOT trigger eviction
        cache.put("a", vec![10]);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a"), Some(vec![10]));
        assert_eq!(cache.get("b"), Some(vec![2]));
    }
}
