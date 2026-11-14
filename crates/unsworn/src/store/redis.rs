//! The `redis` store: rate limits and cached responses shared by every
//! instance that uses the same Redis (or Valkey, Dragonfly, ...).

use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use http::HeaderValue;
use redis::AsyncCommands;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use serde::{Deserialize, Serialize};

use super::CachedResponse;

/// How long one store operation may take before it counts as failed.
pub const TIMEOUT: Duration = Duration::from_secs(1);

/// The same token bucket as the memory store, run atomically in Redis on
/// Redis's clock, so instances with skewed clocks still agree. Returns
/// how many milliseconds to wait, or 0 when the request is allowed.
const RATE_LIMIT: &str = r#"
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

pub struct RedisStore {
    conn: ConnectionManager,
    prefix: String,
    script: redis::Script,
}

impl RedisStore {
    /// Connects lazily: Unsworn starts even when Redis is down, and the
    /// connection is retried as requests need it.
    pub fn new(url: &str, prefix: &str) -> Result<RedisStore, String> {
        let client = redis::Client::open(url).map_err(|e| format!("[store].url: {e}"))?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(TIMEOUT))
            .set_response_timeout(Some(TIMEOUT))
            .set_number_of_retries(1);
        let conn = ConnectionManager::new_lazy_with_config(client, config)
            .map_err(|e| format!("[store]: {e}"))?;
        Ok(RedisStore {
            conn,
            prefix: prefix.to_string(),
            script: redis::Script::new(RATE_LIMIT),
        })
    }

    pub async fn rate_limit(
        &self,
        key: &str,
        requests: u32,
        per: Duration,
    ) -> redis::RedisResult<Option<Duration>> {
        let mut conn = self.conn.clone();
        let wait: u64 = self
            .script
            .key(format!("{}rl:{key}", self.prefix))
            .arg(requests)
            .arg(per.as_secs_f64() * 1000.0)
            .invoke_async(&mut conn)
            .await?;
        Ok((wait > 0).then(|| Duration::from_millis(wait)))
    }

    pub async fn cache_get(&self, key: &str) -> redis::RedisResult<Option<CachedResponse>> {
        let mut conn = self.conn.clone();
        let raw: Option<Vec<u8>> = conn.get(format!("{}cache:{key}", self.prefix)).await?;
        Ok(raw.and_then(|r| decode(&r)).filter(|e| e.fresh()))
    }

    pub async fn cache_put(&self, key: &str, value: &CachedResponse) -> redis::RedisResult<()> {
        let ttl_ms = value.ttl.as_millis().max(1) as u64;
        let mut conn = self.conn.clone();
        conn.pset_ex::<_, _, ()>(format!("{}cache:{key}", self.prefix), encode(value), ttl_ms)
            .await
    }
}

/// A cached response: a JSON line with the status, headers and timing,
/// then the raw body.
#[derive(Serialize, Deserialize)]
struct Meta {
    status: u16,
    headers: Vec<(String, String)>,
    stored_ms: u64,
    ttl_ms: u64,
}

fn encode(value: &CachedResponse) -> Vec<u8> {
    let meta = Meta {
        status: value.status,
        // Header values that aren't text can't be cached in Redis; cacheable
        // responses practically never have them.
        headers: value
            .headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.clone(), v.to_string())))
            .collect(),
        stored_ms: value
            .stored
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        ttl_ms: value.ttl.as_millis() as u64,
    };
    let mut out = serde_json::to_vec(&meta).unwrap_or_default();
    out.push(b'\n');
    out.extend_from_slice(&value.body);
    out
}

fn decode(raw: &[u8]) -> Option<CachedResponse> {
    let split = raw.iter().position(|b| *b == b'\n')?;
    let meta: Meta = serde_json::from_slice(&raw[..split]).ok()?;
    Some(CachedResponse {
        status: meta.status,
        headers: meta
            .headers
            .into_iter()
            .filter_map(|(k, v)| HeaderValue::from_str(&v).ok().map(|v| (k, v)))
            .collect(),
        body: Bytes::copy_from_slice(&raw[split + 1..]),
        stored: UNIX_EPOCH + Duration::from_millis(meta.stored_ms),
        ttl: Duration::from_millis(meta.ttl_ms),
    })
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    #[test]
    fn cached_responses_round_trip() {
        let value = CachedResponse {
            status: 201,
            headers: vec![
                (
                    "content-type".into(),
                    HeaderValue::from_static("text/plain"),
                ),
                ("x-a".into(), HeaderValue::from_static("1")),
                ("x-a".into(), HeaderValue::from_static("2")),
            ],
            body: Bytes::from_static(b"line one\nline two\x00\xff"),
            stored: SystemTime::now(),
            ttl: Duration::from_secs(30),
        };
        let back = decode(&encode(&value)).unwrap();
        assert_eq!(back.status, 201);
        assert_eq!(back.headers, value.headers);
        assert_eq!(back.body, value.body);
        assert_eq!(back.ttl, value.ttl);
    }
}
