//! The checks and conversions a request goes through, as pure functions.

use crate::mcp::rpc::JsonError;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::net::IpAddr;

use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use serde_json::{Map, Value};

use super::Problem;
use crate::policy::{
    CacheRule, Cors, Effective, IpRules, KeySource, Operand, RateLimit, Requirement,
};
use crate::project::Route;
use crate::rules;

/// The query string's pairs, decoded.
pub fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    query
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default()
}

/// Percent-decodes a path parameter.
pub fn decode_param(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Normalizes a request path the way RFC 3986 section 6.2.2 describes, so
/// every route and policy sees the path an upstream would resolve it to:
/// percent-encoded unreserved characters are decoded (`%61` is `a`), other
/// escapes get uppercase hex digits, and `.` and `..` segments are removed.
/// Reserved characters stay encoded, so `%2F` is never a separator.
pub fn normalize_path(path: &str) -> Cow<'_, str> {
    if !path.starts_with('/') || (!path.contains('%') && !path.contains("/.")) {
        return Cow::Borrowed(path);
    }
    let bytes = path.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut decoded = String::with_capacity(path.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| Some(hex(*bytes.get(i + 1)?)? << 4 | hex(*bytes.get(i + 2)?)?))
            .flatten();
        match escaped {
            Some(b) if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') => {
                decoded.push(b as char);
                i += 3;
            }
            Some(b) => {
                decoded.push_str(&format!("%{b:02X}"));
                i += 3;
            }
            None => {
                // `path` is valid UTF-8, and this copies whole characters.
                let len = path[i..].chars().next().map_or(1, char::len_utf8);
                decoded.push_str(&path[i..i + len]);
                i += len;
            }
        }
    }
    let mut segments: Vec<&str> = Vec::new();
    // A path that ends in a dot segment names a directory.
    let mut directory = false;
    for segment in decoded.split('/').skip(1) {
        match segment {
            "." => directory = true,
            ".." => {
                segments.pop();
                directory = true;
            }
            s => {
                segments.push(s);
                directory = false;
            }
        }
    }
    let mut out = format!("/{}", segments.join("/"));
    if directory && !segments.is_empty() {
        out.push('/');
    }
    if out == path {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(out)
    }
}

/// Whether a path under a `static` route names a dotfile, or something in
/// a dot directory. `.well-known` is public by definition (RFC 8615).
pub fn hidden_path(rest: &str) -> bool {
    rest.split('/')
        .any(|segment| segment.starts_with('.') && segment != ".well-known")
}

fn ip_matches(entry: &str, ip: IpAddr) -> bool {
    if let Ok(net) = entry.parse::<ipnet::IpNet>() {
        return net.contains(&ip);
    }
    entry.parse::<IpAddr>().is_ok_and(|e| e == ip)
}

/// Whether `ip` passes the allow and deny lists. Deny wins.
pub fn ip_allowed(rules: &IpRules, ip: IpAddr) -> bool {
    if rules.deny.iter().any(|d| ip_matches(d, ip)) {
        return false;
    }
    rules.allow.is_empty() || rules.allow.iter().any(|a| ip_matches(a, ip))
}

/// The `origin` and `ip` policies.
pub fn access(eff: &Effective, origin: Option<&str>, client_ip: IpAddr) -> Result<(), Problem> {
    if let (Some(allow), Some(origin)) = (&eff.origin, origin)
        && !allow.iter().any(|a| a == origin)
    {
        return Err(Problem::new(
            StatusCode::FORBIDDEN,
            format!("requests from origin {origin} aren't allowed"),
        ));
    }
    // A host that doesn't know the client's address passes `0.0.0.0`,
    // which no client has. It can't be shown to be allowed, or not denied.
    if let Some(rules) = &eff.ip
        && (client_ip.is_unspecified() || !ip_allowed(rules, client_ip))
    {
        return Err(Problem::new(
            StatusCode::FORBIDDEN,
            "requests from this address aren't allowed",
        ));
    }
    Ok(())
}

