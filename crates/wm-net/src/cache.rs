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

/// Responses a [`ResponseCache`] keeps by default. The URLs asked
/// conditionally are few and stable (a provider's endpoint per station, a
/// market's event), so this is room to spare, not a working limit: it
/// bounds the memory a source of ever-new URLs could take (18 November
/// 2026: the cache kept every response, of every URL, for good).
pub const MAX_CACHED_RESPONSES: usize = 64;

/// Thread-safe map of URL → latest response, at most `capacity` of them:
/// past that, the one used longest ago goes.
#[derive(Debug)]
pub struct ResponseCache {
    capacity: usize,
    inner: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    /// URL → (response, when it was last put or read).
    map: HashMap<String, (CachedResponse, u64)>,
    /// A counter of puts and reads: the order of use.
    clock: u64,
}

impl Entries {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::with_capacity(MAX_CACHED_RESPONSES)
    }
}

impl ResponseCache {
    /// A cache of at most `capacity` responses (at least one).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Entries::default()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn get(&self, url: &str) -> Option<CachedResponse> {
        let mut e = self.entries();
        let now = e.tick();
        let (value, used) = e.map.get_mut(url)?;
        *used = now;
        Some(value.clone())
    }

    pub fn put(&self, url: &str, value: CachedResponse) {
        let mut e = self.entries();
        let now = e.tick();
        e.map.insert(url.to_owned(), (value, now));
        while e.map.len() > self.capacity {
            let Some(oldest) = e
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(url, _)| url.clone())
            else {
                break;
            };
            e.map.remove(&oldest);
        }
    }

    pub fn len(&self) -> usize {
        self.entries().map.len()
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

    fn response(body: &'static [u8]) -> CachedResponse {
        CachedResponse {
            body: Bytes::from_static(body),
            etag: None,
            last_modified: None,
            content_type: None,
            fetched_at: Utc::now(),
        }
    }

    /// Past its capacity the cache lets go of the response used longest
    /// ago: a read counts as a use, so a URL asked again and again (304
    /// after 304) stays.
    #[test]
    fn response_cache_keeps_the_most_recently_used() {
        let c = ResponseCache::with_capacity(2);
        c.put("a", response(b"a"));
        c.put("b", response(b"b"));
        assert!(c.get("a").is_some());
        c.put("c", response(b"c"));
        assert_eq!(c.len(), 2);
        assert!(c.get("b").is_none(), "used longest ago");
        assert_eq!(&c.get("a").unwrap().body[..], b"a");
        assert_eq!(&c.get("c").unwrap().body[..], b"c");
        // A new response for a URL it holds replaces it, nothing goes.
        c.put("c", response(b"c2"));
        assert_eq!(c.len(), 2);
        assert_eq!(&c.get("c").unwrap().body[..], b"c2");
        assert!(c.get("a").is_some());
        // By default: MAX_CACHED_RESPONSES.
        let d = ResponseCache::default();
        for i in 0..MAX_CACHED_RESPONSES + 5 {
            d.put(&format!("u{i}"), response(b"x"));
        }
        assert_eq!(d.len(), MAX_CACHED_RESPONSES);
        assert!(d.get("u0").is_none());
        assert!(d.get(&format!("u{}", MAX_CACHED_RESPONSES + 4)).is_some());
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
