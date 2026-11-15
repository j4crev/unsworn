//! Credential verification: JWT and API keys.
//!
//! Pure: keys, remote verdicts and the current time are passed in. The host
//! fetches JWKS documents and caches the resulting [`KeySet`]s, and asks
//! remote API-key verifiers about keys, turning their answers into
//! [`Verdict`]s with [`Verdict::from_response`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::policy::{AuthAlt, AuthReq};

#[derive(Debug, Clone, Serialize)]
pub struct ProtectedResource {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub scopes_supported: Vec<String>,
    pub resource_name: Option<String>,
}

impl ProtectedResource {
    /// The RFC 9728 metadata document.
    pub fn metadata(&self) -> Value {
        let mut doc = json!({
            "resource": self.resource,
            "authorization_servers": self.authorization_servers,
            "bearer_methods_supported": ["header"],
        });
        if !self.scopes_supported.is_empty() {
            doc["scopes_supported"] = json!(self.scopes_supported);
        }
        if let Some(name) = &self.resource_name {
            doc["resource_name"] = json!(name);
        }
        doc
    }

    /// Paths where the metadata is served: the root form and the form with
    /// the resource's path appended.
    pub fn well_known_paths(&self) -> Vec<String> {
        let mut paths = vec![WELL_KNOWN.to_string()];
        let (_, path) = self.origin_and_path();
        if !path.is_empty() {
            paths.push(format!("{WELL_KNOWN}{path}"));
        }
        paths
    }

    /// The metadata URL to put in `WWW-Authenticate`.
    pub fn metadata_url(&self) -> String {
        let (origin, path) = self.origin_and_path();
        format!("{origin}{WELL_KNOWN}{path}")
    }

    /// The resource URL as (`https://host:port`, `/path`), the path without
    /// a trailing slash.
    fn origin_and_path(&self) -> (String, String) {
        let uri = self.resource.parse::<http::Uri>().ok();
        let parts = uri
            .as_ref()
            .and_then(|uri| Some((uri.scheme_str()?, uri.authority()?, uri.path())));
        match parts {
            Some((scheme, authority, path)) => (
                format!("{scheme}://{authority}"),
                path.trim_end_matches('/').to_string(),
            ),
            None => (
                self.resource.trim_end_matches('/').to_string(),
                String::new(),
            ),
        }
    }
}

/// Where RFC 9728 puts protected-resource metadata.
const WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";

#[derive(Debug, Clone)]
pub enum JwtKeys {
    Jwks(String),
    Secret(String),
    /// PEM contents of a public key file.
    PublicKey(String),
}

#[derive(Debug, Clone)]
pub struct JwtScheme {
    pub name: String,
    /// Lowercase header name. `authorization` expects `Bearer <token>`.
    pub header: String,
    pub issuer: Option<String>,
    pub audience: Vec<String>,
    pub algorithms: Vec<Algorithm>,
    pub leeway: u64,
    pub keys: JwtKeys,
    pub protected_resource: Option<ProtectedResource>,
}

#[derive(Debug, Clone)]
pub struct ApiKeyScheme {
    pub name: String,
    pub header: Option<String>,
    pub query: Option<String>,
    pub keys: ApiKeys,
}

/// Where an API key scheme's valid keys come from.
#[derive(Debug, Clone)]
pub enum ApiKeys {
    /// Listed in the config.
    Static(Vec<String>),
    /// Checked per request by an external service.
    Remote(KeyVerifier),
}

/// An external API-key verification endpoint, speaking OAuth 2.0 token
/// introspection (RFC 7662).
///
/// Unsworn sends `POST <url>` with the form field `token=<key>` and expects
/// `200` with a JSON object: `{"active": true, ...}`, whose other members
/// are the key's claims, or `{"active": false}`.
#[derive(Debug, Clone)]
pub struct KeyVerifier {
    pub url: String,
    /// Extra headers on the verification request, such as the service's own
    /// credentials. Names are lowercase.
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    /// How long answers are remembered; `None` asks on every request.
    pub cache: Option<Duration>,
}

impl KeyVerifier {
    /// Where the verdict for `key` is cached. The key is hashed, so the
    /// store never holds it, and the verifier URL is included, so changing
    /// it starts afresh.
    pub fn cache_key(&self, key: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(self.url.as_bytes());
        hash.update([0]);
        hash.update(key.as_bytes());
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        format!("auth|{hex}")
    }
}

