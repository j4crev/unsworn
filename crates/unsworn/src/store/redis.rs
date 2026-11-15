//! The `redis` store: rate limits and cached responses shared by every
//! instance that uses the same Redis (or Valkey, Dragonfly, ...).

use std::time::Duration;

use redis::AsyncCommands;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use unsworn_core::store::REDIS_RATE_LIMIT;

use super::CachedResponse;

/// How long one store operation may take before it counts as failed.
pub const TIMEOUT: Duration = Duration::from_secs(1);

pub struct RedisStore {
    conn: ConnectionManager,
    prefix: String,
    script: redis::Script,
}

impl RedisStore {
    /// Connects lazily: Unsworn starts even when Redis is down, and the
    /// connection is retried as requests need it.
    pub fn new(url: &str, prefix: &str) -> Result<RedisStore, String> {
        if unsworn_core::project::StoreConfig::is_rest_url(url) {
            return Err(
                "[store].url: the server speaks the Redis protocol, so give it a redis:// or rediss:// URL; https:// (a REST API) is for serverless targets".into(),
            );
        }
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
            script: redis::Script::new(REDIS_RATE_LIMIT),
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
