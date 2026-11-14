//! Credential verification: JWT and API keys.
//!
//! Pure: keys and the current time are passed in. The host fetches JWKS
//! documents and caches the resulting [`KeySet`]s.

use std::collections::BTreeMap;
use std::sync::Arc;

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
        let mut paths = vec!["/.well-known/oauth-protected-resource".to_string()];
        let (_, path) = split_url(&self.resource);
        let path = path.trim_end_matches('/');
        if !path.is_empty() {
            paths.push(format!("/.well-known/oauth-protected-resource{path}"));
        }
        paths
    }

    /// The metadata URL to put in `WWW-Authenticate`.
    pub fn metadata_url(&self) -> String {
        let (origin, path) = split_url(&self.resource);
        format!(
            "{origin}/.well-known/oauth-protected-resource{}",
            path.trim_end_matches('/')
        )
    }
}

/// Splits `https://host:port/path` into (`https://host:port`, `/path`).
fn split_url(url: &str) -> (&str, &str) {
    let start = url.find("://").map_or(0, |i| i + 3);
    match url[start..].find('/') {
        Some(i) => url.split_at(start + i),
        None => (url, ""),
    }
}

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
    pub keys: Vec<String>,
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
pub fn authenticate(
    req: &AuthReq,
    schemes: &BTreeMap<String, Arc<Scheme>>,
    keys: &dyn Fn(&str) -> Option<Arc<KeySet>>,
    source: &dyn CredentialSource,
    now: u64,
) -> Result<Option<AuthSuccess>, AuthFailure> {
    let alts = match req {
        AuthReq::Public => return Ok(None),
        AuthReq::AnyOf(alts) => alts,
    };
    let mut first_failure: Option<AuthFailure> = None;
    let mut scope_failure: Option<AuthFailure> = None;
    for AuthAlt { scheme, scopes } in alts {
        let Some(s) = schemes.get(scheme) else {
            continue;
        };
        let result = match s.as_ref() {
            Scheme::Jwt(jwt) => check_jwt(jwt, keys, source, now),
            Scheme::ApiKey(k) => check_api_key(k, source),
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
    Err(scope_failure
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
    let token = if scheme.header == "authorization" {
        match raw.split_once(' ') {
            Some((kind, token)) if kind.eq_ignore_ascii_case("bearer") => token.trim(),
            _ => return Ok(None),
        }
    } else {
        raw.trim()
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

    let leeway = scheme.leeway;
    if let Some(exp) = claims.get("exp").and_then(Value::as_u64)
        && now > exp + leeway
    {
        return Err("token has expired".into());
    }
    if let Some(nbf) = claims.get("nbf").and_then(Value::as_u64)
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

fn check_api_key(
    scheme: &ApiKeyScheme,
    source: &dyn CredentialSource,
) -> Result<Option<Map<String, Value>>, String> {
    let provided = scheme
        .header
        .as_deref()
        .and_then(|h| source.header(h))
        .or_else(|| scheme.query.as_deref().and_then(|q| source.query(q)));
    let Some(provided) = provided else {
        return Ok(None);
    };
    let matched = scheme
        .keys
        .iter()
        .position(|k| constant_time_eq(k.as_bytes(), provided.trim().as_bytes()));
    match matched {
        Some(index) => {
            let mut claims = Map::new();
            claims.insert("type".into(), json!("api_key"));
            claims.insert("key".into(), json!(index));
            Ok(Some(claims))
        }
        None => Err("invalid API key".into()),
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

/// The `WWW-Authenticate` header for a failure.
pub fn www_authenticate(
    failure: &AuthFailure,
    schemes: &BTreeMap<String, Arc<Scheme>>,
) -> Option<String> {
    let scheme_name = match failure {
        AuthFailure::Unauthorized { scheme, .. } => scheme.as_deref()?,
        AuthFailure::InsufficientScope { scheme, .. } => scheme,
    };
    let Scheme::Jwt(jwt) = schemes.get(scheme_name)?.as_ref() else {
        return None;
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
                keys: vec!["k1".into()],
            })),
        );
        let set = Arc::new(KeySet::from_secret("secret").unwrap());
        authenticate(req, &schemes, &|_| Some(set.clone()), &Req(headers), 1_000)
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
}
