//! The `memory` store: rate limits and cached responses in this process.
//!
//! Everything here is bounded, because clients choose the keys: a new query
//! string is a new cache entry, and a new address a new bucket. When a
//! space is full, what goes first is what loses least: the bucket that
//! refills soonest (one already full again loses nothing), and the cache
//! entry that expires soonest. Each space keeps its entries ordered for
//! that, so making room never scans the whole map.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use unsworn_core::store::{Bucket, CachedResponse, take_token};

/// The most cached responses kept.
pub const MAX_CACHE_ENTRIES: usize = 10_000;

/// The most bytes of cached responses kept. Without it, 10,000 entries of
/// up to 1 MiB each could take 10 GiB. A project that needs more caches in
/// Redis.
pub const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// The most remembered API-key verdicts. They have a space of their own, so
/// a client sending made-up keys can't push cached responses out, and
/// responses can't push out verdicts.
pub const MAX_VERDICTS: usize = 10_000;
pub const MAX_VERDICT_BYTES: usize = 8 * 1024 * 1024;

/// The most rate-limit buckets kept. A client that could mint keys without
/// end (many addresses, say) would otherwise fill memory with them.
pub const MAX_BUCKETS: usize = 200_000;

/// The keys of remote API-key verdicts (see `KeyVerifier::cache_key`).
const VERDICT_PREFIX: &str = "auth|";

pub struct MemoryStore {
    /// The zero of the buckets' monotonic clock.
    epoch: Instant,
    buckets: Mutex<Buckets>,
    responses: Mutex<Cache>,
    verdicts: Mutex<Cache>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::with_limits(
            MAX_BUCKETS,
            (MAX_CACHE_ENTRIES, MAX_CACHE_BYTES),
            (MAX_VERDICTS, MAX_VERDICT_BYTES),
        )
    }
}

impl MemoryStore {
    /// A store with other limits than the defaults: buckets, then (entries,
    /// bytes) for responses and for verdicts.
    pub fn with_limits(
        buckets: usize,
        responses: (usize, usize),
        verdicts: (usize, usize),
    ) -> Self {
        MemoryStore {
            epoch: Instant::now(),
            buckets: Mutex::new(Buckets::new(buckets)),
            responses: Mutex::new(Cache::new(responses.0, responses.1)),
            verdicts: Mutex::new(Cache::new(verdicts.0, verdicts.1)),
        }
    }

    /// Takes a token from the bucket `key` (see `take_token`).
    pub fn take(&self, key: &str, requests: u32, per: Duration) -> Option<Duration> {
        let now = self.epoch.elapsed();
        self.buckets.lock().unwrap().take(key, now, requests, per)
    }

    fn cache(&self, key: &str) -> &Mutex<Cache> {
        if key.starts_with(VERDICT_PREFIX) {
            &self.verdicts
        } else {
            &self.responses
        }
    }

    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        self.cache(key).lock().unwrap().get(key)
    }

    pub fn put(&self, key: String, value: CachedResponse, now_ms: u64) {
        self.cache(&key).lock().unwrap().put(key, value, now_ms);
    }

    /// Drops expired entries and refilled buckets.
    pub fn sweep(&self, now_ms: u64) {
        self.responses.lock().unwrap().sweep(now_ms);
        self.verdicts.lock().unwrap().sweep(now_ms);
        let now = self.epoch.elapsed();
        self.buckets.lock().unwrap().sweep(now);
    }

    #[cfg(test)]
    pub fn bucket_count(&self) -> usize {
        self.buckets.lock().unwrap().map.len()
    }

    #[cfg(test)]
    pub fn cache_usage(&self) -> (usize, usize) {
        let cache = self.responses.lock().unwrap();
        (cache.map.len(), cache.bytes)
    }
}

/// Rate-limit buckets, ordered by when each is full again.
struct Buckets {
    max: usize,
    /// Each bucket, with when it's full again and its place in `by_full`.
    map: HashMap<String, (Bucket, Duration, u64)>,
    by_full: BTreeMap<(Duration, u64), String>,
    next: u64,
}

impl Buckets {
    fn new(max: usize) -> Self {
        Buckets {
            max,
            map: HashMap::new(),
            by_full: BTreeMap::new(),
            next: 0,
        }
    }

    fn take(&mut self, key: &str, now: Duration, requests: u32, per: Duration) -> Option<Duration> {
        let known = self.map.remove(key);
        if let Some((_, full, seq)) = &known {
            self.by_full.remove(&(*full, *seq));
        } else if self.map.len() >= self.max {
            // The bucket closest to full makes way: one that's full already
            // is the same as a new one, so nothing is lost.
            if let Some((_, k)) = self.by_full.pop_first() {
                self.map.remove(&k);
            }
        }
        let (bucket, wait) = take_token(known.map(|(b, _, _)| b), now, requests, per);
        let full = full_at(&bucket, requests, per);
        let seq = self.next;
        self.next += 1;
        self.by_full.insert((full, seq), key.to_string());
        self.map.insert(key.to_string(), (bucket, full, seq));
        wait
    }

    fn sweep(&mut self, now: Duration) {
        while let Some(entry) = self.by_full.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let key = entry.remove();
            self.map.remove(&key);
        }
    }
}