/// The key id (`kid`) in the header of the token a JWT scheme would read,
/// used to notice a rotated JWKS. `None` when the request has no token for
/// the scheme or the token's header can't be read.
pub fn token_kid(scheme: &JwtScheme, source: &dyn CredentialSource) -> Option<String> {
    let token = scheme.token(source.header(&scheme.header)?)?;
    jsonwebtoken::decode_header(token).ok()?.kid
}

impl JwtScheme {
    /// Where the scheme's verification keys are cached: its name and a hash
    /// of where they come from. A reload that changes the secret, the key
    /// file or the JWKS URL starts afresh, so a rotated key takes effect at
    /// once, and the old one stops being accepted.
    pub fn cache_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let (kind, material) = match &self.keys {
            JwtKeys::Jwks(url) => ("jwks", url),
            JwtKeys::Secret(secret) => ("secret", secret),
            JwtKeys::PublicKey(pem) => ("pem", pem),
        };
        let mut hash = Sha256::new();
        hash.update(kind.as_bytes());
        hash.update([0]);
        hash.update(material.as_bytes());
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        format!("{}|{hex}", self.name)
    }

    /// The token in the value of the scheme's header: after `Bearer` in
    /// `Authorization`, the whole value of any other header.
    fn token<'a>(&self, raw: &'a str) -> Option<&'a str> {
        if self.header != "authorization" {
            return Some(raw.trim());
        }
        match raw.split_once(' ') {
            Some((kind, token)) if kind.eq_ignore_ascii_case("bearer") => Some(token.trim()),
            _ => None,
        }
    }
}

impl ApiKeyScheme {
    /// The key the request carries, from the header or the query string.
    pub fn credential<'a>(&self, source: &'a dyn CredentialSource) -> Option<&'a str> {
        self.header
            .as_deref()
            .and_then(|h| source.header(h))
            .or_else(|| self.query.as_deref().and_then(|q| source.query(q)))
            .map(str::trim)
    }

    pub fn verifier(&self) -> Option<&KeyVerifier> {
        match &self.keys {
            ApiKeys::Remote(v) => Some(v),
            ApiKeys::Static(_) => None,
        }
    }
}

/// A remote verifier's answer about one key.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The key is valid; these claims become `ctx.auth`.
    Active(Map<String, Value>),
    /// The key isn't valid: `401`.
    Inactive,
    /// The verifier couldn't answer (down, timed out, or broke the
    /// contract): `503`.
    Unavailable(String),
}

impl Verdict {
    /// The media type of [`Verdict::request_body`].
    pub const REQUEST_TYPE: &'static str = "application/x-www-form-urlencoded";

    /// The body of an introspection request (RFC 7662 section 2.1).
    pub fn request_body(key: &str) -> Vec<u8> {
        form_urlencoded::Serializer::new(String::new())
            .append_pair("token", key)
            .finish()
            .into_bytes()
    }

    /// The verdict as an introspection response, for caching. `None` for
    /// [`Verdict::Unavailable`], which is never cached.
    pub fn to_json(&self) -> Option<Vec<u8>> {
        let doc = match self {
            Verdict::Active(claims) => {
                let mut doc = claims.clone();
                doc.insert("active".into(), json!(true));
                Value::Object(doc)
            }
            Verdict::Inactive => json!({ "active": false }),
            Verdict::Unavailable(_) => return None,
        };
        serde_json::to_vec(&doc).ok()
    }

    /// How long to cache this verdict, given the scheme's `cache` and the
    /// current time: no longer than an `exp` claim allows, and not at all
    /// once it has passed.
    pub fn cache_ttl(&self, max: Duration, now: u64) -> Option<Duration> {
        match self {
            Verdict::Active(claims) => match claims
                .get("exp")
                .and_then(Value::as_f64)
                .map(|exp| exp.max(0.0) as u64)
            {
                Some(exp) if exp <= now => None,
                Some(exp) => Some(max.min(Duration::from_secs(exp - now))),
                None => Some(max),
            },
            Verdict::Inactive => Some(max),
            Verdict::Unavailable(_) => None,
        }
    }

