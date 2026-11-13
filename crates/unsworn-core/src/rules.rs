//! CORS and caching rules.

use crate::policy::{CacheRule, Cors, KeySource};

/// The value for `Access-Control-Allow-Origin`, if the origin is allowed.
pub fn cors_allow_origin(cors: &Cors, origin: &str) -> Option<String> {
    match &cors.origins {
        None if cors.credentials => Some(origin.to_string()),
        None => Some("*".to_string()),
        Some(list) if list.iter().any(|o| o == origin) => Some(origin.to_string()),
        Some(_) => None,
    }
}

/// Headers for a CORS preflight response. `route_methods` is used when the
/// policy doesn't list methods.
pub fn cors_preflight_headers(
    cors: &Cors,
    origin: &str,
    requested_headers: Option<&str>,
    route_methods: &[String],
) -> Option<Vec<(String, String)>> {
    let allow = cors_allow_origin(cors, origin)?;
    let mut out = vec![("access-control-allow-origin".to_string(), allow)];
    let methods = cors
        .methods
        .clone()
        .unwrap_or_else(|| route_methods.to_vec());
    out.push(("access-control-allow-methods".into(), methods.join(", ")));
    let headers = match (&cors.headers, requested_headers) {
        (Some(list), _) => Some(list.join(", ")),
        (None, Some(requested)) => Some(requested.to_string()),
        (None, None) => None,
    };
    if let Some(h) = headers {
        out.push(("access-control-allow-headers".into(), h));
    }
    if cors.credentials {
        out.push(("access-control-allow-credentials".into(), "true".into()));
    }
    if let Some(max_age) = cors.max_age {
        out.push((
            "access-control-max-age".into(),
            max_age.as_secs().to_string(),
        ));
    }
    out.push(("vary".into(), "Origin".into()));
    Some(out)
}

/// Headers to add to a normal response.
pub fn cors_response_headers(cors: &Cors, origin: &str) -> Vec<(String, String)> {
    let Some(allow) = cors_allow_origin(cors, origin) else {
        return Vec::new();
    };
    let mut out = vec![("access-control-allow-origin".to_string(), allow)];
    if cors.credentials {
        out.push(("access-control-allow-credentials".into(), "true".into()));
    }
    if !cors.expose.is_empty() {
        out.push((
            "access-control-expose-headers".into(),
            cors.expose.join(", "),
        ));
    }
    out.push(("vary".into(), "Origin".into()));
    out
}

/// Only `GET` and `HEAD` responses are cached.
pub fn cache_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD")
}

/// Whether a response to an authenticated request may be cached under this
/// rule: only if the cache key separates users.
pub fn cache_separates_users(rule: &CacheRule) -> bool {
    rule.vary.iter().any(|h| h == "authorization") || matches!(rule.key, Some(KeySource::Claim(_)))
}

const CACHEABLE_STATUS: &[u16] = &[200, 203, 204, 301, 404, 410];

/// Whether a response may be stored. `header` returns all values of a
/// (lowercase) response header.
pub fn cache_storable(status: u16, header: &dyn Fn(&str) -> Vec<String>) -> bool {
    if !CACHEABLE_STATUS.contains(&status) {
        return false;
    }
    if !header("set-cookie").is_empty() {
        return false;
    }
    let cache_control = header("cache-control").join(",").to_ascii_lowercase();
    if cache_control
        .split(',')
        .map(str::trim)
        .any(|d| d == "no-store" || d == "private" || d == "no-cache")
    {
        return false;
    }
    !header("content-type")
        .iter()
        .any(|ct| ct.starts_with("text/event-stream"))
}

/// Builds a cache key from its parts.
pub fn cache_key(
    route: usize,
    method: &str,
    path: &str,
    raw_query: Option<&str>,
    vary: &[(String, Option<String>)],
    key: Option<&str>,
) -> String {
    let method = if method == "HEAD" { "GET" } else { method };
    let mut out = format!("{route}|{method}|{path}?{}", raw_query.unwrap_or(""));
    for (name, value) in vary {
        out.push('|');
        out.push_str(name);
        out.push('=');
        out.push_str(value.as_deref().unwrap_or(""));
    }
    if let Some(k) = key {
        out.push_str("|key=");
        out.push_str(k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cors(origins: Option<Vec<&str>>, credentials: bool) -> Cors {
        Cors {
            origins: origins.map(|o| o.into_iter().map(String::from).collect()),
            credentials,
            methods: None,
            headers: None,
            expose: vec![],
            max_age: Some(Duration::from_secs(600)),
        }
    }

    #[test]
    fn origins() {
        assert_eq!(
            cors_allow_origin(&cors(None, false), "https://a"),
            Some("*".into())
        );
        assert_eq!(
            cors_allow_origin(&cors(None, true), "https://a"),
            Some("https://a".into())
        );
        assert_eq!(
            cors_allow_origin(&cors(Some(vec!["https://b"]), false), "https://a"),
            None
        );
        let pre = cors_preflight_headers(
            &cors(Some(vec!["https://a"]), true),
            "https://a",
            Some("content-type"),
            &["GET".into(), "POST".into()],
        )
        .unwrap();
        assert!(pre.contains(&("access-control-allow-methods".into(), "GET, POST".into())));
        assert!(pre.contains(&("access-control-max-age".into(), "600".into())));
    }

    #[test]
    fn storability() {
        let none = |_: &str| Vec::<String>::new();
        assert!(cache_storable(200, &none));
        assert!(!cache_storable(500, &none));
        let private = |h: &str| {
            if h == "cache-control" {
                vec!["private, max-age=0".to_string()]
            } else {
                vec![]
            }
        };
        assert!(!cache_storable(200, &private));
        let cookie = |h: &str| {
            if h == "set-cookie" {
                vec!["a=b".to_string()]
            } else {
                vec![]
            }
        };
        assert!(!cache_storable(200, &cookie));
    }

    #[test]
    fn user_separation() {
        let mut rule = CacheRule {
            ttl: Duration::from_secs(1),
            vary: vec![],
            key: None,
        };
        assert!(!cache_separates_users(&rule));
        rule.key = Some(KeySource::Claim("sub".into()));
        assert!(cache_separates_users(&rule));
    }
}
