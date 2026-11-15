//! Shared state for `rate_limit` and `cache`: what a host's store backends
//! implement, and the rules every backend follows.
//!
//! A store is two traits, because platforms back them differently: rate
//! limits need consistent increments (a Redis script, a Durable Object),
//! while cached responses suit an eventually consistent cache (Redis, the
//! Cache API, KV). One type may implement both, as the binary's memory and
//! Redis stores do.
//!
//! Backends report failures as [`StoreError`]. What a failure means for the
//! request (`[store].on_error`) is decided here, so every host answers the
//! same way.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::config::OnError;
use crate::{MaybeSend, MaybeSync};

/// Responses larger than this aren't cached.
pub const MAX_CACHED_BODY: usize = 1024 * 1024;

/// A store operation that failed: unreachable, timed out, or a bad reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

/// Counts requests against rate limits.
pub trait RateLimiter: MaybeSend + MaybeSync {
    /// Takes one token from the bucket `key`, which holds `requests` tokens
    /// and refills at `requests` per `per` (see [`take_token`]). Returns
    /// `None` when the request is allowed, or how long until a token is
    /// available.
    fn take(
        &self,
        key: &str,
        requests: u32,
        per: Duration,
    ) -> impl Future<Output = Result<Option<Duration>, StoreError>> + MaybeSend;
}

/// Keeps cached responses.
pub trait ResponseCache: MaybeSend + MaybeSync {
    /// The entry under `key`. Returning an expired entry is fine: callers
    /// check freshness with the host's clock.
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<CachedResponse>, StoreError>> + MaybeSend;

    /// Stores `value` under `key` for at least `value.ttl`.
    fn put(
        &self,
        key: &str,
        value: CachedResponse,
    ) -> impl Future<Output = Result<(), StoreError>> + MaybeSend;
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

impl RateLimit {
    /// Applies `[store].on_error` to a backend's answer.
    pub fn from_result(result: Result<Option<Duration>, StoreError>, on_error: OnError) -> Self {
        match result {
            Ok(None) => RateLimit::Allowed,
            Ok(Some(retry_after)) => RateLimit::Limited { retry_after },
            Err(_) => match on_error {
                OnError::Open => RateLimit::Allowed,
                OnError::Closed => RateLimit::Unavailable,
            },
        }
    }
}

/// Counts a request against the bucket `key`.
pub async fn rate_limit<S: RateLimiter + ?Sized>(
    store: &S,
    key: &str,
    requests: u32,
    per: Duration,
    on_error: OnError,
) -> RateLimit {
    RateLimit::from_result(store.take(key, requests, per).await, on_error)
}

/// A fresh cached response under `key`. Store errors count as a miss.
/// `now_ms` is the host's wall clock in milliseconds since the Unix epoch.
pub async fn cache_get<S: ResponseCache + ?Sized>(
    store: &S,
    key: &str,
    now_ms: u64,
) -> Option<CachedResponse> {
    store
        .get(key)
        .await
        .ok()
        .flatten()
        .filter(|e| e.fresh(now_ms))
}

/// A token bucket's state: the tokens left, and when it was last counted,
/// on whatever monotonic clock the backend keeps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub tokens: f64,
    pub last: Duration,
}

/// The token bucket every backend implements: a bucket holds up to
/// `requests` tokens, refills at `requests` per `per`, and each request
/// takes one. A missing bucket starts full. Returns the new state, and
/// `None` when the request is allowed or how long until a token is
/// available.
///
/// The Redis backend runs the same algorithm as a Lua script on Redis's
/// clock; keep the two in step.
pub fn take_token(
    bucket: Option<Bucket>,
    now: Duration,
    requests: u32,
    per: Duration,
) -> (Bucket, Option<Duration>) {
    let capacity = f64::from(requests);
    let rate = capacity / per.as_secs_f64();
    let mut b = bucket.unwrap_or(Bucket {
        tokens: capacity,
        last: now,
    });
    let elapsed = now.saturating_sub(b.last).as_secs_f64();
    b.tokens = (b.tokens + elapsed * rate).min(capacity);
    b.last = now;
    if b.tokens >= 1.0 {
        b.tokens -= 1.0;
        (b, None)
    } else {
        let wait = (1.0 - b.tokens) / rate;
        (b, Some(Duration::from_secs_f64(wait.max(0.001))))
    }
}