/// Validates and converts path parameters, query parameters and headers.
/// Returns the params and query the handler sees.
pub fn validate_inputs(
    route: &Route,
    raw_params: &[(String, String)],
    query_pairs: &[(String, String)],
    headers: &HeaderMap,
) -> Result<(Map<String, Value>, Map<String, Value>), Problem> {
    let invalid = |what: &str, errors: &[_]| {
        Problem::new(StatusCode::BAD_REQUEST, format!("invalid {what}")).with_errors(Some(errors))
    };
    let params = match &route.validation.params {
        Some(rules) => rules
            .apply(raw_params)
            .map_err(|e| invalid("path parameters", &e))?,
        None => raw_params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    };
    let query = match &route.validation.query {
        Some(rules) => rules
            .apply(query_pairs)
            .map_err(|e| invalid("query parameters", &e))?,
        None => {
            let mut query = Map::new();
            for (k, v) in query_pairs {
                query.entry(k.clone()).or_insert(Value::String(v.clone()));
            }
            query
        }
    };
    if let Some(rules) = &route.validation.headers {
        let pairs: Vec<(String, String)> = headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_string(), v.to_string()))
            })
            .collect();
        rules.apply(&pairs).map_err(|e| invalid("headers", &e))?;
    }
    Ok((params, query))
}

/// Rejects a body whose declared length is over the limit, before reading
/// it.
pub fn check_content_length(headers: &HeaderMap, max_body: u64) -> Result<(), Problem> {
    match headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(len) if len > max_body => Err(Problem::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the body is larger than {max_body} bytes"),
        )),
        _ => Ok(()),
    }
}

/// What [`check_body`] found a request's body to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonBody {
    /// Not JSON, or empty.
    No,
    /// JSON, read and found well-formed.
    Checked,
    /// Sent as JSON and not read. See [`check_body`].
    Unread,
}

/// The `400` for a body that was sent as JSON and isn't, or that has an
/// object with two members of one name.
pub fn bad_json(e: JsonError) -> Problem {
    match e {
        JsonError::Invalid(e) => Problem::new(
            StatusCode::BAD_REQUEST,
            format!("the body isn't valid JSON: {e}"),
        ),
        JsonError::Duplicate(key) => Problem::new(
            StatusCode::BAD_REQUEST,
            format!("the member \"{key}\" appears more than once in an object in the body"),
        ),
    }
}

/// Checks JSON text the way [`check_body`] does, for a host that left a
/// body unread and then has to write it into a message after all. `Err`
/// has the `400`'s detail.
pub fn check_json_text(text: &[u8]) -> Result<(), String> {
    crate::mcp::rpc::check_json(text).map_err(|e| bad_json(e).detail)
}

/// Checks a JSON body: that it's well-formed, and that it matches the
/// route's body schema if there is one.
///
/// The parsed value isn't kept. A handler is sent the body as it arrived
/// (see `unsworn_proto::Json`), so only a body with a schema to check is
/// built in memory at all.
///
/// With `may_leave_unread`, a body with no schema to check isn't read
/// here: its one reader is the handler it's going to, which parses it
/// anyway and answers `400` if it can't.
pub fn check_body(
    route: &Route,
    headers: &HeaderMap,
    body: Option<&[u8]>,
    may_leave_unread: bool,
) -> Result<JsonBody, Problem> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    // The media type without its parameters: `application/json`, or any
    // type with the `+json` structured syntax suffix (RFC 6839).
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    let is_json = media_type == "application/json" || media_type.ends_with("+json");
    let empty_body = body.is_none_or(<[u8]>::is_empty);
    let mut json_body = None;
    let mut found = JsonBody::No;
    if let Some(bytes) = body
        && !empty_body
        && is_json
    {
        if may_leave_unread && route.validation.body.is_none() {
            return Ok(JsonBody::Unread);
        }
        // Parsers disagree about which of two members with one name
        // counts. This one keeps the last, and an upstream behind a `proxy`
        // route gets the body as it was sent, so it could act on a value
        // the body schema never saw.
        let parsed = match &route.validation.body {
            Some(_) => crate::mcp::rpc::parse_json(bytes).map(Some),
            None => crate::mcp::rpc::check_json(bytes).map(|()| None),
        };
        json_body = parsed.map_err(bad_json)?;
        found = JsonBody::Checked;
    }
    if let Some(rule) = &route.validation.body {
        if empty_body {
            if rule.required {
                return Err(Problem::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "a JSON body is required",
                ));
            }
        } else if !is_json {
            return Err(Problem::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "the body must be JSON (Content-Type: application/json)",
            ));
        } else if let Some(v) = &json_body
            && let Err(errors) = rule.check(v, "body")
        {
            return Err(
                Problem::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid body")
                    .with_errors(Some(&errors)),
            );
        }
    }
    Ok(found)
}

