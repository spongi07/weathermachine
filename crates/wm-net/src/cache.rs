//! Local caches: the latest response per URL (for conditional requests and
//! internal reuse) and a generic TTL cache for slow-changing metadata.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::Duration;

/// Latest successful response for a URL.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedResponse {
    pub body: Bytes,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

/// Thread-safe map of URL → latest response.
#[derive(Debug, Default)]
pub struct ResponseCache {
    inner: Mutex<HashMap<String, CachedResponse>>,
}

impl ResponseCache {
    pub fn get(&self, url: &str) -> Option<CachedResponse> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(url)
            .cloned()
    }

    pub fn put(&self, url: &str, value: CachedResponse) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(url.to_owned(), value);
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Generic TTL cache keyed by `K`. Time is supplied by the caller (monotonic),
/// keeping the cache deterministic in tests.
#[derive(Debug)]
pub struct TtlCache<K, V> {
    ttl: Duration,
    inner: Mutex<HashMap<K, (V, Duration)>>,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Value if present and younger than the TTL at monotonic time `now`.
    pub fn get(&self, key: &K, now: Duration) -> Option<V> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(key)
            .and_then(|(v, at)| (now.saturating_sub(*at) < self.ttl).then(|| v.clone()))
    }

    pub fn insert(&self, key: K, value: V, now: Duration) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, (value, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_cache_expires() {
        let c: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(10));
        c.insert("a", 1, Duration::from_secs(0));
        assert_eq!(c.get(&"a", Duration::from_secs(9)), Some(1));
        assert_eq!(c.get(&"a", Duration::from_secs(10)), None);
    }

    #[test]
    fn response_cache_roundtrip() {
        let c = ResponseCache::default();
        assert!(c.is_empty());
        c.put(
            "u",
            CachedResponse {
                body: Bytes::from_static(b"x"),
                etag: Some("\"e\"".into()),
                last_modified: None,
                content_type: None,
                fetched_at: Utc::now(),
            },
        );
        assert_eq!(c.get("u").unwrap().etag.as_deref(), Some("\"e\""));
        assert_eq!(c.len(), 1);
    }
}
