//! The checks and conversions a request goes through, as pure functions.

use std::collections::BTreeMap;
use std::net::IpAddr;

use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use serde_json::{Map, Value};

use super::Problem;
use crate::policy::{CacheRule, Cors, Effective, IpRules, KeySource, RateLimit};
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
    if let Some(rules) = &eff.ip
        && !ip_allowed(rules, client_ip)
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

/// Parses a JSON body and validates it against the route's body schema.
/// Returns the parsed JSON, if the body is JSON.
pub fn check_body(
    route: &Route,
    headers: &HeaderMap,
    body: Option<&[u8]>,
) -> Result<Option<Value>, Problem> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_json = content_type.starts_with("application/json") || content_type.contains("+json");
    let empty_body = body.is_none_or(<[u8]>::is_empty);
    let mut json_body = None;
    if let Some(bytes) = body
        && !empty_body
        && is_json
    {
        match serde_json::from_slice::<Value>(bytes) {
            Ok(v) => json_body = Some(v),
            Err(e) => {
                return Err(Problem::new(
                    StatusCode::BAD_REQUEST,
                    format!("the body isn't valid JSON: {e}"),
                ));
            }
        }
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
    Ok(json_body)
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

/// The rate-limit bucket a request counts against. `route_id` keeps each
/// route's (and each MCP tool's) counts separate. A key the request lacks
/// falls back to the client's IP.
pub fn rate_limit_bucket(
    route_id: usize,
    limit: &RateLimit,
    client_ip: IpAddr,
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
) -> String {
    let key =
        key_value(&limit.key, client_ip, headers, claims).unwrap_or_else(|| client_ip.to_string());
    format!("{route_id}|{}|{key}", limit.key.describe())
}

/// The cache key for a request, or `None` when it mustn't be cached: a
/// method other than `GET` or `HEAD`, a protocol upgrade such as a
/// WebSocket, or an authenticated request whose key doesn't separate users.
#[allow(clippy::too_many_arguments)]
pub fn cache_key(
    rule: &CacheRule,
    credential_headers: &[String],
    route_id: usize,
    method: &str,
    uri: &Uri,
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
    client_ip: IpAddr,
) -> Option<String> {
    if !rules::cache_method(method) || super::proxy::is_upgrade(headers) {
        return None;
    }
    if claims.is_some() && !rules::cache_separates_users(rule, credential_headers) {
        return None;
    }
    let vary: Vec<(String, Option<String>)> = rule
        .vary
        .iter()
        .map(|h| {
            let value = headers
                .get(h.as_str())
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            (h.clone(), value)
        })
        .collect();
    let key = rule
        .key
        .as_ref()
        .map(|k| key_value(k, client_ip, headers, claims).unwrap_or_default());
    Some(rules::cache_key(
        route_id,
        method,
        uri.path(),
        uri.query(),
        &vary,
        key.as_deref(),
    ))
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
    for name in &eff.headers.remove {
        headers.remove(name.as_str());
    }
    for (name, value) in &eff.headers.set {
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

/// A `redirect` route's target, with path parameters filled in.
pub fn redirect_location(to: &str, raw_params: &[(String, String)]) -> String {
    let mut location = to.to_string();
    for (k, v) in raw_params {
        location = location
            .replace(&format!("{{*{k}}}"), v)
            .replace(&format!("{{{k}}}"), v);
    }
    location
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
        };
        assert_eq!(
            rate_limit_bucket(3, &limit(KeySource::Ip), client, &headers, None),
            "3|ip|192.0.2.1"
        );
        assert_eq!(
            rate_limit_bucket(
                3,
                &limit(KeySource::Header("x-tenant".into())),
                client,
                &headers,
                None
            ),
            "3|header.x-tenant|acme"
        );
        // Missing keys fall back to the IP.
        assert_eq!(
            rate_limit_bucket(
                3,
                &limit(KeySource::Claim("sub".into())),
                client,
                &headers,
                None
            ),
            "3|auth.sub|192.0.2.1"
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
        assert_eq!(
            cache_key(&rule, &[], 1, "HEAD", &uri, &headers, None, client).as_deref(),
            Some("1|GET|/a?b=1|accept=")
        );
        assert_eq!(
            cache_key(&rule, &[], 1, "POST", &uri, &headers, None, client),
            None
        );
        // Authenticated, and the key doesn't separate users.
        assert_eq!(
            cache_key(&rule, &[], 1, "GET", &uri, &headers, Some(&claims), client),
            None
        );
        // A WebSocket upgrade never gets a cached response.
        let mut upgrade = headers.clone();
        upgrade.insert("connection", "Upgrade".parse().unwrap());
        upgrade.insert("upgrade", "websocket".parse().unwrap());
        assert_eq!(
            cache_key(&rule, &[], 1, "GET", &uri, &upgrade, None, client),
            None
        );
    }

    #[test]
    fn redirects_and_decoding() {
        let params = vec![("id".to_string(), "7".to_string())];
        assert_eq!(redirect_location("/v2/{id}", &params), "/v2/7");
        assert_eq!(redirect_location("/files/{*id}", &params), "/files/7");
        assert_eq!(decode_param("a%20b"), "a b");
        assert_eq!(
            query_pairs(Some("a=1&b=x%20y")),
            vec![("a".into(), "1".into()), ("b".into(), "x y".into())]
        );
    }

    #[test]
    fn header_rules_and_cors() {
        let mut eff = crate::policy::Layer::default().finalize(Duration::from_secs(1), 1);
        eff.headers.set = vec![("x-frame-options".into(), "DENY".into())];
        eff.headers.remove = vec!["server".into()];
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
        r.json = Some(serde_json::json!({}));
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