    /// Reads an introspection response (RFC 7662 section 2.2): a JSON
    /// object with a boolean `active`, whose other members are the claims.
    /// Anything else is a broken verifier, not a bad key, so misconfigured
    /// URLs (and credentials the verifier rejects with `401`) show up as
    /// `503`s rather than as `401`s.
    pub fn from_response(status: u16, body: &[u8]) -> Verdict {
        if status != 200 {
            return Verdict::Unavailable(format!("verifier returned {status}"));
        }
        let Ok(Value::Object(mut doc)) = serde_json::from_slice::<Value>(body) else {
            return Verdict::Unavailable("verifier didn't return a JSON object".into());
        };
        match doc.remove("active") {
            Some(Value::Bool(true)) => Verdict::Active(doc),
            Some(Value::Bool(false)) => Verdict::Inactive,
            _ => Verdict::Unavailable("verifier's response has no boolean `active`".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Scheme {
    Jwt(JwtScheme),
    ApiKey(ApiKeyScheme),
}

impl Scheme {
    pub fn name(&self) -> &str {
        match self {
            Scheme::Jwt(s) => &s.name,
            Scheme::ApiKey(s) => &s.name,
        }
    }
}

/// Verification keys for one JWT scheme.
pub struct KeySet {
    keys: Vec<(Option<String>, DecodingKey, Option<Algorithm>)>,
}

impl std::fmt::Debug for KeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeySet({} keys)", self.keys.len())
    }
}

impl KeySet {
    pub fn from_jwks(json: &str) -> Result<KeySet, String> {
        let set: JwkSet = serde_json::from_str(json).map_err(|e| format!("invalid JWKS: {e}"))?;
        let mut keys = Vec::new();
        for jwk in &set.keys {
            let alg = jwk
                .common
                .key_algorithm
                .and_then(|a| a.to_string().parse::<Algorithm>().ok());
            match DecodingKey::from_jwk(jwk) {
                Ok(key) => keys.push((jwk.common.key_id.clone(), key, alg)),
                Err(_) => continue, // unsupported key types are skipped
            }
        }
        if keys.is_empty() {
            return Err("the JWKS has no usable keys".into());
        }
        Ok(KeySet { keys })
    }

    pub fn from_secret(secret: &str) -> Result<KeySet, String> {
        let key = DecodingKey::from_secret(secret.as_bytes())
            .map_err(|e| format!("invalid secret: {e}"))?;
        Ok(KeySet {
            keys: vec![(None, key, None)],
        })
    }

    pub fn from_pem(pem: &str) -> Result<KeySet, String> {
        let bytes = pem.as_bytes();
        let key = DecodingKey::from_rsa_pem(bytes)
            .or_else(|_| DecodingKey::from_ec_pem(bytes))
            .or_else(|_| DecodingKey::from_ed_pem(bytes))
            .map_err(|e| format!("unsupported public key: {e}"))?;
        Ok(KeySet {
            keys: vec![(None, key, None)],
        })
    }

    pub fn has_kid(&self, kid: &str) -> bool {
        self.keys.iter().any(|(k, _, _)| k.as_deref() == Some(kid))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    /// No credentials, or invalid ones: `401`.
    Unauthorized {
        scheme: Option<String>,
        reason: String,
    },
    /// Valid credentials without the required scopes: `403`.
    InsufficientScope { scheme: String, scopes: Vec<String> },
    /// The credential couldn't be checked, because its verifier is down or
    /// misbehaving: `503`.
    Unavailable { scheme: String, reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthSuccess {
    pub scheme: String,
    pub claims: Map<String, Value>,
}

/// What a credential check needs from the request.
pub trait CredentialSource {
    fn header(&self, name: &str) -> Option<&str>;
    fn query(&self, name: &str) -> Option<&str>;
}

/// Checks a request against an [`AuthReq`].
///
/// `keys` gives a JWT scheme's verification keys; `verdicts` gives a remote
/// API key scheme's verdict on the key this request carries.
pub fn authenticate(
    req: &AuthReq,
    schemes: &BTreeMap<String, Arc<Scheme>>,
    keys: &dyn Fn(&str) -> Option<Arc<KeySet>>,
    verdicts: &dyn Fn(&str) -> Option<Verdict>,
    source: &dyn CredentialSource,
    now: u64,
) -> Result<Option<AuthSuccess>, AuthFailure> {
    let alts = match req {
        AuthReq::Public => return Ok(None),
        AuthReq::AnyOf(alts) => alts,
    };
    let mut first_failure: Option<AuthFailure> = None;
    let mut scope_failure: Option<AuthFailure> = None;
    let mut unavailable: Option<AuthFailure> = None;
    for AuthAlt { scheme, scopes } in alts {
        let Some(s) = schemes.get(scheme) else {
            continue;
        };
        let result = match s.as_ref() {
            Scheme::Jwt(jwt) => check_jwt(jwt, keys, source, now),
            Scheme::ApiKey(k) => match check_api_key(k, verdicts, source) {
                Err(KeyError::Unavailable(reason)) => {
                    unavailable.get_or_insert(AuthFailure::Unavailable {
                        scheme: scheme.clone(),
                        reason,
                    });
                    continue;
                }
                Err(KeyError::Invalid(reason)) => Err(reason),
                Ok(claims) => Ok(claims),
            },
        };
        match result {
            Ok(Some(claims)) => {
                let missing: Vec<String> = scopes
                    .iter()
                    .filter(|s| !has_scope(&claims, s))
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    return Ok(Some(AuthSuccess {
                        scheme: scheme.clone(),
                        claims,
                    }));
                }
                scope_failure.get_or_insert(AuthFailure::InsufficientScope {
                    scheme: scheme.clone(),
                    scopes: scopes.clone(),
                });
            }
            Ok(None) => {}
            Err(reason) => {
                first_failure.get_or_insert(AuthFailure::Unauthorized {
                    scheme: Some(scheme.clone()),
                    reason,
                });
            }
        }
    }
    // A verifier outage outranks an invalid credential: the key it couldn't
    // check may well have been valid.
    Err(scope_failure
        .or(unavailable)
        .or(first_failure)
        .unwrap_or_else(|| AuthFailure::Unauthorized {
            scheme: alts.first().map(|a| a.scheme.clone()),
            reason: "missing credentials".into(),
        }))
}

fn check_jwt(
    scheme: &JwtScheme,
    keys: &dyn Fn(&str) -> Option<Arc<KeySet>>,
    source: &dyn CredentialSource,
    now: u64,
) -> Result<Option<Map<String, Value>>, String> {
    let Some(raw) = source.header(&scheme.header) else {
        return Ok(None);
    };
    let Some(token) = scheme.token(raw) else {
        return Ok(None);
    };
    let header = jsonwebtoken::decode_header(token).map_err(|e| format!("malformed token: {e}"))?;
    if !scheme.algorithms.contains(&header.alg) {
        return Err(format!("algorithm {:?} isn't allowed", header.alg));
    }
    let set = keys(&scheme.name).ok_or("signing keys aren't available yet")?;

    let mut validation = Validation::new(header.alg);
    validation.algorithms = scheme.algorithms.clone();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();

    let candidates = set.keys.iter().filter(|(kid, _, alg)| {
        let kid_ok = match (&header.kid, kid) {
            (Some(want), Some(have)) => want == have,
            _ => true,
        };
        kid_ok && alg.is_none_or(|a| a == header.alg)
    });
    let mut last_error = String::from("no key matches the token");
    let mut claims = None;
    for (_, key, _) in candidates {
        match jsonwebtoken::decode::<Map<String, Value>>(token, key, &validation) {
            Ok(data) => {
                claims = Some(data.claims);
                break;
            }
            Err(e) => last_error = format!("invalid token: {e}"),
        }
    }
    let claims = claims.ok_or(last_error)?;

    let (now, leeway) = (now as f64, scheme.leeway as f64);
    // A token must say when it expires: one that doesn't is valid forever
    // once it leaks.
    let Some(exp) = numeric_date(&claims, "exp")? else {
        return Err("token has no `exp` claim".into());
    };
    if now >= exp + leeway {
        return Err("token has expired".into());
    }
    if let Some(nbf) = numeric_date(&claims, "nbf")?
        && now + leeway < nbf
    {
        return Err("token isn't valid yet".into());
    }
    if let Some(iss) = &scheme.issuer
        && claims.get("iss").and_then(Value::as_str) != Some(iss)
    {
        return Err("token has the wrong issuer".into());
    }
    if !scheme.audience.is_empty() {
        let ok = match claims.get("aud") {
            Some(Value::String(a)) => scheme.audience.contains(a),
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .any(|a| scheme.audience.iter().any(|want| want == a)),
            _ => false,
        };
        if !ok {
            return Err("token wasn't issued for this audience".into());
        }
    }
    Ok(Some(claims))
}

/// A NumericDate claim (RFC 7519 section 2): seconds since the Unix epoch,
/// which may have a fraction. A claim that's present but isn't a number is
/// an error, so a token can't dodge a check by sending `"exp": "never"`.
fn numeric_date(claims: &Map<String, Value>, name: &str) -> Result<Option<f64>, String> {
    match claims.get(name) {
        None => Ok(None),
        Some(v) => match v.as_f64().filter(|n| n.is_finite()) {
            Some(n) => Ok(Some(n)),
            None => Err(format!("token's `{name}` isn't a number")),
        },
    }
}

enum KeyError {
    Invalid(String),
    Unavailable(String),
}

fn check_api_key(
    scheme: &ApiKeyScheme,
    verdicts: &dyn Fn(&str) -> Option<Verdict>,
    source: &dyn CredentialSource,
) -> Result<Option<Map<String, Value>>, KeyError> {
    let Some(provided) = scheme.credential(source) else {
        return Ok(None);
    };
    let keys = match &scheme.keys {
        ApiKeys::Static(keys) => keys,
        ApiKeys::Remote(_) => {
            return match verdicts(&scheme.name) {
                Some(Verdict::Active(claims)) => Ok(Some(claims)),
                Some(Verdict::Inactive) => Err(KeyError::Invalid("invalid API key".into())),
                Some(Verdict::Unavailable(reason)) => Err(KeyError::Unavailable(reason)),
                None => Err(KeyError::Unavailable("the key wasn't verified".into())),
            };
        }
    };
    let matched = keys
        .iter()
        .position(|k| constant_time_eq(k.as_bytes(), provided.as_bytes()));
    match matched {
        Some(index) => {
            let mut claims = Map::new();
            claims.insert("type".into(), json!("api_key"));
            claims.insert("key".into(), json!(index));
            Ok(Some(claims))
        }
        None => Err(KeyError::Invalid("invalid API key".into())),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn has_scope(claims: &Map<String, Value>, scope: &str) -> bool {
    let check = |v: &Value| match v {
        Value::String(s) => s.split_whitespace().any(|s| s == scope),
        Value::Array(list) => list.iter().any(|s| s.as_str() == Some(scope)),
        _ => false,
    };
    claims.get("scope").is_some_and(check) || claims.get("scp").is_some_and(check)
}

/// The challenge an API key scheme answers with. No registered HTTP
/// authentication scheme covers API keys, but a `401` must carry a
/// challenge (RFC 9110 section 15.5.2), and clients skip ones they don't
/// know.
const API_KEY_CHALLENGE: &str = "ApiKey";

/// The `WWW-Authenticate` header for a failure.
pub fn www_authenticate(
    failure: &AuthFailure,
    schemes: &BTreeMap<String, Arc<Scheme>>,
) -> Option<String> {
    let scheme_name = match failure {
        AuthFailure::Unauthorized { scheme, .. } => scheme.as_deref()?,
        AuthFailure::InsufficientScope { scheme, .. } => scheme,
        AuthFailure::Unavailable { .. } => return None,
    };
    let jwt = match schemes.get(scheme_name)?.as_ref() {
        Scheme::Jwt(jwt) => jwt,
        Scheme::ApiKey(_) => return Some(API_KEY_CHALLENGE.into()),
    };
    let mut parts = Vec::new();
    match failure {
        AuthFailure::Unauthorized { reason, .. } => {
            if reason != "missing credentials" {
                parts.push("error=\"invalid_token\"".to_string());
            }
        }
        AuthFailure::InsufficientScope { scopes, .. } => {
            parts.push("error=\"insufficient_scope\"".to_string());
            parts.push(format!("scope=\"{}\"", scopes.join(" ")));
        }
        AuthFailure::Unavailable { .. } => {}
    }
    if let Some(pr) = &jwt.protected_resource {
        parts.push(format!("resource_metadata=\"{}\"", pr.metadata_url()));
    }
    if parts.is_empty() {
        Some("Bearer".into())
    } else {
        Some(format!("Bearer {}", parts.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};

    struct Req(Vec<(&'static str, String)>);
    impl CredentialSource for Req {
        fn header(&self, name: &str) -> Option<&str> {
            self.0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.as_str())
        }
        fn query(&self, _: &str) -> Option<&str> {
            None
        }
    }

    fn jwt_scheme() -> Arc<Scheme> {
        Arc::new(Scheme::Jwt(JwtScheme {
            name: "jwt".into(),
            header: "authorization".into(),
            issuer: Some("https://auth.example.com/".into()),
            audience: vec!["api".into()],
            algorithms: vec![Algorithm::HS256],
            leeway: 0,
            keys: JwtKeys::Secret("secret".into()),
            protected_resource: Some(ProtectedResource {
                resource: "https://api.example.com/mcp".into(),
                authorization_servers: vec!["https://auth.example.com/".into()],
                scopes_supported: vec![],
                resource_name: None,
            }),
        }))
    }

    fn token(claims: Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"secret").unwrap(),
        )
        .unwrap()
    }

    fn run(
        req: &AuthReq,
        headers: Vec<(&'static str, String)>,
    ) -> Result<Option<AuthSuccess>, AuthFailure> {
        let mut schemes = BTreeMap::new();
        schemes.insert("jwt".to_string(), jwt_scheme());
        schemes.insert(
            "key".to_string(),
            Arc::new(Scheme::ApiKey(ApiKeyScheme {
                name: "key".into(),
                header: Some("x-api-key".into()),
                query: None,
                keys: ApiKeys::Static(vec!["k1".into()]),
            })),
        );
        schemes.insert(
            "remote".to_string(),
            Arc::new(Scheme::ApiKey(ApiKeyScheme {
                name: "remote".into(),
                header: Some("x-customer-key".into()),
                query: None,
                keys: ApiKeys::Remote(KeyVerifier {
                    url: "https://auth.example.com/verify-key".into(),
                    headers: vec![],
                    timeout: Duration::from_secs(1),
                    cache: None,
                }),
            })),
        );
        let set = Arc::new(KeySet::from_secret("secret").unwrap());
        let verdict = |name: &str| -> Option<Verdict> {
            if name != "remote" {
                return None;
            }
            let key = headers.iter().find(|(n, _)| *n == "x-customer-key")?;
            Some(match key.1.as_str() {
                "good" => Verdict::Active(
                    json!({"sub": "customer_1", "scope": "reports:read"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
                "down" => Verdict::Unavailable("verifier returned 502".into()),
                _ => Verdict::Inactive,
            })
        };
        authenticate(
            req,
            &schemes,
            &|_| Some(set.clone()),
            &verdict,
            &Req(headers.clone()),
            1_000,
        )
    }

    fn jwt_req(scopes: &[&str]) -> AuthReq {
        AuthReq::AnyOf(vec![AuthAlt {
            scheme: "jwt".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }])
    }

    #[test]
    fn jwt_success_and_scopes() {
        let t = token(
            json!({"sub": "u1", "iss": "https://auth.example.com/", "aud": "api", "exp": 2000, "scope": "a b"}),
        );
        let headers = vec![("authorization", format!("Bearer {t}"))];
        let ok = run(&jwt_req(&["a"]), headers.clone()).unwrap().unwrap();
        assert_eq!(ok.claims["sub"], "u1");
        let err = run(&jwt_req(&["c"]), headers).unwrap_err();
        assert!(matches!(err, AuthFailure::InsufficientScope { .. }));
    }

    #[test]
    fn jwt_rejections() {
        let expired = token(json!({"iss": "https://auth.example.com/", "aud": "api", "exp": 10}));
        let wrong_aud =
            token(json!({"iss": "https://auth.example.com/", "aud": "other", "exp": 2000}));
        for t in [expired, wrong_aud, "garbage".to_string()] {
            let err = run(
                &jwt_req(&[]),
                vec![("authorization", format!("Bearer {t}"))],
            )
            .unwrap_err();
            assert!(matches!(err, AuthFailure::Unauthorized { .. }), "{err:?}");
        }
        // `exp` and `nbf` are NumericDates: a fraction counts, the moment of
        // expiry is already too late, and anything else is rejected.
        let claims = |extra: Value| {
            let mut c = json!({"iss": "https://auth.example.com/", "aud": "api"});
            c.as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap());
            vec![("authorization", format!("Bearer {}", token(c)))]
        };
        assert!(run(&jwt_req(&[]), claims(json!({"exp": 1000.5}))).is_ok());
        for bad in [
            json!({}),
            json!({"nbf": 0}),
            json!({"exp": 999.5}),
            json!({"exp": 1000}),
            json!({"exp": "2000"}),
            json!({"exp": null}),
            json!({"exp": 2000, "nbf": 1000.5}),
            json!({"exp": 2000, "nbf": "0"}),
        ] {
            let err = run(&jwt_req(&[]), claims(bad.clone())).unwrap_err();
            assert!(matches!(err, AuthFailure::Unauthorized { .. }), "{bad}");
        }
        let missing = run(&jwt_req(&[]), vec![]).unwrap_err();
        assert_eq!(
            missing,
            AuthFailure::Unauthorized {
                scheme: Some("jwt".into()),
                reason: "missing credentials".into()
            }
        );
    }

    #[test]
    fn any_of_and_api_keys() {
        let req = AuthReq::AnyOf(vec![
            AuthAlt {
                scheme: "jwt".into(),
                scopes: vec![],
            },
            AuthAlt {
                scheme: "key".into(),
                scopes: vec![],
            },
        ]);
        let ok = run(&req, vec![("x-api-key", "k1".into())])
            .unwrap()
            .unwrap();
        assert_eq!(ok.scheme, "key");
        assert!(run(&req, vec![("x-api-key", "nope".into())]).is_err());
        assert_eq!(run(&AuthReq::Public, vec![]).unwrap(), None);
    }

    #[test]
    fn remote_api_keys() {
        let req = |scopes: &[&str]| {
            AuthReq::AnyOf(vec![AuthAlt {
                scheme: "remote".into(),
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
            }])
        };
        let ok = run(
            &req(&["reports:read"]),
            vec![("x-customer-key", "good".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.claims["sub"], "customer_1");
        assert!(matches!(
            run(
                &req(&["reports:write"]),
                vec![("x-customer-key", "good".into())]
            ),
            Err(AuthFailure::InsufficientScope { .. })
        ));
        assert!(matches!(
            run(&req(&[]), vec![("x-customer-key", "bad".into())]),
            Err(AuthFailure::Unauthorized { .. })
        ));
        assert!(matches!(
            run(&req(&[]), vec![("x-customer-key", "down".into())]),
            Err(AuthFailure::Unavailable { .. })
        ));
        assert_eq!(
            run(&req(&[]), vec![]).unwrap_err(),
            AuthFailure::Unauthorized {
                scheme: Some("remote".into()),
                reason: "missing credentials".into()
            }
        );

        // An outage outranks another scheme's rejection, and doesn't stop
        // another scheme from succeeding.
        let either = AuthReq::AnyOf(vec![
            AuthAlt {
                scheme: "key".into(),
                scopes: vec![],
            },
            AuthAlt {
                scheme: "remote".into(),
                scopes: vec![],
            },
        ]);
        let down_and_bad = vec![
            ("x-api-key", "nope".to_string()),
            ("x-customer-key", "down".to_string()),
        ];
        assert!(matches!(
            run(&either, down_and_bad),
            Err(AuthFailure::Unavailable { .. })
        ));
        let down_and_good = vec![
            ("x-api-key", "k1".to_string()),
            ("x-customer-key", "down".to_string()),
        ];
        assert_eq!(run(&either, down_and_good).unwrap().unwrap().scheme, "key");
    }

    #[test]
    fn verdict_contract() {
        assert_eq!(
            Verdict::from_response(
                200,
                br#"{"active": true, "sub": "c1", "scope": "a b", "exp": 2000}"#
            ),
            Verdict::Active(
                json!({"sub": "c1", "scope": "a b", "exp": 2000})
                    .as_object()
                    .cloned()
                    .unwrap()
            )
        );
        assert_eq!(
            Verdict::from_response(200, br#"{"active": true}"#),
            Verdict::Active(Map::new())
        );
        assert_eq!(
            Verdict::from_response(200, br#"{"active": false}"#),
            Verdict::Inactive
        );
        for (status, body) in [
            (401, &br#"{"active": false}"#[..]),
            (500, b""),
            (200, b"not json"),
            (200, br#"{"active": "yes"}"#),
            (200, br#"{"sub": "c1"}"#),
            (200, br#"{"active": 1}"#),
            (200, br#"[true]"#),
        ] {
            assert!(
                matches!(
                    Verdict::from_response(status, body),
                    Verdict::Unavailable(_)
                ),
                "{status} {}",
                String::from_utf8_lossy(body)
            );
        }
        assert_eq!(Verdict::request_body("k"), b"token=k");
        assert_eq!(
            Verdict::request_body("a+b &=/\u{e9}"),
            b"token=a%2Bb+%26%3D%2F%C3%A9"
        );
    }

    #[test]
    fn verdict_caching() {
        let active = |claims: Value| Verdict::Active(claims.as_object().cloned().unwrap());
        let max = Duration::from_secs(30);
        for v in [active(json!({"sub": "c1"})), Verdict::Inactive] {
            let json = v.to_json().unwrap();
            assert_eq!(Verdict::from_response(200, &json), v, "round trip");
            assert_eq!(v.cache_ttl(max, 1_000), Some(max));
        }
        assert_eq!(Verdict::Unavailable("down".into()).to_json(), None);
        assert_eq!(
            Verdict::Unavailable("down".into()).cache_ttl(max, 1_000),
            None
        );
        // An `exp` claim caps the TTL, and an expired one isn't cached.
        assert_eq!(
            active(json!({"exp": 1_010})).cache_ttl(max, 1_000),
            Some(Duration::from_secs(10))
        );
        assert_eq!(active(json!({"exp": 1_000})).cache_ttl(max, 1_000), None);
        assert_eq!(
            active(json!({"exp": 9_999})).cache_ttl(max, 1_000),
            Some(max)
        );
    }

    #[test]
    fn www_authenticate_header() {
        let mut schemes = BTreeMap::new();
        schemes.insert("jwt".to_string(), jwt_scheme());
        let h = www_authenticate(
            &AuthFailure::InsufficientScope {
                scheme: "jwt".into(),
                scopes: vec!["billing:write".into()],
            },
            &schemes,
        )
        .unwrap();
        assert_eq!(
            h,
            "Bearer error=\"insufficient_scope\", scope=\"billing:write\", resource_metadata=\"https://api.example.com/.well-known/oauth-protected-resource/mcp\""
        );
        // An API key scheme has a challenge too: every `401` needs one.
        schemes.insert(
            "key".to_string(),
            Arc::new(Scheme::ApiKey(ApiKeyScheme {
                name: "key".into(),
                header: Some("x-api-key".into()),
                query: None,
                keys: ApiKeys::Static(vec!["k1".into()]),
            })),
        );
        let unauthorized = |scheme: &str| AuthFailure::Unauthorized {
            scheme: Some(scheme.into()),
            reason: "missing credentials".into(),
        };
        assert_eq!(
            www_authenticate(&unauthorized("key"), &schemes).as_deref(),
            Some("ApiKey")
        );
        assert!(
            www_authenticate(&unauthorized("jwt"), &schemes)
                .unwrap()
                .starts_with("Bearer resource_metadata=")
        );
        let resource = |url: &str| ProtectedResource {
            resource: url.into(),
            authorization_servers: vec![],
            scopes_supported: vec![],
            resource_name: None,
        };
        for (url, metadata, paths) in [
            (
                "https://api.example.com",
                "https://api.example.com/.well-known/oauth-protected-resource",
                1,
            ),
            (
                "https://api.example.com/",
                "https://api.example.com/.well-known/oauth-protected-resource",
                1,
            ),
            (
                "https://api.example.com:8443/a/b/?x=1",
                "https://api.example.com:8443/.well-known/oauth-protected-resource/a/b",
                2,
            ),
        ] {
            assert_eq!(resource(url).metadata_url(), metadata, "{url}");
            assert_eq!(resource(url).well_known_paths().len(), paths, "{url}");
        }
        let Scheme::Jwt(jwt) = jwt_scheme().as_ref().clone() else {
            unreachable!()
        };
        assert_eq!(
            jwt.protected_resource.unwrap().well_known_paths(),
            vec![
                "/.well-known/oauth-protected-resource",
                "/.well-known/oauth-protected-resource/mcp"
            ]
        );
    }

    #[test]
    fn kid_and_verdict_keys() {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("k1".into());
        let token = encode(
            &header,
            &json!({ "sub": "a" }),
            &EncodingKey::from_secret(b"secret").unwrap(),
        )
        .unwrap();
        let Scheme::Jwt(mut scheme) = jwt_scheme().as_ref().clone() else {
            unreachable!()
        };
        let kid = |scheme: &JwtScheme, header: &'static str, value: String| {
            token_kid(scheme, &Req(vec![(header, value)]))
        };
        assert_eq!(
            kid(&scheme, "authorization", format!("Bearer {token}")).as_deref(),
            Some("k1")
        );
        assert_eq!(kid(&scheme, "authorization", "Bearer nope".into()), None);
        assert_eq!(
            kid(&scheme, "authorization", format!("Basic {token}")),
            None
        );
        assert_eq!(token_kid(&scheme, &Req(vec![])), None);
        // A scheme on another header carries the bare token there.
        scheme.header = "x-token".into();
        assert_eq!(
            kid(&scheme, "x-token", token.clone()).as_deref(),
            Some("k1")
        );
        assert_eq!(
            kid(&scheme, "authorization", format!("Bearer {token}")),
            None
        );

        // Keys are cached per source, and the cache key doesn't hold it.
        let cached = scheme.cache_key();
        assert!(cached.starts_with("jwt|") && !cached.contains("secret"));
        let mut rotated = scheme.clone();
        rotated.keys = JwtKeys::Secret("rotated".into());
        assert_ne!(cached, rotated.cache_key());
        rotated.keys = JwtKeys::Jwks("secret".into());
        assert_ne!(cached, rotated.cache_key());

        let verifier = KeyVerifier {
            url: "https://keys.example/verify".into(),
            headers: vec![],
            timeout: Duration::from_secs(1),
            cache: None,
        };
        let key = verifier.cache_key("sk_live_1");
        assert!(key.starts_with("auth|") && !key.contains("sk_live_1"));
        assert_ne!(key, verifier.cache_key("sk_live_2"));
    }
}
