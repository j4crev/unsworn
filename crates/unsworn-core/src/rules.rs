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
/// rule: only if the cache key separates users, by varying on a header that
/// carries credentials or by keying on a claim. `credential_headers` are the
/// (lowercase) headers the route's auth schemes read.
pub fn cache_separates_users(rule: &CacheRule, credential_headers: &[String]) -> bool {
    rule.vary
        .iter()
        .any(|h| h == "authorization" || credential_headers.contains(h))
        || matches!(rule.key, Some(KeySource::Claim(_)))
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
    // `Vary: *` says no later request can be matched to this response
    // (RFC 9111 section 4.1).
    if header("vary")
        .iter()
        .flat_map(|v| v.split(','))
        .any(|v| v.trim() == "*")
    {
        return false;
    }
    !header("content-type")
        .iter()
        .any(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"))
}

/// An HTTP date, in milliseconds since the Unix epoch.
fn http_date_ms(value: &str) -> Option<u64> {
    let time = httpdate::parse_http_date(value.trim()).ok()?;
    let since = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_millis() as u64)
}

/// How long a response may be served from the cache, or `None` when it's
/// already stale and shouldn't be stored.
///
/// The response decides, as RFC 9111 section 4.2 has a shared cache work it
/// out: `s-maxage`, else `max-age`, else `Expires` (counted from `Date`).
/// The rule's `ttl` is the lifetime of a response that says nothing, and
/// the most any response gets. Time the response already spent in caches
/// upstream (`Age`) comes off. Freshness information that can't be read
/// means stale. `header` returns all values of a (lowercase) response
/// header, and `now_ms` is the host's wall clock.
pub fn cache_lifetime(
    ttl: std::time::Duration,
    now_ms: u64,
    header: &dyn Fn(&str) -> Vec<String>,
) -> Option<std::time::Duration> {
    use std::time::Duration;
    let cache_control = header("cache-control").join(",").to_ascii_lowercase();
    let directive = |name: &str| {
        cache_control.split(',').find_map(|d| {
            let (key, value) = d.trim().split_once('=')?;
            (key.trim() == name).then(|| {
                let seconds = value.trim().trim_matches('"').parse::<u64>();
                Duration::from_secs(seconds.unwrap_or(0))
            })
        })
    };
    let expires = || {
        let expires = header("expires").into_iter().next()?;
        let date = header("date").into_iter().next();
        let from = date.as_deref().and_then(http_date_ms).unwrap_or(now_ms);
        // A date that can't be read, such as `0`, is one in the past.
        let until = http_date_ms(&expires).unwrap_or(0);
        Some(Duration::from_millis(until.saturating_sub(from)))
    };
    let stated = directive("s-maxage")
        .or_else(|| directive("max-age"))
        .or_else(expires);
    let lifetime = stated.map_or(ttl, |stated| stated.min(ttl));
    let age = response_age(header);
    lifetime.checked_sub(age).filter(|left| !left.is_zero())
}

/// The `Age` a response arrived with: how long caches upstream held it.
pub fn response_age(header: &dyn Fn(&str) -> Vec<String>) -> std::time::Duration {
    let seconds = header("age")
        .first()
        .and_then(|v| v.trim().parse::<u64>().ok());
    std::time::Duration::from_secs(seconds.unwrap_or(0))
}