/// Request headers as the handler protocol carries them: one value per
/// name, repeated headers joined.
pub fn header_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in headers {
        let Ok(v) = v.to_str() else { continue };
        let sep = if k == header::COOKIE { "; " } else { ", " };
        out.entry(k.as_str().to_string())
            .and_modify(|e| {
                e.push_str(sep);
                e.push_str(v);
            })
            .or_insert_with(|| v.to_string());
    }
    out
}

/// The value a policy key picks out of a request.
pub fn key_value(
    key: &KeySource,
    client_ip: IpAddr,
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
) -> Option<String> {
    match key {
        KeySource::Ip => Some(client_ip.to_string()),
        KeySource::Header(h) => headers
            .get(h.as_str())
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        KeySource::Claim(c) => claims.and_then(|m| m.get(c)).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
    }
}

/// The first `require` rule a request doesn't meet, if any: its two values
/// differ, or the request lacks one of them. `path` is the normalized
/// request path.
pub fn unmet<'a>(
    rules: &'a [Requirement],
    path: &str,
    query: &[(String, String)],
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
) -> Option<&'a Requirement> {
    let value = |side: &Operand| -> Option<String> {
        match side {
            // A claim that's a list or an object equals nothing: comparing
            // its JSON text with a path segment would be a guess.
            Operand::Claim(name) => match claims?.get(name)? {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            },
            Operand::Header(name) => {
                let mut values = headers.get_all(name.as_str()).iter();
                let first = values.next()?.to_str().ok()?;
                // Sent twice, it has no one value to compare.
                values.next().is_none().then(|| first.to_string())
            }
            Operand::Query(name) => {
                let mut values = query.iter().filter(|(k, _)| k == name);
                let first = values.next()?;
                values.next().is_none().then(|| first.1.clone())
            }
            Operand::Param {
                segment: Some(at),
                rest,
                ..
            } => {
                let mut parts = path.split('/').filter(|s| !s.is_empty()).skip(*at);
                match rest {
                    false => parts.next().map(decode_param),
                    true => Some(decode_param(&parts.collect::<Vec<_>>().join("/"))),
                }
            }
            Operand::Param { segment: None, .. } => None,
        }
    };
    rules
        .iter()
        .find(|rule| match (value(&rule.left), value(&rule.right)) {
            (Some(left), Some(right)) => left != right,
            _ => true,
        })
}

/// The client's address as a rate-limit key. An IPv6 client counts by its
/// /64: a network hands out at least that much to one subscriber, who could
/// otherwise get a new bucket with every request by changing address.
pub fn rate_limit_ip(client_ip: IpAddr) -> String {
    match client_ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => {
            let network = u128::from(ip) & (u128::MAX << 64);
            format!("{}/64", std::net::Ipv6Addr::from(network))
        }
    }
}

