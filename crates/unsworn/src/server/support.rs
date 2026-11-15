//! Shared pieces for the server: bodies, problem responses, metrics, JWT
//! key caching and remote API-key checks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderValue, Response, StatusCode, header};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use serde_json::Value;
use unsworn_core::auth::{JwtKeys, KeySet, KeyVerifier, Scheme, Verdict};
use unsworn_core::pipeline::Problem;
use unsworn_core::validate::FieldError;

use crate::store::{CachedResponse, Store};

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
    problem_response(Problem::new(status, detail))
}

pub fn problem_with(
    status: StatusCode,
    detail: impl Into<String>,
    errors: Option<&[FieldError]>,
) -> Response<Body> {
    problem_response(Problem::new(status, detail).with_errors(errors))
}

pub fn problem_response(problem: Problem) -> Response<Body> {
    let mut resp = Response::new(full(problem.body_bytes()));
    *resp.status_mut() = problem.status;
    *resp.headers_mut() = problem.headers;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(Problem::CONTENT_TYPE),
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

    pub fn render(&self, runtimes: &[(String, bool, usize, usize)], store: &Store) -> String {
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
        let backend = store.backend_name();
        out.push_str("# HELP unsworn_store_up Whether the last store operation succeeded.\n");
        out.push_str("# TYPE unsworn_store_up gauge\n");
        out.push_str(&format!(
            "unsworn_store_up{{backend=\"{backend}\"}} {}\n",
            u8::from(store.is_up())
        ));
        out.push_str("# HELP unsworn_store_errors_total Failed store operations.\n");
        out.push_str("# TYPE unsworn_store_errors_total counter\n");
        out.push_str(&format!(
            "unsworn_store_errors_total{{backend=\"{backend}\"}} {}\n",
            store.error_count()
        ));
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

/// The most of a verifier's response Unsworn reads.
const VERDICT_LIMIT: usize = 64 * 1024;

/// Asks a remote verifier whether `key` is valid.
async fn verify_key(client: &HttpClient, verifier: &KeyVerifier, key: &str) -> Verdict {
    let mut req = http::Request::post(verifier.url.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .header(
            header::USER_AGENT,
            concat!("unsworn/", env!("CARGO_PKG_VERSION")),
        );
    for (name, value) in &verifier.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let req = match req.body(full(Verdict::request_body(key))) {
        Ok(r) => r,
        Err(e) => return Verdict::Unavailable(format!("can't build the request: {e}")),
    };
    let call = async {
        let resp = client
            .request(req)
            .await
            .map_err(|e| format!("can't reach the verifier: {e}"))?;
        let status = resp.status().as_u16();
        let body = http_body_util::Limited::new(resp.into_body(), VERDICT_LIMIT)
            .collect()
            .await
            .map_err(|e| format!("can't read the verifier's response: {e}"))?
            .to_bytes();
        Ok::<_, String>(Verdict::from_response(status, &body))
    };
    match tokio::time::timeout(verifier.timeout, call).await {
        Ok(Ok(verdict)) => verdict,
        Ok(Err(e)) => Verdict::Unavailable(e),
        Err(_) => Verdict::Unavailable(format!(
            "the verifier didn't answer within {:?}",
            verifier.timeout
        )),
    }
}

/// Checks `key` with the verifier, through the `[store]` cache when the
/// scheme sets `cache`. Store errors count as a miss.
pub async fn check_key(
    client: &HttpClient,
    store: &Arc<Store>,
    verifier: &KeyVerifier,
    key: &str,
    now: u64,
) -> Verdict {
    let Some(max) = verifier.cache else {
        return verify_key(client, verifier, key).await;
    };
    let cache_key = verifier.cache_key(key);
    if let Some(hit) = store.cache_get(&cache_key).await {
        match Verdict::from_response(hit.status, &hit.body) {
            Verdict::Unavailable(_) => {} // not ours; ask again
            verdict => return verdict,
        }
    }
    let verdict = verify_key(client, verifier, key).await;
    if let (Some(ttl), Some(body)) = (verdict.cache_ttl(max, now), verdict.to_json()) {
        let entry = CachedResponse {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            stored: std::time::SystemTime::now(),
            ttl,
        };
        // Written in the background, so a slow store never delays the request.
        let store = store.clone();
        tokio::spawn(async move { store.cache_put(cache_key, entry).await });
    }
    verdict
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
