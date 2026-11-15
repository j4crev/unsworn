//! The `memory` store: rate limits and cached responses in this process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use unsworn_core::store::{Bucket, CachedResponse, take_token};

pub const MAX_CACHE_ENTRIES: usize = 10_000;

/// The most rate-limit buckets kept. A client that could mint keys without
/// end (many addresses, say) would otherwise fill memory with them.
pub const MAX_BUCKETS: usize = 200_000;

pub struct MemoryStore {
    /// The zero of the buckets' monotonic clock.
    epoch: Instant,
    /// Each bucket with its refill period, which is how long it's kept.
    buckets: Mutex<HashMap<String, (Bucket, Duration)>>,
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
        let known = buckets.get(key).map(|(b, _)| *b);
        if known.is_none() && buckets.len() >= MAX_BUCKETS {
            buckets.retain(|_, b| !refilled(b, now));
            // Still full of buckets in use: one makes way, as in the cache.
            if buckets.len() >= MAX_BUCKETS
                && let Some(k) = buckets.keys().next().cloned()
            {
                buckets.remove(&k);
            }
        }
        let (bucket, wait) = take_token(known, now, requests, per);
        buckets.insert(key.to_string(), (bucket, per));
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

    /// Drops expired entries and refilled buckets.
    pub fn sweep(&self, now_ms: u64) {
        self.cache.lock().unwrap().retain(|_, v| v.fresh(now_ms));
        let now = self.epoch.elapsed();
        self.buckets
            .lock()
            .unwrap()
            .retain(|_, b| !refilled(b, now));
    }

    #[cfg(test)]
    pub fn bucket_count(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

/// Whether a bucket has been idle for its whole refill period. It's full
/// again by then, the same as a new one, so nothing is lost by dropping it.
fn refilled((bucket, per): &(Bucket, Duration), now: Duration) -> bool {
    now.saturating_sub(bucket.last) >= *per
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refilled_buckets_are_dropped() {
        let store = MemoryStore::default();
        store.take("short", 1, Duration::from_millis(20));
        store.take("long", 1, Duration::from_secs(60));
        assert_eq!(store.bucket_count(), 2);
        std::thread::sleep(Duration::from_millis(40));
        store.sweep(0);
        // `short` is full again, so dropping it loses nothing; `long`
        // still remembers the token it gave out.
        assert_eq!(store.bucket_count(), 1);
        assert!(store.take("long", 1, Duration::from_secs(60)).is_some());
        assert!(store.take("short", 1, Duration::from_millis(20)).is_none());
    }
}
