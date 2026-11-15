//! The `memory` store: rate limits and cached responses in this process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use unsworn_core::store::{Bucket, CachedResponse, take_token};

pub const MAX_CACHE_ENTRIES: usize = 10_000;

/// Buckets idle this long are dropped by [`MemoryStore::sweep`].
const IDLE_BUCKET: Duration = Duration::from_secs(3600);

pub struct MemoryStore {
    /// The zero of the buckets' monotonic clock.
    epoch: Instant,
    buckets: Mutex<HashMap<String, Bucket>>,
    cache: Mutex<HashMap<String, CachedResponse>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore {
            epoch: Instant::now(),
            buckets: Mutex::default(),
            cache: Mutex::default(),
        }
    }
}

impl MemoryStore {
    /// Takes a token from the bucket `key` (see `take_token`).
    pub fn take(&self, key: &str, requests: u32, per: Duration) -> Option<Duration> {
        let now = self.epoch.elapsed();
        let mut buckets = self.buckets.lock().unwrap();
        let (bucket, wait) = take_token(buckets.get(key).copied(), now, requests, per);
        buckets.insert(key.to_string(), bucket);
        wait
    }

    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        self.cache.lock().unwrap().get(key).cloned()
    }

    pub fn put(&self, key: String, value: CachedResponse, now_ms: u64) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, v| v.fresh(now_ms));
            if cache.len() >= MAX_CACHE_ENTRIES
                && let Some(k) = cache.keys().next().cloned()
            {
                cache.remove(&k);
            }
        }
        cache.insert(key, value);
    }

    /// Drops expired entries and idle buckets.
    pub fn sweep(&self, now_ms: u64) {
        self.cache.lock().unwrap().retain(|_, v| v.fresh(now_ms));
        let now = self.epoch.elapsed();
        self.buckets
            .lock()
            .unwrap()
            .retain(|_, b| now.saturating_sub(b.last) < IDLE_BUCKET);
    }
}
