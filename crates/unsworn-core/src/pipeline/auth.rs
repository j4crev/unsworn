//! Authentication with the host's help: loading JWT keys (fetching JWKS
//! documents) and asking remote API-key verifiers, then the core's
//! synchronous [`authenticate`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, header};
use serde_json::{Map, Value, json};

use super::host::{FetchError, Host};
use super::{Problem, auth_failure};
use crate::auth::{
    CredentialSource, JwtKeys, KeySet, KeyVerifier, Scheme, Verdict, authenticate, token_kid,
};
use crate::policy::{AuthReq, Effective};
use crate::project::Project;
use crate::store::{CachedResponse, ResponseCache, cache_get};

/// JWKS documents are refreshed this often.
const JWKS_REFRESH: Duration = Duration::from_secs(600);
/// A token naming an unknown key id refetches the JWKS at most this often.
const JWKS_UNKNOWN_KID: Duration = Duration::from_secs(60);
/// After a failed load, the next try waits this long.
const JWKS_RETRY: Duration = Duration::from_secs(10);
const JWKS_TIMEOUT: Duration = Duration::from_secs(10);
const JWKS_LIMIT: usize = 1024 * 1024;
/// The most of a verifier's response Unsworn reads.
const VERDICT_LIMIT: usize = 64 * 1024;
const USER_AGENT: &str = concat!("unsworn/", env!("CARGO_PKG_VERSION"));

struct KeyEntry {
    set: Option<Arc<KeySet>>,
    /// When the keys were last loaded, on the host's monotonic clock.
    loaded: Option<Duration>,
}

/// JWT verification keys per scheme. JWKS documents are fetched on first
/// use, refreshed every 10 minutes, and refetched (at most once a minute)
/// when a token names an unknown key id. Outlives a project: hosts keep one
/// for the life of the process, so reloads keep their keys.
pub struct KeyCache {
    /// Held while loading, so concurrent requests wait for one fetch.
    entries: futures_util::lock::Mutex<HashMap<String, KeyEntry>>,
    snapshot: Mutex<HashMap<String, Arc<KeySet>>>,
}

impl Default for KeyCache {
    fn default() -> Self {
        KeyCache {
            entries: futures_util::lock::Mutex::new(HashMap::new()),
            snapshot: Mutex::new(HashMap::new()),
        }
    }
}

impl KeyCache {
    /// The loaded keys for a scheme.
    pub fn get(&self, scheme: &str) -> Option<Arc<KeySet>> {
        self.snapshot.lock().unwrap().get(scheme).cloned()
    }