/// Whether a conditional request's validators match a stored response, so
/// the client's copy is still good and `304` can stand in for the body
/// (RFC 9110 section 13.2.2): `If-None-Match` against the `ETag`, or, only
/// without one, `If-Modified-Since` against `Last-Modified`.
pub fn not_modified(
    if_none_match: Option<&str>,
    if_modified_since: Option<&str>,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> bool {
    if let Some(condition) = if_none_match {
        return etag.is_some_and(|etag| etag_listed(condition, etag));
    }
    match (
        if_modified_since.and_then(http_date_ms),
        last_modified.and_then(http_date_ms),
    ) {
        (Some(since), Some(modified)) => modified <= since,
        _ => false,
    }
}

/// Whether an `If-None-Match` list names `etag`, compared weakly: `W/`
/// prefixes don't count.
fn etag_listed(list: &str, etag: &str) -> bool {
    let opaque = |tag: &str| tag.strip_prefix("W/").unwrap_or(tag).to_string();
    let want = opaque(etag.trim());
    let mut rest = list.trim();
    if rest == "*" {
        return true;
    }
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        let tag = rest.strip_prefix("W/").unwrap_or(rest);
        // An entity tag is quoted, and may hold commas.
        let Some(end) = tag.strip_prefix('"').and_then(|t| t.find('"')) else {
            return false;
        };
        if tag[..end + 2] == want {
            return true;
        }
        rest = &tag[end + 2..];
    }
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
        let vary = |value: &'static str| {
            move |h: &str| {
                if h == "vary" {
                    vec![value.to_string()]
                } else {
                    vec![]
                }
            }
        };
        assert!(cache_storable(200, &vary("Accept-Encoding")));
        assert!(!cache_storable(200, &vary("Accept, *")));
    }

    #[test]
    fn lifetimes() {
        let ttl = Duration::from_secs(30);
        let now = 1_700_000_000_000; // Tue, 14 Nov 2023 22:13:20 GMT
        let life = |headers: &'static [(&'static str, &'static str)]| {
            cache_lifetime(ttl, now, &|name| {
                headers
                    .iter()
                    .filter(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
                    .collect()
            })
            .map(|d| d.as_secs())
        };
        // Nothing stated: the rule's ttl.
        assert_eq!(life(&[]), Some(30));
        assert_eq!(
            life(&[("cache-control", "public, must-revalidate")]),
            Some(30)
        );
        // The response may shorten it, never lengthen it.
        assert_eq!(life(&[("cache-control", "max-age=5")]), Some(5));
        assert_eq!(life(&[("cache-control", "Max-Age=3600")]), Some(30));
        assert_eq!(life(&[("cache-control", "max-age=0")]), None);
        // A shared cache reads s-maxage first, then max-age, then Expires.
        assert_eq!(
            life(&[("cache-control", "max-age=5, s-maxage=10")]),
            Some(10)
        );
        assert_eq!(
            life(&[
                ("cache-control", "max-age=5"),
                ("expires", "Tue, 14 Nov 2023 22:13:40 GMT")
            ]),
            Some(5)
        );
        assert_eq!(
            life(&[("expires", "Tue, 14 Nov 2023 22:13:40 GMT")]),
            Some(20)
        );
        assert_eq!(
            life(&[
                ("expires", "Tue, 14 Nov 2023 22:13:40 GMT"),
                ("date", "Tue, 14 Nov 2023 22:13:30 GMT")
            ]),
            Some(10)
        );
        assert_eq!(life(&[("expires", "Tue, 14 Nov 2023 22:13:20 GMT")]), None);
        // What can't be read is stale.
        assert_eq!(life(&[("expires", "0")]), None);
        assert_eq!(life(&[("cache-control", "max-age=soon")]), None);
        // Time spent in caches upstream counts.
        assert_eq!(life(&[("age", "10")]), Some(20));
        assert_eq!(
            life(&[("cache-control", "max-age=20"), ("age", "5")]),
            Some(15)
        );
        assert_eq!(life(&[("cache-control", "max-age=5"), ("age", "5")]), None);
    }

    #[test]
    fn conditions() {
        let etag = Some("\"v1\"");
        let modified = Some("Tue, 14 Nov 2023 22:13:20 GMT");
        for matching in [
            "\"v1\"",
            "W/\"v1\"",
            "\"a,b\", \"v1\"",
            "\"v0\" ,W/\"v1\"",
            "*",
        ] {
            assert!(not_modified(Some(matching), None, etag, None), "{matching}");
        }
        assert!(not_modified(Some("\"v1\""), None, Some("W/\"v1\""), None));
        for other in ["\"v2\"", "v1", "\"v", "", "\"v0\", \"v11\""] {
            assert!(!not_modified(Some(other), None, etag, None), "{other}");
        }
        assert!(!not_modified(Some("\"v1\""), None, None, modified));
        // Dates only count when there's no If-None-Match.
        let later = Some("Tue, 14 Nov 2023 22:13:21 GMT");
        let earlier = Some("Tue, 14 Nov 2023 22:13:19 GMT");
        assert!(not_modified(None, modified, None, modified));
        assert!(not_modified(None, later, None, modified));
        assert!(!not_modified(None, earlier, None, modified));
        assert!(!not_modified(None, Some("yesterday"), None, modified));
        assert!(!not_modified(None, later, etag, None));
        assert!(!not_modified(Some("\"v2\""), later, etag, modified));
        assert!(!not_modified(None, None, etag, modified));
    }

    #[test]
    fn user_separation() {
        let mut rule = CacheRule {
            ttl: Duration::from_secs(1),
            vary: vec![],
            key: None,
        };
        assert!(!cache_separates_users(&rule, &[]));
        rule.vary = vec!["x-api-key".into()];
        assert!(cache_separates_users(&rule, &["x-api-key".into()]));
        rule.vary.clear();
        rule.key = Some(KeySource::Claim("sub".into()));
        assert!(cache_separates_users(&rule, &[]));
    }
}