/// The rate-limit bucket a request counts against: the limit's counter,
/// which is where the limit is written, and the caller. A key the request
/// lacks falls back to the client's IP. A header's value is hashed, since the
/// header may well carry a credential (`header.x-api-key`), and the store
/// shouldn't hold one.
pub fn rate_limit_bucket(
    limit: &RateLimit,
    client_ip: IpAddr,
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
) -> String {
    let key = match &limit.key {
        KeySource::Ip => None,
        key @ KeySource::Header(_) => key_value(key, client_ip, headers, claims).map(|v| {
            use sha2::{Digest, Sha256};
            let hex: String = Sha256::digest(v.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("sha256:{hex}")
        }),
        key => key_value(key, client_ip, headers, claims),
    }
    .unwrap_or_else(|| rate_limit_ip(client_ip));
    format!("{}|{}|{key}", limit.counter, limit.key.describe())
}

/// The cache key for a request, or `None` when it mustn't be cached: a
/// method other than `GET` or `HEAD`, a protocol upgrade such as a
/// WebSocket, an authenticated request whose key doesn't separate users,
/// one keyed on a claim the caller's credentials don't have, or a request
/// with credentials Unsworn didn't check (an `Authorization` header or
/// cookies, which the upstream may have checked) that the rule doesn't vary
/// on (RFC 9111 section 3.5). `scheme` is the auth scheme that verified
/// the caller, when one did.
#[allow(clippy::too_many_arguments)]
pub fn cache_key(
    rule: &CacheRule,
    credential_headers: &[String],
    route_id: usize,
    method: &str,
    uri: &Uri,
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
    scheme: Option<&str>,
    client_ip: IpAddr,
) -> Option<String> {
    if !rules::cache_method(method) || super::proxy::is_upgrade(headers) {
        return None;
    }
    if claims.is_some() && !rules::cache_separates_users(rule, credential_headers) {
        return None;
    }
    if claims.is_none()
        && [header::AUTHORIZATION, header::COOKIE]
            .iter()
            .any(|h| headers.contains_key(h) && !rule.vary.iter().any(|v| v == h.as_str()))
    {
        return None;
    }
    let vary: Vec<(String, Option<String>)> = rule
        .vary
        .iter()
        .map(|h| {
            let value = joined_values(headers, h);
            (h.clone(), value)
        })
        .collect();
    let key = match &rule.key {
        None => None,
        // A caller without the claim can't be told apart from another
        // one without it, so their responses aren't cached at all.
        Some(k @ KeySource::Claim(_)) => Some(key_value(k, client_ip, headers, claims)?),
        Some(k) => Some(key_value(k, client_ip, headers, claims).unwrap_or_default()),
    };
    Some(rules::cache_key(
        route_id,
        method,
        uri.path(),
        uri.query(),
        &vary,
        key.as_deref(),
        claims.and(scheme),
    ))
}

/// Every value of a request header, joined, or `None` when it's absent.
/// Joined rather than the first, so two requests that differ only in a
/// repeated header's later values don't share an entry.
fn joined_values(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

fn append(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        headers.append(n, v);
    }
}

fn insert(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        headers.insert(n, v);
    }
}

/// The headers for a CORS preflight response. Empty when the origin isn't
/// allowed.
pub fn preflight_headers(
    cors: &Cors,
    origin: &str,
    requested_headers: Option<&str>,
    route_methods: &[String],
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in rules::cors_preflight_headers(cors, origin, requested_headers, route_methods)
        .unwrap_or_default()
    {
        append(&mut out, &k, &v);
    }
    out
}

/// Header rules and CORS headers, applied to every routed response.
pub fn finish_headers(eff: &Effective, origin: Option<&str>, headers: &mut HeaderMap) {
    for name in &eff.response_headers.remove {
        headers.remove(name.as_str());
    }
    for (name, value) in &eff.response_headers.set {
        insert(headers, name, value);
    }
    if let (Some(cors), Some(origin)) = (&eff.cors, origin) {
        for (k, v) in rules::cors_response_headers(cors, origin) {
            if k == "vary" {
                append(headers, &k, &v);
            } else {
                insert(headers, &k, &v);
            }
        }
    }
}

/// Everything but RFC 3986's unreserved characters.
pub(crate) const NOT_UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A `redirect` route's target, with path parameters filled in. Their
/// (decoded) values are percent-encoded again, so the result is a valid
/// URI whatever they hold; a catch-all's slashes stay.
pub fn redirect_location(to: &str, params: &[(String, String)]) -> String {
    let encode = |v: &str| percent_encoding::utf8_percent_encode(v, NOT_UNRESERVED).to_string();
    let mut location = to.to_string();
    for (k, v) in params {
        let path: Vec<String> = v.split('/').map(encode).collect();
        location = location
            .replace(&format!("{{*{k}}}"), &path.join("/"))
            .replace(&format!("{{{k}}}"), &encode(v));
    }
    if to.starts_with("//") {
        location
    } else {
        same_site(location)
    }
}

