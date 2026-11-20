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

/// The token bucket ([`take_token`]) as a Redis script, run atomically on
/// Redis's clock, so instances with skewed clocks still agree. `KEYS[1]` is
/// the bucket; `ARGV` is the capacity and the refill period in
/// milliseconds. Returns how many milliseconds to wait, or 0 when the
/// request is allowed. Every host that keeps rate limits in Redis runs this
/// script on `{prefix}rl:{key}`, so they can share buckets.
pub const REDIS_RATE_LIMIT: &str = r#"
local capacity = tonumber(ARGV[1])
local per_ms = tonumber(ARGV[2])
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local state = redis.call('HMGET', KEYS[1], 'tokens', 'ts')
local tokens = tonumber(state[1]) or capacity
local ts = tonumber(state[2]) or now
local rate = capacity / per_ms
tokens = math.min(capacity, tokens + math.max(0, now - ts) * rate)
local wait = 0
if tokens >= 1 then
  tokens = tokens - 1
else
  wait = math.max(1, math.ceil((1 - tokens) / rate))
end
redis.call('HSET', KEYS[1], 'tokens', tostring(tokens), 'ts', tostring(now))
redis.call('PEXPIRE', KEYS[1], math.ceil(per_ms))
return wait
"#;

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

impl<T: RateLimiter + ?Sized> RateLimiter for std::sync::Arc<T> {
    fn take(
        &self,
        key: &str,
        requests: u32,
        per: Duration,
    ) -> impl Future<Output = Result<Option<Duration>, StoreError>> + MaybeSend {
        (**self).take(key, requests, per)
    }
}

impl<T: ResponseCache + ?Sized> ResponseCache for std::sync::Arc<T> {
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<CachedResponse>, StoreError>> + MaybeSend {
        (**self).get(key)
    }

    fn put(
        &self,
        key: &str,
        value: CachedResponse,
    ) -> impl Future<Output = Result<(), StoreError>> + MaybeSend {
        (**self).put(key, value)
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

/// A request path as the key cached responses are invalidated by: its
/// segments, escapes resolved, each written so that no two paths share a
/// key. Query strings aren't part of it: invalidating `/users/7` covers
/// every response cached for that path.
pub fn path_key<'a>(segments: impl Iterator<Item = &'a str>) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let mut key = String::from("inval|");
    for segment in segments {
        key.push('/');
        key.extend(utf8_percent_encode(segment, NON_ALPHANUMERIC));
    }
    key
}

/// Marks the cached responses for a path as out of date from `now_ms` on.
///
/// The mark is an entry of its own, written with the one operation every
/// store has. A cached response older than the mark for its path is a
/// miss ([`invalidated`]). The mark is kept for `ttl`, which has to be at
/// least as long as any response it covers can live.
pub async fn invalidate<S: ResponseCache + ?Sized>(
    store: &S,
    path_key: &str,
    now_ms: u64,
    ttl: Duration,
) -> Result<(), StoreError> {
    let mark = CachedResponse {
        status: 0,
        headers: Vec::new(),
        body: Bytes::new(),
        stored_ms: now_ms,
        ttl,
    };
    store.put(path_key, mark).await
}

/// Whether a response cached at `stored_ms` has been invalidated since.
/// One stored in the same millisecond as the mark counts as before it. A
/// store that can't be read counts as invalidated: a fresh answer is
/// always a right one.
pub async fn invalidated<S: ResponseCache + ?Sized>(
    store: &S,
    path_key: &str,
    stored_ms: u64,
) -> bool {
    match store.get(path_key).await {
        Ok(Some(mark)) => mark.stored_ms >= stored_ms,
        Ok(None) => false,
        Err(_) => true,
    }
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
