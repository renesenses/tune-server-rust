use std::collections::HashMap;
use std::time::Instant;

pub struct CachedUrl {
    pub url: String,
    expires_at: Instant,
}

pub struct StreamUrlCache {
    ttl_secs: u64,
    max_size: usize,
    cache: HashMap<String, CachedUrl>,
    insertion_order: Vec<String>,
}

impl StreamUrlCache {
    pub fn new(ttl_seconds: u64, max_size: usize) -> Self {
        Self {
            ttl_secs: ttl_seconds,
            max_size,
            cache: HashMap::with_capacity(max_size),
            insertion_order: Vec::with_capacity(max_size),
        }
    }

    pub fn get(&mut self, track_id: &str) -> Option<String> {
        let expired = self
            .cache
            .get(track_id)
            .map(|e| e.expires_at <= Instant::now())
            .unwrap_or(true);

        if expired {
            self.cache.remove(track_id);
            self.insertion_order.retain(|k| k != track_id);
            None
        } else {
            self.cache.get(track_id).map(|e| e.url.clone())
        }
    }

    pub fn set(&mut self, track_id: &str, url: &str, ttl: Option<u64>) {
        let exists = self.cache.contains_key(track_id);
        if !exists && self.cache.len() >= self.max_size {
            if let Some(oldest) = self.insertion_order.first().cloned() {
                self.cache.remove(&oldest);
                self.insertion_order.remove(0);
            }
        }
        let ttl = ttl.unwrap_or(self.ttl_secs);
        self.cache.insert(
            track_id.to_string(),
            CachedUrl {
                url: url.to_string(),
                expires_at: Instant::now() + std::time::Duration::from_secs(ttl),
            },
        );
        if !exists {
            self.insertion_order.push(track_id.to_string());
        }
    }

    pub fn invalidate(&mut self, track_id: &str) {
        self.cache.remove(track_id);
        self.insertion_order.retain(|k| k != track_id);
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.insertion_order.clear();
    }

    pub fn cleanup(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .cache
            .iter()
            .filter(|(_, v)| v.expires_at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &expired {
            self.cache.remove(k);
        }
        self.insertion_order.retain(|k| !expired.contains(k));
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl Default for StreamUrlCache {
    fn default() -> Self {
        Self::new(300, 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get() {
        let mut cache = StreamUrlCache::new(60, 100);
        cache.set("t1", "http://example.com/stream", None);
        assert_eq!(cache.get("t1").as_deref(), Some("http://example.com/stream"));
    }

    #[test]
    fn miss_returns_none() {
        let mut cache = StreamUrlCache::new(60, 100);
        assert!(cache.get("unknown").is_none());
    }

    #[test]
    fn invalidate_removes() {
        let mut cache = StreamUrlCache::new(60, 100);
        cache.set("t1", "http://a.com", None);
        cache.invalidate("t1");
        assert_eq!(cache.get("t1"), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn clear_removes_all() {
        let mut cache = StreamUrlCache::new(60, 100);
        cache.set("t1", "http://a.com", None);
        cache.set("t2", "http://b.com", None);
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn evicts_oldest_on_overflow() {
        let mut cache = StreamUrlCache::new(60, 2);
        cache.set("t1", "http://a.com", None);
        cache.set("t2", "http://b.com", None);
        cache.set("t3", "http://c.com", None);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("t1"), None);
        assert!(cache.get("t2").is_some());
        assert!(cache.get("t3").is_some());
    }

    #[test]
    fn expired_entry_returns_none() {
        let mut cache = StreamUrlCache::new(0, 100); // 0 TTL = instant expiry
        cache.set("t1", "http://a.com", Some(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(cache.get("t1"), None);
    }

    #[test]
    fn update_existing_keeps_size() {
        let mut cache = StreamUrlCache::new(60, 100);
        cache.set("t1", "http://a.com", None);
        cache.set("t1", "http://b.com", None);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("t1").as_deref(), Some("http://b.com"));
    }
}
