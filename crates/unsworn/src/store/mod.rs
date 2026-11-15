//! Shared state for rate limits and response caching, behind one interface
//! with a backend per `[store].backend`.

mod memory;
mod redis;

use std::fmt::Display;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use http::HeaderValue;
use unsworn_core::config::{OnError, StoreBackend};
use unsworn_core::project::StoreConfig;

use memory::MemoryStore;
use redis::RedisStore;

pub const MAX_CACHE_ENTRIES: usize = 10_000;
/// Responses larger than this aren't cached.
pub const MAX_CACHED_BODY: usize = 1024 * 1024;
/// Store errors are logged at most this often; the rest are counted.
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// A cached response.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, HeaderValue)>,
    pub body: Bytes,
    pub stored: SystemTime,
    pub ttl: Duration,
}

impl CachedResponse {
    pub fn age(&self) -> Duration {
        self.stored.elapsed().unwrap_or_default()
    }

    fn fresh(&self) -> bool {
        self.age() < self.ttl
    }
}

/// The outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimit {
    Allowed,
    Limited {
        retry_after: Duration,
    },
    /// The store failed and `on_error = "closed"`.
    Unavailable,
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

    /// Token bucket: `requests` per `per`, per key.
    pub async fn rate_limit(&self, key: &str, requests: u32, per: Duration) -> RateLimit {
        let result = match &self.backend {
            Backend::Memory(m) => Ok(m.rate_limit(key, requests, per)),
            Backend::Redis(r) => self.timed(r.rate_limit(key, requests, per)).await,
        };
        match result {
            Ok(None) => RateLimit::Allowed,
            Ok(Some(retry_after)) => RateLimit::Limited { retry_after },
            Err(e) => {
                self.failed("rate limit", e);
                match self.config.on_error {
                    OnError::Open => RateLimit::Allowed,
                    OnError::Closed => RateLimit::Unavailable,
                }
            }
        }
    }

    /// A fresh cached response. Store errors count as a miss.
    pub async fn cache_get(&self, key: &str) -> Option<CachedResponse> {
        let result = match &self.backend {
            Backend::Memory(m) => return m.cache_get(key),
            Backend::Redis(r) => self.timed(r.cache_get(key)).await,
        };
        result.unwrap_or_else(|e| {
            self.failed("cache read", e);
            None
        })
    }

    pub async fn cache_put(&self, key: String, value: CachedResponse) {
        let result = match &self.backend {
            Backend::Memory(m) => return m.cache_put(key, value),
            Backend::Redis(r) => self.timed(r.cache_put(&key, &value)).await,
        };
        if let Err(e) = result {
            self.failed("cache write", e);
        }
    }

    /// Drops expired entries. Called periodically; only the memory store
    /// needs it.
    pub fn sweep(&self) {
        if let Backend::Memory(m) = &self.backend {
            m.sweep();
        }
    }

    /// Runs a remote operation with [`redis::TIMEOUT`], and notes recovery.
    async fn timed<T, E: Display>(
        &self,
        op: impl Future<Output = Result<T, E>>,
    ) -> Result<T, String> {
        match tokio::time::timeout(redis::TIMEOUT, op).await {
            Ok(Ok(v)) => {
                if !self.up.swap(true, Ordering::Relaxed) {
                    tracing::info!(store = self.backend_name(), "the store is reachable again");
                }
                Ok(v)
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("no answer within {}s", redis::TIMEOUT.as_secs())),
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

#[cfg(test)]
mod tests;
