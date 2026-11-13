//! Shared pieces for the server: bodies, problem responses, the memory
//! store, metrics, JWT key caching and client IPs.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use ipnet::IpNet;
use serde_json::{Value, json};
use unsworn_core::auth::{JwtKeys, KeySet, Scheme};
use unsworn_core::validate::FieldError;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = UnsyncBoxBody<Bytes, BoxError>;
pub type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|e| match e {})
        .boxed_unsync()
}

pub fn empty() -> Body {
    Empty::new().map_err(|e| match e {}).boxed_unsync()
}

pub fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    let mut resp = Response::new(full(serde_json::to_vec(value).unwrap_or_default()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// An RFC 9457 problem response.
pub fn problem(status: StatusCode, detail: impl Into<String>) -> Response<Body> {
    problem_with(status, detail, None)
}

pub fn problem_with(
    status: StatusCode,
    detail: impl Into<String>,
    errors: Option<&[FieldError]>,
) -> Response<Body> {
    let mut body = json!({
        "type": "about:blank",
        "title": status.canonical_reason().unwrap_or("Error"),
        "status": status.as_u16(),
        "detail": detail.into(),
    });
    if let Some(errors) = errors {
        body["errors"] = json!(errors);
    }
    let mut resp = json_response(status, &body);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    resp
}

pub fn build_client() -> HttpClient {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .build(https)
}

/// Works out the client's IP, trusting `X-Forwarded-For` only from
/// configured proxies.
pub struct ClientIp {
    trust_all: bool,
    nets: Vec<IpNet>,
}

impl ClientIp {
    pub fn new(trusted: &[String]) -> Result<ClientIp, String> {
        let mut nets = Vec::new();
        let mut trust_all = false;
        for t in trusted {
            if t == "all" {
                trust_all = true;
            } else if let Ok(net) = t.parse::<IpNet>() {
                nets.push(net);
            } else if let Ok(ip) = t.parse::<IpAddr>() {
                nets.push(IpNet::from(ip));
            } else {
                return Err(format!(
                    "[server].trusted_proxies: \"{t}\" isn't an IP or CIDR"
                ));
            }
        }
        Ok(ClientIp { trust_all, nets })
    }

    fn trusted(&self, ip: IpAddr) -> bool {
        self.trust_all || self.nets.iter().any(|n| n.contains(&ip))
    }

    pub fn resolve(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        let mut ip = peer.ip();
        if !self.trusted(ip) {
            return ip;
        }
        let forwarded: Vec<IpAddr> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        // Walk from the closest hop back, stopping at the first untrusted one.
        for hop in forwarded.iter().rev() {
            ip = *hop;
            if !self.trusted(*hop) {
                break;
            }
        }
        ip
    }
}

/// A cached response.
#[derive(Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, HeaderValue)>,
    pub body: Bytes,
    pub stored: Instant,
    pub ttl: Duration,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// In-memory rate limits and response cache.
#[derive(Default)]
pub struct MemoryStore {
    buckets: Mutex<HashMap<String, Bucket>>,
    cache: Mutex<HashMap<String, CachedResponse>>,
}

const MAX_CACHE_ENTRIES: usize = 10_000;
pub const MAX_CACHED_BODY: usize = 1024 * 1024;

impl MemoryStore {
    /// Token bucket: `requests` per `per`. Returns `Err(retry_after)` when
    /// the limit is hit, else the remaining requests.
    pub fn rate_limit(&self, key: &str, requests: u32, per: Duration) -> Result<u32, Duration> {
        let now = Instant::now();
        let rate = requests as f64 / per.as_secs_f64();
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: requests as f64,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(requests as f64);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(bucket.tokens as u32)
        } else {
            let wait = (1.0 - bucket.tokens) / rate;
            Err(Duration::from_secs_f64(wait.max(0.001)))
        }
    }

    pub fn cache_get(&self, key: &str) -> Option<CachedResponse> {
        let cache = self.cache.lock().unwrap();
        let entry = cache.get(key)?;
        (entry.stored.elapsed() < entry.ttl).then(|| entry.clone())
    }

    pub fn cache_put(&self, key: String, value: CachedResponse) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, v| v.stored.elapsed() < v.ttl);
            if cache.len() >= MAX_CACHE_ENTRIES
                && let Some(k) = cache.keys().next().cloned()
            {
                cache.remove(&k);
            }
        }
        cache.insert(key, value);
    }

    /// Drops expired entries. Called periodically.
    pub fn sweep(&self) {
        self.cache
            .lock()
            .unwrap()
            .retain(|_, v| v.stored.elapsed() < v.ttl);
        let idle = Duration::from_secs(3600);
        self.buckets
            .lock()
            .unwrap()
            .retain(|_, b| b.last.elapsed() < idle);
    }
}

/// Request counts and durations, exposed in Prometheus format.
#[derive(Default)]
pub struct Metrics {
    requests: Mutex<HashMap<(String, String, u16), (u64, f64)>>,
}

impl Metrics {
    pub fn record(&self, method: &str, route: &str, status: u16, elapsed: Duration) {
        let mut m = self.requests.lock().unwrap();
        let entry = m
            .entry((method.to_string(), route.to_string(), status))
            .or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += elapsed.as_secs_f64();
    }

