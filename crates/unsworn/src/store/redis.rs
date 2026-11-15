//! The `redis` store: rate limits and cached responses shared by every
//! instance that uses the same Redis (or Valkey, Dragonfly, ...).

use std::time::Duration;

use redis::AsyncCommands;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use super::CachedResponse;

/// How long one store operation may take before it counts as failed.
pub const TIMEOUT: Duration = Duration::from_secs(1);

/// The core's token bucket (`unsworn_core::store::take_token`), run
/// atomically in Redis on Redis's clock, so instances with skewed clocks still agree. Returns
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

    pub async fn take(
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

    /// An entry that doesn't decode (written by another version, say)
    /// counts as a miss.
    pub async fn get(&self, key: &str) -> redis::RedisResult<Option<CachedResponse>> {
        let mut conn = self.conn.clone();
        let raw: Option<Vec<u8>> = conn.get(format!("{}cache:{key}", self.prefix)).await?;
        Ok(raw.and_then(|r| CachedResponse::decode(&r)))
    }

    pub async fn put(&self, key: &str, value: &CachedResponse) -> redis::RedisResult<()> {
        let ttl_ms = value.ttl.as_millis().max(1) as u64;
        let mut conn = self.conn.clone();
        conn.pset_ex::<_, _, ()>(
            format!("{}cache:{key}", self.prefix),
            value.encode(),
            ttl_ms,
        )
        .await
    }
}