/// A `Location` built from a request's path, kept on this site. Browsers
/// read `//host/path` as another site's URL, so a path whose first segment
/// is empty (`/old//evil.example`) could otherwise send a visitor anywhere.
pub fn same_site(location: String) -> String {
    if location.starts_with("//") {
        format!("/{}", location.trim_start_matches('/'))
    } else {
        location
    }
}

/// The status and headers of a handler's response, with a `Content-Type`
/// to match its body when the handler set none.
pub fn response_head(
    response: &unsworn_proto::Response,
) -> Result<(StatusCode, HeaderMap), Problem> {
    let Ok(status) = StatusCode::from_u16(response.status) else {
        return Err(Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the handler returned an invalid status {}", response.status),
        ));
    };
    let mut headers = HeaderMap::new();
    for (name, values) in &response.headers {
        let Ok(n) = HeaderName::try_from(name.as_str()) else {
            tracing::warn!(header = %name, "handler returned an invalid header name");
            continue;
        };
        for v in values.iter() {
            if let Ok(v) = HeaderValue::from_str(v) {
                headers.append(n.clone(), v);
            }
        }
    }
    if !headers.contains_key(header::CONTENT_TYPE) {
        let ct = if response.json.is_some() {
            Some("application/json")
        } else if response.text.is_some() {
            Some("text/plain; charset=utf-8")
        } else if response.stream {
            None
        } else if response.body_len.is_some() {
            Some("application/octet-stream")
        } else {
            None
        };
        if let Some(ct) = ct {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
        }
    }
    Ok((status, headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ip_rules() {
        let rules = IpRules {
            allow: vec!["10.0.0.0/8".into()],
            deny: vec!["10.0.0.9".into()],
        };
        assert!(ip_allowed(&rules, ip("10.1.2.3")));
        assert!(!ip_allowed(&rules, ip("10.0.0.9")));
        assert!(!ip_allowed(&rules, ip("192.0.2.1")));
        assert!(ip_allowed(&IpRules::default(), ip("192.0.2.1")));

        // An unknown client fails any `ip` rule, even one it would pass.
        let mut eff = crate::policy::Layer::default().finalize(Duration::from_secs(1), 1);
        assert!(access(&eff, None, ip("0.0.0.0")).is_ok());
        eff.ip = Some(IpRules {
            allow: vec!["0.0.0.0/0".into()],
            deny: vec![],
        });
        assert!(access(&eff, None, ip("192.0.2.1")).is_ok());
        assert!(access(&eff, None, ip("0.0.0.0")).is_err());
        assert!(access(&eff, None, ip("::")).is_err());

        for hidden in [".env", ".git/config", "docs/.htpasswd", "a/.b/c.txt"] {
            assert!(hidden_path(hidden), "{hidden}");
        }
        for shown in [
            "",
            "index.html",
            "a/b.c",
            ".well-known/security.txt",
            "a..b",
        ] {
            assert!(!hidden_path(shown), "{shown}");
        }
    }

    #[test]
    fn keys() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tenant", "acme".parse().unwrap());
        let claims: Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "sub": "u1", "n": 7 })).unwrap();
        let client = ip("192.0.2.1");
        let limit = |key| RateLimit {
            requests: 1,
            per: Duration::from_secs(1),
            key,
            counter: "* /api/{*rest}".into(),
        };
        // The counter is where the limit is written, so every request
        // the limit applies to counts in one place.
        assert_eq!(
            rate_limit_bucket(&limit(KeySource::Ip), client, &headers, None),
            "* /api/{*rest}|ip|192.0.2.1"
        );
        // An IPv6 client is its /64, whatever the rest of the address.
        for v6 in ["2001:db8:1:2:3:4:5:6", "2001:db8:1:2::1"] {
            assert_eq!(
                rate_limit_bucket(&limit(KeySource::Ip), ip(v6), &headers, None),
                "* /api/{*rest}|ip|2001:db8:1:2::/64"
            );
        }
        assert_eq!(
            rate_limit_bucket(
                &limit(KeySource::Header("x-tenant".into())),
                client,
                &headers,
                None
            ),
            // sha256("acme"): the store never holds a header's value.
            "* /api/{*rest}|header.x-tenant|sha256:822b33ad87c148a0a20a5ba7cd5ebcaa68d36a18e7aad165554903f52ca82757"
        );
        // Missing keys fall back to the IP.
        assert_eq!(
            rate_limit_bucket(
                &limit(KeySource::Claim("sub".into())),
                client,
                &headers,
                None
            ),
            "* /api/{*rest}|auth.sub|192.0.2.1"
        );
        assert_eq!(
            key_value(
                &KeySource::Claim("n".into()),
                client,
                &headers,
                Some(&claims)
            ),
            Some("7".into())
        );

        let rule = CacheRule {
            ttl: Duration::from_secs(1),
            vary: vec!["accept".into()],
            key: None,
        };
        let uri: Uri = "/a?b=1".parse().unwrap();
        let key = |rule: &CacheRule,
                   method: &str,
                   uri: &Uri,
                   headers: &HeaderMap,
                   claims: Option<&Map<String, Value>>,
                   scheme: Option<&str>| {
            cache_key(rule, &[], 1, method, uri, headers, claims, scheme, client)
        };
        let anon = key(&rule, "GET", &uri, &headers, None, None).unwrap();
        // Hashed: the key holds none of the request's values.
        assert!(anon.starts_with("resp|") && anon.len() == 5 + 64, "{anon}");
        assert_eq!(
            key(&rule, "HEAD", &uri, &headers, None, None).as_ref(),
            Some(&anon)
        );
        assert_eq!(key(&rule, "POST", &uri, &headers, None, None), None);
        let other: Uri = "/a?b=2".parse().unwrap();
        assert_ne!(
            key(&rule, "GET", &other, &headers, None, None).unwrap(),
            anon
        );
        // Authenticated, and the key doesn't separate users.
        assert_eq!(
            key(&rule, "GET", &uri, &headers, Some(&claims), Some("jwt")),
            None
        );
        // Credentials Unsworn didn't check: cached only per credential.
        let mut bearer = headers.clone();
        bearer.insert("authorization", "Bearer a".parse().unwrap());
        assert_eq!(key(&rule, "GET", &uri, &bearer, None, None), None);
        let per_user = CacheRule {
            vary: vec!["authorization".into()],
            ..rule.clone()
        };
        let a = key(&per_user, "GET", &uri, &bearer, None, None).unwrap();
        assert!(!a.contains("Bearer"));
        bearer.insert("authorization", "Bearer b".parse().unwrap());
        assert_ne!(key(&per_user, "GET", &uri, &bearer, None, None).unwrap(), a);
        // Cookies are credentials too: an app behind a proxy route may keep
        // its sessions in them.
        let mut session = headers.clone();
        session.insert("cookie", "sid=1".parse().unwrap());
        assert_eq!(key(&rule, "GET", &uri, &session, None, None), None);
        let per_session = CacheRule {
            vary: vec!["cookie".into()],
            ..rule.clone()
        };
        assert!(key(&per_session, "GET", &uri, &session, None, None).is_some());
        // A WebSocket upgrade never gets a cached response.
        let mut upgrade = headers.clone();
        upgrade.insert("connection", "Upgrade".parse().unwrap());
        upgrade.insert("upgrade", "websocket".parse().unwrap());
        assert_eq!(key(&rule, "GET", &uri, &upgrade, None, None), None);

        // Keyed on a claim: one entry per user, per scheme. A caller without
        // the claim (an API key has no `sub`) isn't cached at all, rather
        // than sharing one entry with every other such caller.
        let by_sub = CacheRule {
            key: Some(KeySource::Claim("sub".into())),
            ..rule.clone()
        };
        let u1 = key(&by_sub, "GET", &uri, &headers, Some(&claims), Some("jwt")).unwrap();
        let other_scheme = key(
            &by_sub,
            "GET",
            &uri,
            &headers,
            Some(&claims),
            Some("partner"),
        );
        assert_ne!(Some(u1), other_scheme);
        let api_key: Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "type": "api_key", "key": 0 })).unwrap();
        assert_eq!(
            key(&by_sub, "GET", &uri, &headers, Some(&api_key), Some("keys")),
            None
        );

        // Parts are length-prefixed, so a query can't spell out the
        // `vary` part of another request's key.
        let two = CacheRule {
            vary: vec!["x-a".into(), "x-b".into()],
            ..rule.clone()
        };
        let mut h1 = HeaderMap::new();
        h1.insert("x-a", "1|x-b=2".parse().unwrap());
        let mut h2 = HeaderMap::new();
        h2.insert("x-a", "1".parse().unwrap());
        h2.insert("x-b", "2".parse().unwrap());
        assert_ne!(
            key(&two, "GET", &uri, &h1, None, None),
            key(&two, "GET", &uri, &h2, None, None)
        );
    }

    #[test]
    fn normalized_paths() {
        for (path, want) in [
            ("/", "/"),
            ("/a/b", "/a/b"),
            ("/.well-known/x", "/.well-known/x"),
            ("/a/./b", "/a/b"),
            ("/public/../admin/x", "/admin/x"),
            ("/a/b/..", "/a/"),
            ("/a/b/.", "/a/b/"),
            ("/../..", "/"),
            ("/a/../", "/"),
            ("/a//b/", "/a//b/"),
            ("/%61dmin", "/admin"),
            ("/a/%2e%2E/b", "/b"),
            ("/a%2fb/%c3%a9", "/a%2Fb/%C3%A9"),
            ("/a%zz/100%", "/a%zz/100%"),
            ("/caf\u{e9}/%7Euser", "/caf\u{e9}/~user"),
            ("*", "*"),
        ] {
            assert_eq!(normalize_path(path), want, "{path}");
        }
        assert!(matches!(normalize_path("/a%2Fb"), Cow::Borrowed(_)));
    }

    #[test]
    fn redirects_and_decoding() {
        let params = vec![("id".to_string(), "7".to_string())];
        assert_eq!(redirect_location("/v2/{id}", &params), "/v2/7");
        assert_eq!(redirect_location("/files/{*id}", &params), "/files/7");
        let params = vec![("p".to_string(), "a b/c?d#\u{e9}\n".to_string())];
        assert_eq!(
            redirect_location("/to/{p}", &params),
            "/to/a%20b%2Fc%3Fd%23%C3%A9%0A"
        );
        assert_eq!(
            redirect_location("https://x.example/{*p}", &params),
            "https://x.example/a%20b/c%3Fd%23%C3%A9%0A"
        );
        // A parameter can't turn a local target into another site's.
        let params = vec![("rest".to_string(), "/evil.example/x".to_string())];
        assert_eq!(redirect_location("/{*rest}", &params), "/evil.example/x");
        assert_eq!(
            redirect_location("/new/{*rest}", &params),
            "/new//evil.example/x"
        );
        // One the config spells out is left alone.
        assert_eq!(
            redirect_location("//cdn.example/{*rest}", &params),
            "//cdn.example//evil.example/x"
        );
        assert_eq!(decode_param("a%20b"), "a b");
        assert_eq!(
            query_pairs(Some("a=1&b=x%20y")),
            vec![("a".into(), "1".into()), ("b".into(), "x y".into())]
        );
    }

    #[test]
    fn header_rules_and_cors() {
        let mut eff = crate::policy::Layer::default().finalize(Duration::from_secs(1), 1);
        eff.response_headers.set = vec![("x-frame-options".into(), "DENY".into())];
        eff.response_headers.remove = vec!["server".into()];
        eff.cors = Some(Cors {
            origins: None,
            credentials: false,
            methods: None,
            headers: None,
            expose: vec![],
            max_age: None,
        });
        let mut headers = HeaderMap::new();
        headers.insert("server", "x".parse().unwrap());
        headers.insert("vary", "Accept".parse().unwrap());
        finish_headers(&eff, Some("https://a"), &mut headers);
        assert!(!headers.contains_key("server"));
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert_eq!(headers.get_all("vary").iter().count(), 2);
    }

    #[test]
    fn handler_heads() {
        let mut r = unsworn_proto::Response::new("1", 201);
        r.json = Some(serde_json::json!({}).into());
        let (status, headers) = response_head(&r).unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        let bad = unsworn_proto::Response::new("1", 1000);
        assert!(response_head(&bad).is_err());
    }

    #[test]
    fn content_length() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, "11".parse().unwrap());
        assert!(check_content_length(&headers, 10).is_err());
        assert!(check_content_length(&headers, 11).is_ok());
    }
}