    /// Makes sure keys for `scheme` are loaded, fetching a JWKS if needed.
    pub async fn ensure<H: Host>(&self, host: &H, scheme: &Scheme, kid: Option<&str>) {
        let Scheme::Jwt(jwt) = scheme else {
            return;
        };
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(jwt.name.clone()).or_insert(KeyEntry {
            set: None,
            loaded: None,
        });
        let now = host.monotonic();
        let since = |t: Duration| now.saturating_sub(t);
        let needs = match (&jwt.keys, &entry.set, entry.loaded) {
            (JwtKeys::Jwks(_), None, None) => true,
            (JwtKeys::Jwks(_), None, Some(t)) => since(t) > JWKS_RETRY,
            (JwtKeys::Jwks(_), Some(set), Some(t)) => {
                since(t) > JWKS_REFRESH
                    || (kid.is_some_and(|k| !set.has_kid(k)) && since(t) > JWKS_UNKNOWN_KID)
            }
            (_, None, _) => true,
            _ => false,
        };
        if !needs {
            return;
        }
        entry.loaded = Some(now);
        let loaded = match &jwt.keys {
            JwtKeys::Secret(s) => KeySet::from_secret(s),
            JwtKeys::PublicKey(pem) => KeySet::from_pem(pem),
            JwtKeys::Jwks(url) => fetch_jwks(host, url)
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

async fn fetch_jwks<H: Host>(host: &H, url: &str) -> Result<String, String> {
    let request = http::Request::get(url)
        .header(header::USER_AGENT, USER_AGENT)
        .body(Bytes::new())
        .map_err(|e| format!("bad URL {url}: {e}"))?;
    let resp = match host.fetch(request, JWKS_TIMEOUT, JWKS_LIMIT).await {
        Ok(r) => r,
        Err(FetchError::Timeout) => return Err(format!("timed out fetching {url}")),
        Err(e) => return Err(format!("can't fetch {url}: {e}")),
    };
    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }
    String::from_utf8(resp.into_body().to_vec()).map_err(|e| e.to_string())
}

/// Asks a remote verifier whether `key` is valid, as an RFC 7662 token
/// introspection request.
async fn verify_key<H: Host>(host: &H, verifier: &KeyVerifier, key: &str) -> Verdict {
    let mut request = http::Request::post(verifier.url.as_str())
        .header(header::CONTENT_TYPE, Verdict::REQUEST_TYPE)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, USER_AGENT);
    for (name, value) in &verifier.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let request = match request.body(Bytes::from(Verdict::request_body(key))) {
        Ok(r) => r,
        Err(e) => return Verdict::Unavailable(format!("can't build the request: {e}")),
    };
    match host.fetch(request, verifier.timeout, VERDICT_LIMIT).await {
        Ok(resp) => Verdict::from_response(resp.status().as_u16(), resp.body()),
        Err(FetchError::Timeout) => Verdict::Unavailable(format!(
            "the verifier didn't answer within {:?}",
            verifier.timeout
        )),
        Err(FetchError::Connect(e)) => {
            Verdict::Unavailable(format!("can't reach the verifier: {e}"))
        }
        Err(FetchError::Read(e)) => {
            Verdict::Unavailable(format!("can't read the verifier's response: {e}"))
        }
    }
}

/// Checks `key` with the verifier, through the store's cache when the
/// scheme sets `cache`. Store errors count as a miss.
async fn check_key<H: Host>(host: &H, verifier: &KeyVerifier, key: &str) -> Verdict {
    let Some(max) = verifier.cache else {
        return verify_key(host, verifier, key).await;
    };
    let store = host.store();
    let cache_key = verifier.cache_key(key);
    if let Some(hit) = cache_get(&store, &cache_key, host.now_ms()).await {
        match Verdict::from_response(hit.status, &hit.body) {
            Verdict::Unavailable(_) => {} // not ours; ask again
            verdict => return verdict,
        }
    }
    let verdict = verify_key(host, verifier, key).await;
    let now_ms = host.now_ms();
    if let (Some(ttl), Some(body)) = (verdict.cache_ttl(max, now_ms / 1000), verdict.to_json()) {
        let entry = CachedResponse {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            stored_ms: now_ms,
            ttl,
        };
        host.background(async move {
            let _ = store.put(&cache_key, entry).await;
        })
        .await;
    }
    verdict
}

/// Credentials from a request's headers and query string.
pub(super) struct Creds<'a> {
    pub headers: &'a HeaderMap,
    pub query: &'a [(String, String)],
}

impl CredentialSource for Creds<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// What authentication adds: the context (`auth`, when a scheme matched)
/// and the verified claims, for policy keys like `auth.sub`.
pub(super) type AuthOutcome = (Map<String, Value>, Option<Map<String, Value>>);

/// Checks a request's credentials against `eff.auth`.
pub(super) async fn authenticate_request<H: Host>(
    host: &H,
    keys: &KeyCache,
    project: &Project,
    headers: &HeaderMap,
    eff: &Effective,
    query: &[(String, String)],
) -> Result<AuthOutcome, Problem> {
    let mut context = Map::new();
    let AuthReq::AnyOf(alts) = &eff.auth else {
        return Ok((context, None));
    };
    let creds = Creds { headers, query };
    let schemes: Vec<Arc<Scheme>> = alts
        .iter()
        .filter_map(|alt| project.schemes.get(&alt.scheme).cloned())
        .collect();
    for scheme in &schemes {
        let kid = match scheme.as_ref() {
            Scheme::Jwt(jwt) => token_kid(jwt, &creds),
            Scheme::ApiKey(_) => None,
        };
        keys.ensure(host, scheme, kid.as_deref()).await;
    }
    // Remote API keys are checked up front, and only when the request
    // carries one, so `authenticate` stays synchronous.
    let mut verdicts = HashMap::new();
    for scheme in &schemes {
        let Scheme::ApiKey(k) = scheme.as_ref() else {
            continue;
        };
        let (Some(verifier), Some(key)) = (k.verifier(), k.credential(&creds)) else {
            continue;
        };
        let verdict = check_key(host, verifier, key).await;
        if let Verdict::Unavailable(reason) = &verdict {
            tracing::error!(scheme = %k.name, "can't verify API key: {reason}");
        }
        verdicts.insert(k.name.clone(), verdict);
    }
    let lookup = |n: &str| keys.get(n);
    let verdict = |n: &str| verdicts.get(n).cloned();
    let outcome = authenticate(
        &eff.auth,
        &project.schemes,
        &lookup,
        &verdict,
        &creds,
        host.now_ms() / 1000,
    );
    match outcome {
        Ok(Some(ok)) => {
            let mut auth = ok.claims.clone();
            auth.insert("scheme".into(), json!(ok.scheme));
            context.insert("auth".into(), Value::Object(auth));
            Ok((context, Some(ok.claims)))
        }
        Ok(None) => Ok((context, None)),
        Err(failure) => Err(auth_failure(&failure, &project.schemes)),
    }
}
