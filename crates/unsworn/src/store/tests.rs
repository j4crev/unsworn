//! One suite, run against every backend. The Redis runs need a server:
//! set `UNSWORN_TEST_REDIS_URL`, e.g. `redis://127.0.0.1:6379`.

use bytes::Bytes;

use super::*;

fn config(backend: StoreBackend, url: Option<String>, on_error: OnError) -> StoreConfig {
    StoreConfig {
        backend,
        url,
        // A fresh namespace per run, so runs never see each other's keys.
        prefix: format!(
            "test-{}-{}:",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ),
        on_error,
    }
}

fn redis_url() -> Option<String> {
    let url = std::env::var("UNSWORN_TEST_REDIS_URL").ok();
    if url.is_none() {
        eprintln!("skipping: set UNSWORN_TEST_REDIS_URL to run the Redis store tests");
    }
    url
}

fn response(body: &'static [u8], ttl: Duration) -> CachedResponse {
    CachedResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/plain".into())],
        body: Bytes::from_static(body),
        stored_ms: now_ms(),
        ttl,
    }
}

async fn suite(store: &Store) {
    // Rate limits: a burst of `requests`, then a wait of about 1/rate.
    for _ in 0..3 {
        assert_eq!(
            store.rate_limit("k", 3, Duration::from_secs(60)).await,
            RateLimit::Allowed
        );
    }
    match store.rate_limit("k", 3, Duration::from_secs(60)).await {
        RateLimit::Limited { retry_after } => {
            assert!(
                retry_after > Duration::from_secs(15) && retry_after <= Duration::from_secs(20),
                "{retry_after:?}"
            );
        }
        other => panic!("expected a limit, got {other:?}"),
    }
    assert_eq!(
        store.rate_limit("other", 3, Duration::from_secs(60)).await,
        RateLimit::Allowed,
        "keys are separate"
    );
    // A fast refill: 1 per 100ms.
    assert_eq!(
        store
            .rate_limit("fast", 1, Duration::from_millis(100))
            .await,
        RateLimit::Allowed
    );
    assert!(matches!(
        store
            .rate_limit("fast", 1, Duration::from_millis(100))
            .await,
        RateLimit::Limited { .. }
    ));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        store
            .rate_limit("fast", 1, Duration::from_millis(100))
            .await,
        RateLimit::Allowed
    );

    // Cache: hit, miss and expiry.
    assert!(store.cache_get("page").await.is_none());
    store
        .cache_put("page".into(), response(b"hello", Duration::from_secs(30)))
        .await;
    let hit = store.cache_get("page").await.expect("a cache hit");
    assert_eq!(hit.body, Bytes::from_static(b"hello"));
    assert_eq!(hit.headers[0].0, "content-type");
    assert!(hit.age(now_ms()) < Duration::from_secs(5));
    store
        .cache_put("short".into(), response(b"x", Duration::from_millis(100)))
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(store.cache_get("short").await.is_none(), "expired");

    assert!(store.is_up());
    assert_eq!(store.error_count(), 0);
}

#[tokio::test]
async fn memory_store() {
    let store = Store::new(&config(StoreBackend::Memory, None, OnError::Open)).unwrap();
    suite(&store).await;
}

#[tokio::test]
async fn redis_store() {
    let Some(url) = redis_url() else { return };
    let store = Store::new(&config(StoreBackend::Redis, Some(url), OnError::Open)).unwrap();
    suite(&store).await;
}

#[tokio::test]
async fn redis_instances_share_state() {
    let Some(url) = redis_url() else { return };
    let cfg = config(StoreBackend::Redis, Some(url), OnError::Open);
    let a = Store::new(&cfg).unwrap();
    let b = Store::new(&cfg).unwrap();
    assert_eq!(
        a.rate_limit("k", 1, Duration::from_secs(60)).await,
        RateLimit::Allowed
    );
    assert!(matches!(
        b.rate_limit("k", 1, Duration::from_secs(60)).await,
        RateLimit::Limited { .. }
    ));
    a.cache_put("page".into(), response(b"shared", Duration::from_secs(30)))
        .await;
    assert_eq!(b.cache_get("page").await.unwrap().body, &b"shared"[..]);

    // A different prefix is a different namespace.
    let other = Store::new(&StoreConfig {
        prefix: format!("{}other:", cfg.prefix),
        ..cfg
    })
    .unwrap();
    assert_eq!(
        other.rate_limit("k", 1, Duration::from_secs(60)).await,
        RateLimit::Allowed
    );
    assert!(other.cache_get("page").await.is_none());
}

/// Nothing listens on port 1, so every operation fails.
fn unreachable(on_error: OnError) -> Store {
    let url = Some("redis://127.0.0.1:1".to_string());
    Store::new(&config(StoreBackend::Redis, url, on_error)).unwrap()
}

#[tokio::test]
async fn unreachable_store_fails_open_by_default() {
    let store = unreachable(OnError::Open);
    assert_eq!(
        store.rate_limit("k", 1, Duration::from_secs(60)).await,
        RateLimit::Allowed
    );
    assert!(store.cache_get("page").await.is_none());
    store
        .cache_put("page".into(), response(b"x", Duration::from_secs(30)))
        .await;
    assert!(!store.is_up());
    assert_eq!(store.error_count(), 3);
}

#[tokio::test]
async fn unreachable_store_can_fail_closed() {
    let store = unreachable(OnError::Closed);
    assert_eq!(
        store.rate_limit("k", 1, Duration::from_secs(60)).await,
        RateLimit::Unavailable
    );
    // The cache never fails a request.
    assert!(store.cache_get("page").await.is_none());
}