/// A cached response. Header values are text: a value that isn't can't be
/// cached, and cacheable responses practically never have one.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// When it was stored, in milliseconds since the Unix epoch.
    pub stored_ms: u64,
    pub ttl: Duration,
}

/// The JSON line at the start of an encoded entry.
#[derive(Serialize, Deserialize)]
struct Meta {
    status: u16,
    headers: Vec<(String, String)>,
    stored_ms: u64,
    ttl_ms: u64,
}

impl CachedResponse {
    /// How old the entry is at `now_ms`.
    pub fn age(&self, now_ms: u64) -> Duration {
        Duration::from_millis(now_ms.saturating_sub(self.stored_ms))
    }

    pub fn fresh(&self, now_ms: u64) -> bool {
        self.age(now_ms) < self.ttl
    }

    /// Encodes the entry for a byte store: a JSON line with the status,
    /// headers and timing, then the raw body.
    pub fn encode(&self) -> Vec<u8> {
        let meta = Meta {
            status: self.status,
            headers: self.headers.clone(),
            stored_ms: self.stored_ms,
            ttl_ms: self.ttl.as_millis() as u64,
        };
        let mut out = serde_json::to_vec(&meta).unwrap_or_default();
        out.push(b'\n');
        out.extend_from_slice(&self.body);
        out
    }

    /// Decodes what [`CachedResponse::encode`] wrote.
    pub fn decode(raw: &[u8]) -> Option<CachedResponse> {
        let split = raw.iter().position(|b| *b == b'\n')?;
        let meta: Meta = serde_json::from_slice(&raw[..split]).ok()?;
        Some(CachedResponse {
            status: meta.status,
            headers: meta.headers,
            body: Bytes::copy_from_slice(&raw[split + 1..]),
            stored_ms: meta.stored_ms,
            ttl: Duration::from_millis(meta.ttl_ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket() {
        let per = Duration::from_secs(60);
        let mut state = None;
        let t0 = Duration::from_secs(100);
        for _ in 0..3 {
            let (b, wait) = take_token(state, t0, 3, per);
            assert_eq!(wait, None);
            state = Some(b);
        }
        let (b, wait) = take_token(state, t0, 3, per);
        assert_eq!(wait, Some(Duration::from_secs(20)));
        // A third of the refill period later, a token is back.
        let (_, wait) = take_token(Some(b), t0 + Duration::from_secs(20), 3, per);
        assert_eq!(wait, None);
        // A clock that steps back doesn't add tokens.
        let (_, wait) = take_token(Some(b), t0 - Duration::from_secs(5), 3, per);
        assert!(wait.is_some());
    }

    #[test]
    fn on_error() {
        let failed = || Err(StoreError("down".into()));
        assert_eq!(
            RateLimit::from_result(failed(), OnError::Open),
            RateLimit::Allowed
        );
        assert_eq!(
            RateLimit::from_result(failed(), OnError::Closed),
            RateLimit::Unavailable
        );
        assert_eq!(
            RateLimit::from_result(Ok(Some(Duration::from_secs(2))), OnError::Closed),
            RateLimit::Limited {
                retry_after: Duration::from_secs(2)
            }
        );
    }

    #[test]
    fn entries_round_trip_and_expire() {
        let value = CachedResponse {
            status: 201,
            headers: vec![
                ("content-type".into(), "text/plain".into()),
                ("x-a".into(), "1".into()),
                ("x-a".into(), "2".into()),
            ],
            body: Bytes::from_static(b"line one\nline two\x00\xff"),
            stored_ms: 1_000,
            ttl: Duration::from_secs(30),
        };
        assert_eq!(CachedResponse::decode(&value.encode()), Some(value.clone()));
        assert!(value.fresh(30_999));
        assert!(!value.fresh(31_000));
        assert_eq!(value.age(4_500), Duration::from_millis(3_500));
        assert_eq!(value.age(0), Duration::ZERO);
        assert_eq!(CachedResponse::decode(b"not json"), None);
    }
}
