//! Shared state for rate limits and response caching: the binary's store
//! backends (`[store].backend`), behind the core's store traits.
//!
//! [`Store`] wraps a backend with what a long-running server needs on top:
//! a timeout on remote operations, failure counts for `/_unsworn/health` and
//! metrics, and rate-limited error logs. What a failure means for a request
//! is the core's call (see `unsworn_core::store`).

mod memory;
mod redis;

use std::fmt::Display;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use unsworn_core::config::{OnError, StoreBackend};
use unsworn_core::project::StoreConfig;
use unsworn_core::store::{self as core_store, RateLimiter, ResponseCache, StoreError};
pub use unsworn_core::store::{CachedResponse, MAX_CACHED_BODY, RateLimit};

use memory::MemoryStore;
use redis::RedisStore;

/// Store errors are logged at most this often; the rest are counted.
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// The wall clock in milliseconds since the Unix epoch, for cache entries.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

enum Backend {
    Memory(MemoryStore),
    Redis(RedisStore),
}

pub struct Store {
    pub config: StoreConfig,
    backend: Backend,
    errors: AtomicU64,
    up: AtomicBool,
    /// When an error was last logged, and how many were skipped since.
    last_log: Mutex<(Option<Instant>, u64)>,
}

impl Store {
    pub fn new(config: &StoreConfig) -> Result<Store, String> {
        let backend = match config.backend {
            StoreBackend::Memory => Backend::Memory(MemoryStore::default()),
            StoreBackend::Redis => {
                let url = config.url.as_deref().unwrap_or_default();
                Backend::Redis(RedisStore::new(url, &config.prefix)?)
            }
        };
        Ok(Store {
            config: config.clone(),
            backend,
            errors: AtomicU64::new(0),
            up: AtomicBool::new(true),
            last_log: Mutex::new((None, 0)),
        })
    }

    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Memory(_) => "memory",
            Backend::Redis(_) => "redis",
        }
    }

    /// Whether the store lives in this process, so its writes finish
    /// without waiting on the network.
    pub fn is_local(&self) -> bool {
        matches!(self.backend, Backend::Memory(_))
    }

    /// Whether the last store operation succeeded.
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    /// Failed store operations since start.
    pub fn error_count(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Counts a request against the bucket `key`, applying `on_error`.
    pub async fn rate_limit(&self, key: &str, requests: u32, per: Duration) -> RateLimit {
        core_store::rate_limit(self, key, requests, per, self.config.on_error).await
    }

    /// A fresh cached response. Store errors count as a miss.
    pub async fn cache_get(&self, key: &str) -> Option<CachedResponse> {
        core_store::cache_get(self, key, now_ms()).await
    }

    /// Stores a response. Failures are logged and counted, never returned.
    pub async fn cache_put(&self, key: String, value: CachedResponse) {
        let _ = self.put(&key, value).await;
    }

    /// Drops expired entries. Called periodically; only the memory store
    /// needs it.
    pub fn sweep(&self) {
        if let Backend::Memory(m) = &self.backend {
            m.sweep(now_ms());
        }
    }

    /// Runs a remote operation with [`redis::TIMEOUT`], noting failures and
    /// recovery.
    async fn timed<T, E: Display>(
        &self,
        op: &str,
        fut: impl Future<Output = Result<T, E>>,
    ) -> Result<T, StoreError> {
        let result = match tokio::time::timeout(redis::TIMEOUT, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("no answer within {}s", redis::TIMEOUT.as_secs())),
        };
        match result {
            Ok(v) => {
                if !self.up.swap(true, Ordering::Relaxed) {
                    tracing::info!(store = self.backend_name(), "the store is reachable again");
                }
                Ok(v)
            }
            Err(e) => {
                self.failed(op, &e);
                Err(StoreError(e))
            }
        }
    }

    fn failed(&self, op: &str, error: impl Display) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        self.up.store(false, Ordering::Relaxed);
        let mut last = self.last_log.lock().unwrap();
        if last.0.is_some_and(|t| t.elapsed() < ERROR_LOG_INTERVAL) {
            last.1 += 1;
            return;
        }
        let skipped = std::mem::take(&mut last.1);
        last.0 = Some(Instant::now());
        drop(last);
        let fallback = match (op, self.config.on_error) {
            ("rate limit", OnError::Open) => "letting requests through uncounted",
            ("rate limit", OnError::Closed) => "answering 503",
            _ => "skipping the cache",
        };
        tracing::error!(
            store = self.backend_name(),
            skipped,
            "{op} failed ({error}); {fallback}"
        );
    }
}

impl RateLimiter for Store {
    async fn take(
        &self,
        key: &str,
        requests: u32,
        per: Duration,
    ) -> Result<Option<Duration>, StoreError> {
        match &self.backend {
            Backend::Memory(m) => Ok(m.take(key, requests, per)),
            Backend::Redis(r) => self.timed("rate limit", r.take(key, requests, per)).await,
        }
    }
}

impl ResponseCache for Store {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, StoreError> {
        match &self.backend {
            Backend::Memory(m) => Ok(m.get(key)),
            Backend::Redis(r) => self.timed("cache read", r.get(key)).await,
        }
    }

    async fn put(&self, key: &str, value: CachedResponse) -> Result<(), StoreError> {
        match &self.backend {
            Backend::Memory(m) => {
                m.put(key.to_string(), value, now_ms());
                Ok(())
            }
            Backend::Redis(r) => self.timed("cache write", r.put(key, &value)).await,
        }
    }
}

#[cfg(test)]
mod tests;