    pub fn render(&self, runtimes: &[(String, bool, usize, usize)]) -> String {
        let mut out = String::new();
        out.push_str("# HELP unsworn_requests_total Requests handled.\n");
        out.push_str("# TYPE unsworn_requests_total counter\n");
        let m = self.requests.lock().unwrap();
        let mut rows: Vec<_> = m.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        for ((method, route, status), (count, _)) in &rows {
            out.push_str(&format!(
                "unsworn_requests_total{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {count}\n",
                escape(route)
            ));
        }
        out.push_str("# HELP unsworn_request_duration_seconds_sum Total time spent on requests.\n");
        out.push_str("# TYPE unsworn_request_duration_seconds_sum counter\n");
        for ((method, route, status), (_, secs)) in &rows {
            out.push_str(&format!(
                "unsworn_request_duration_seconds_sum{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {secs:.6}\n",
                escape(route)
            ));
        }
        out.push_str("# HELP unsworn_runtime_workers Healthy workers per runtime.\n");
        out.push_str("# TYPE unsworn_runtime_workers gauge\n");
        for (name, _, alive, _) in runtimes {
            out.push_str(&format!(
                "unsworn_runtime_workers{{runtime=\"{name}\"}} {alive}\n"
            ));
        }
        out.push_str("# HELP unsworn_runtime_crashes_recent Worker exits in the last minute.\n");
        out.push_str("# TYPE unsworn_runtime_crashes_recent gauge\n");
        for (name, _, _, crashes) in runtimes {
            out.push_str(&format!(
                "unsworn_runtime_crashes_recent{{runtime=\"{name}\"}} {crashes}\n"
            ));
        }
        out
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

struct KeyEntry {
    set: Option<Arc<KeySet>>,
    fetched: Option<Instant>,
}

/// JWT verification keys per scheme. JWKS documents are fetched on first
/// use, refreshed every 10 minutes, and refetched (at most once a minute)
/// when a token names an unknown key id.
pub struct KeyCache {
    entries: tokio::sync::Mutex<HashMap<String, KeyEntry>>,
    snapshot: Mutex<HashMap<String, Arc<KeySet>>>,
}

impl KeyCache {
    pub fn new() -> KeyCache {
        KeyCache {
            entries: tokio::sync::Mutex::new(HashMap::new()),
            snapshot: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, scheme: &str) -> Option<Arc<KeySet>> {
        self.snapshot.lock().unwrap().get(scheme).cloned()
    }

    /// Makes sure keys for `scheme` are loaded, fetching a JWKS if needed.
    pub async fn ensure(&self, client: HttpClient, scheme: Arc<Scheme>, kid: Option<String>) {
        let Scheme::Jwt(jwt) = scheme.as_ref() else {
            return;
        };
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(jwt.name.clone()).or_insert(KeyEntry {
            set: None,
            fetched: None,
        });
        let needs = match (&jwt.keys, &entry.set, entry.fetched) {
            (JwtKeys::Jwks(_), None, None) => true,
            (JwtKeys::Jwks(_), None, Some(t)) => t.elapsed() > Duration::from_secs(10),
            (JwtKeys::Jwks(_), Some(set), Some(t)) => {
                t.elapsed() > Duration::from_secs(600)
                    || (kid.as_deref().is_some_and(|k| !set.has_kid(k))
                        && t.elapsed() > Duration::from_secs(60))
            }
            (_, None, _) => true,
            _ => false,
        };
        if !needs {
            return;
        }
        entry.fetched = Some(Instant::now());
        let loaded = match &jwt.keys {
            JwtKeys::Secret(s) => KeySet::from_secret(s),
            JwtKeys::PublicKey(pem) => KeySet::from_pem(pem),
            JwtKeys::Jwks(url) => fetch_text(&client, url)
                .await
                .and_then(|text| KeySet::from_jwks(&text)),
        };
        match loaded {
            Ok(set) => {
                let set = Arc::new(set);
                entry.set = Some(set.clone());
                self.snapshot.lock().unwrap().insert(jwt.name.clone(), set);
            }
            Err(e) => tracing::error!(scheme = %jwt.name, "can't load signing keys: {e}"),
        }
    }
}

async fn fetch_text(client: &HttpClient, url: &str) -> Result<String, String> {
    let uri: http::Uri = url.parse().map_err(|e| format!("bad URL {url}: {e}"))?;
    let req = http::Request::get(uri)
        .header(
            header::USER_AGENT,
            concat!("unsworn/", env!("CARGO_PKG_VERSION")),
        )
        .body(empty())
        .map_err(|e| e.to_string())?;
    let resp = tokio::time::timeout(Duration::from_secs(10), client.request(req))
        .await
        .map_err(|_| format!("timed out fetching {url}"))?
        .map_err(|e| format!("can't fetch {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    String::from_utf8(body.to_vec()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_bucket() {
        let store = MemoryStore::default();
        for _ in 0..3 {
            assert!(store.rate_limit("k", 3, Duration::from_secs(60)).is_ok());
        }
        let wait = store
            .rate_limit("k", 3, Duration::from_secs(60))
            .unwrap_err();
        assert!(wait > Duration::from_secs(10));
        assert!(
            store
                .rate_limit("other", 3, Duration::from_secs(60))
                .is_ok()
        );
    }

    #[test]
    fn client_ip_trust() {
        let peer: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.7".parse().unwrap());
        let none = ClientIp::new(&[]).unwrap();
        assert_eq!(none.resolve(peer, &headers), peer.ip());
        let trusted = ClientIp::new(&["10.0.0.0/8".into()]).unwrap();
        assert_eq!(
            trusted.resolve(peer, &headers),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
    }
}