/// When a bucket holds all its tokens again, on the store's clock.
fn full_at(bucket: &Bucket, requests: u32, per: Duration) -> Duration {
    let capacity = f64::from(requests);
    let missing = (capacity - bucket.tokens).max(0.0);
    let secs = missing / capacity * per.as_secs_f64();
    bucket.last + Duration::from_secs_f64(secs.min(per.as_secs_f64()))
}

/// Cached entries, ordered by when each expires, within a count and a byte
/// budget.
struct Cache {
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
    /// Each entry, with its size, its expiry and its place in `by_expiry`.
    map: HashMap<String, (CachedResponse, usize, u64, u64)>,
    by_expiry: BTreeMap<(u64, u64), String>,
    next: u64,
}

impl Cache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Cache {
            max_entries,
            max_bytes,
            bytes: 0,
            map: HashMap::new(),
            by_expiry: BTreeMap::new(),
            next: 0,
        }
    }

    fn get(&self, key: &str) -> Option<CachedResponse> {
        self.map.get(key).map(|(v, _, _, _)| v.clone())
    }

    fn remove(&mut self, key: &str) {
        if let Some((_, size, expires, seq)) = self.map.remove(key) {
            self.by_expiry.remove(&(expires, seq));
            self.bytes -= size;
        }
    }

    fn put(&mut self, key: String, value: CachedResponse, now_ms: u64) {
        self.remove(&key);
        let size = entry_size(&key, &value);
        if size > self.max_bytes || !value.fresh(now_ms) {
            return;
        }
        // Expired entries come first in `by_expiry`, then the ones that
        // expire soonest.
        while self.map.len() >= self.max_entries || self.bytes + size > self.max_bytes {
            let Some((_, k)) = self.by_expiry.pop_first() else {
                break;
            };
            if let Some((_, s, _, _)) = self.map.remove(&k) {
                self.bytes -= s;
            }
        }
        let expires = value.stored_ms.saturating_add(value.ttl.as_millis() as u64);
        let seq = self.next;
        self.next += 1;
        self.by_expiry.insert((expires, seq), key.clone());
        self.bytes += size;
        self.map.insert(key, (value, size, expires, seq));
    }

    fn sweep(&mut self, now_ms: u64) {
        while let Some(entry) = self.by_expiry.first_entry() {
            if entry.key().0 > now_ms {
                break;
            }
            let key = entry.remove();
            if let Some((_, s, _, _)) = self.map.remove(&key) {
                self.bytes -= s;
            }
        }
    }
}

/// Roughly what an entry takes in memory: its key, body and headers, and
/// a little for the bookkeeping around them.
fn entry_size(key: &str, value: &CachedResponse) -> usize {
    let headers: usize = value.headers.iter().map(|(k, v)| k.len() + v.len()).sum();
    key.len() * 2 + value.body.len() + headers + 128
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: usize, stored_ms: u64, ttl_secs: u64) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: vec![],
            body: vec![b'x'; body].into(),
            stored_ms,
            ttl: Duration::from_secs(ttl_secs),
        }
    }

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

    #[test]
    fn a_full_bucket_space_drops_the_bucket_closest_to_full() {
        let store = MemoryStore::with_limits(2, (10, 1 << 20), (10, 1 << 20));
        // `slow` refills over an hour, `fast` over a second.
        assert!(store.take("slow", 1, Duration::from_secs(3600)).is_none());
        assert!(store.take("fast", 1, Duration::from_secs(1)).is_none());
        store.take("new", 1, Duration::from_secs(1));
        assert_eq!(store.bucket_count(), 2);
        // `fast` made way; `slow` still counts its request.
        assert!(store.take("slow", 1, Duration::from_secs(3600)).is_some());
    }

    #[test]
    fn the_cache_keeps_to_its_byte_budget() {
        let store = MemoryStore::with_limits(10, (100, 10_000), (10, 10_000));
        // Each entry is a little over 3,000 bytes, so three fit.
        for (i, ttl) in [(0, 30), (1, 10), (2, 60)] {
            store.put(format!("resp|{i}"), entry(3_000, 0, ttl), 0);
        }
        assert_eq!(store.cache_usage().0, 3);
        store.put("resp|3".into(), entry(3_000, 0, 60), 0);
        // The one expiring soonest made way.
        assert!(store.get("resp|1").is_none());
        for kept in ["resp|0", "resp|2", "resp|3"] {
            assert!(store.get(kept).is_some(), "{kept}");
        }
        let (_, bytes) = store.cache_usage();
        assert!(bytes <= 10_000, "{bytes}");
        // One larger than the whole budget isn't kept.
        store.put("resp|big".into(), entry(20_000, 0, 60), 0);
        assert!(store.get("resp|big").is_none());
        assert_eq!(store.cache_usage().0, 3);
        // Replacing an entry doesn't count it twice.
        store.put("resp|0".into(), entry(3_000, 0, 30), 0);
        assert_eq!(store.cache_usage().0, 3);
        // Expired entries are swept.
        store.sweep(31_000);
        assert_eq!(store.cache_usage().0, 2);
    }

    #[test]
    fn verdicts_have_their_own_space() {
        let store = MemoryStore::with_limits(10, (2, 1 << 20), (2, 1 << 20));
        store.put("resp|page".into(), entry(10, 0, 60), 0);
        for i in 0..5 {
            store.put(format!("auth|{i}"), entry(10, 0, 60), 0);
        }
        assert!(
            store.get("resp|page").is_some(),
            "made-up keys can't push responses out"
        );
        assert!(store.get("auth|4").is_some());
    }
}
