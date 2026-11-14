//! The `memory` store: rate limits and cached responses in this process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{CachedResponse, MAX_CACHE_ENTRIES};

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
pub struct MemoryStore {
    buckets: Mutex<HashMap<String, Bucket>>,
    cache: Mutex<HashMap<String, CachedResponse>>,
}

impl MemoryStore {
    /// Token bucket: `requests` per `per`. Returns how long to wait when the
    /// limit is hit.
    pub fn rate_limit(&self, key: &str, requests: u32, per: Duration) -> Option<Duration> {
        let now = Instant::now();
        let rate = requests as f64 / per.as_secs_f64();
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: requests as f64,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(requests as f64);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            None
        } else {
            let wait = (1.0 - bucket.tokens) / rate;
            Some(Duration::from_secs_f64(wait.max(0.001)))
        }
    }

    pub fn cache_get(&self, key: &str) -> Option<CachedResponse> {
        let cache = self.cache.lock().unwrap();
        cache.get(key).filter(|e| e.fresh()).cloned()
    }

    pub fn cache_put(&self, key: String, value: CachedResponse) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, v| v.fresh());
            if cache.len() >= MAX_CACHE_ENTRIES
                && let Some(k) = cache.keys().next().cloned()
            {
                cache.remove(&k);
            }
        }
        cache.insert(key, value);
    }

    /// Drops expired entries and idle buckets.
    pub fn sweep(&self) {
        self.cache.lock().unwrap().retain(|_, v| v.fresh());
        let idle = Duration::from_secs(3600);
        self.buckets
            .lock()
            .unwrap()
            .retain(|_, b| b.last.elapsed() < idle);
    }
}
