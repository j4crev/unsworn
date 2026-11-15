//! How a `proxy` route rewrites a request for its upstream: which upstream,
//! the target URL, and the forwarding headers. Hosts send the result.

use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};

use super::Problem;
use crate::project::Proxy;

static ROUND_ROBIN: AtomicUsize = AtomicUsize::new(0);

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Removes hop-by-hop headers, including those named in `Connection`.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::try_from(n.trim()).ok())
        .collect();
    for n in named {
        headers.remove(n);
    }
    for h in HOP_BY_HOP {
        headers.remove(*h);
    }
}

/// The name Unsworn goes by in `Via`.
const VIA_NAME: &str = "unsworn";

/// Adds Unsworn to `Via`, which every proxy does to each message it
/// forwards (RFC 9110 section 7.6.3). `version` is the protocol the message
/// was received in.
fn add_via(headers: &mut HeaderMap, version: http::Version) {
    let protocol = match version {
        http::Version::HTTP_10 => "1.0",
        http::Version::HTTP_2 => "2",
        http::Version::HTTP_3 => "3",
        _ => "1.1",
    };
    if let Ok(v) = HeaderValue::from_str(&format!("{protocol} {VIA_NAME}")) {
        headers.append(header::VIA, v);
    }
}

/// Prepares an upstream's response headers for the client: hop-by-hop
/// headers removed and `Via` added. `version` is the protocol the upstream
/// answered in.
pub fn response_headers(headers: &mut HeaderMap, version: http::Version) {
    strip_hop_by_hop(headers);
    add_via(headers, version);
}

/// All values of a list header, as one value.
fn joined(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// This hop's `Forwarded` element (RFC 7239).
fn forwarded_element(info: &ForwardInfo, host: Option<&HeaderValue>) -> String {
    let mut out = match info.client_ip {
        IpAddr::V4(ip) => format!("for={ip}"),
        IpAddr::V6(ip) => format!("for=\"[{ip}]\""),
    };
    out.push_str(if info.tls {
        ";proto=https"
    } else {
        ";proto=http"
    });
    if let Some(host) = host.and_then(|h| h.to_str().ok()) {
        let quoted = host.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!(";host=\"{quoted}\""));
    }
    out
}

/// Whether the request asks to switch protocols, such as to a WebSocket.
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
}

/// The upstream for a request: pinned by `sticky_header` when the request
/// has it, round-robin otherwise.
pub fn pick_upstream<'a>(proxy: &'a Proxy, headers: &HeaderMap) -> &'a str {
    let n = proxy.upstreams.len();
    if n == 1 {
        return &proxy.upstreams[0];
    }
    if let Some(name) = &proxy.sticky_header
        && let Some(value) = headers.get(name.as_str())
    {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        value.as_bytes().hash(&mut h);
        return &proxy.upstreams[(h.finish() as usize) % n];
    }
    &proxy.upstreams[ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) % n]
}

/// Facts about the incoming request the forwarding headers carry.
#[derive(Debug, Clone)]
pub struct ForwardInfo {
    pub client_ip: IpAddr,
    /// The client connected over HTTPS.
    pub tls: bool,
    pub request_id: Option<String>,
}

/// Rewrites a request for one of the proxy's upstreams: the target URI,
/// hop-by-hop headers removed (an upgrade's and `TE: trailers` kept),
/// `Forwarded`, `X-Forwarded-*`, `Via`, `X-Request-Id` and `Host`. Returns
/// the upstream it picked, or `502` when the target isn't a valid URL.
pub fn rewrite(
    proxy: &Proxy,
    parts: &mut http::request::Parts,
    info: &ForwardInfo,
) -> Result<String, Problem> {
    let upstream = pick_upstream(proxy, &parts.headers).to_string();
    let upgrade = is_upgrade(&parts.headers);

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let path_and_query = match &proxy.strip_prefix {
        Some(prefix) if path_and_query.starts_with(prefix.as_str()) => {
            let rest = &path_and_query[prefix.len()..];
            if rest.starts_with('/') {
                rest.to_string()
            } else {
                format!("/{rest}")
            }
        }
        _ => path_and_query,
    };
    let target = format!("{upstream}{path_and_query}");
    let uri: Uri = target.parse().map_err(|e| {
        Problem::new(
            StatusCode::BAD_GATEWAY,
            format!("bad upstream URL {target}: {e}"),
        )
    })?;

    let headers = &mut parts.headers;
    let original_host = headers.get(header::HOST).cloned();
    let upgrade_value = headers.get(header::UPGRADE).cloned();
    // The one `TE` value that says something about this hop too: Unsworn
    // passes trailers on, and gRPC needs to hear that.
    let trailers = headers
        .get_all(header::TE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case("trailers"));
    strip_hop_by_hop(headers);
    if let Some(u) = upgrade_value.filter(|_| upgrade) {
        headers.insert(header::UPGRADE, u);
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
    if trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
    add_via(headers, parts.version);
    let xff = match joined(headers, &HeaderName::from_static("x-forwarded-for")) {
        Some(prev) => format!("{prev}, {}", info.client_ip),
        None => info.client_ip.to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&xff) {
        headers.insert("x-forwarded-for", v);
    }
    let element = forwarded_element(info, original_host.as_ref());
    let forwarded = match joined(headers, &header::FORWARDED) {
        Some(prev) => format!("{prev}, {element}"),
        None => element,
    };
    if let Ok(v) = HeaderValue::from_str(&forwarded) {
        headers.insert(header::FORWARDED, v);
    }
    headers.insert(
        "x-forwarded-proto",
        HeaderValue::from_static(if info.tls { "https" } else { "http" }),
    );
    if let Some(h) = original_host {
        headers.insert("x-forwarded-host", h);
    }
    if let Some(id) = &info.request_id
        && let Ok(v) = HeaderValue::from_str(id)
    {
        headers.insert("x-request-id", v);
    }
    if !proxy.preserve_host {
        headers.remove(header::HOST);
        if let Some(authority) = uri.authority()
            && let Ok(v) = HeaderValue::from_str(authority.as_str())
        {
            headers.insert(header::HOST, v);
        }
    }
    parts.uri = uri;
    parts.version = http::Version::HTTP_11;
    Ok(upstream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(upstreams: &[&str]) -> Proxy {
        Proxy {
            upstreams: upstreams.iter().map(|u| u.to_string()).collect(),
            strip_prefix: Some("/api".into()),
            sticky_header: Some("x-user".into()),
            preserve_host: false,
        }
    }

    fn parts(uri: &str, headers: &[(&str, &str)]) -> http::request::Parts {
        let mut b = http::Request::builder().uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap().into_parts().0
    }

    #[test]
    fn rewrites_for_the_upstream() {
        let p = proxy(&["http://up:8080"]);
        let mut req = parts(
            "/api/users?x=1",
            &[
                ("host", "example.com"),
                ("connection", "keep-alive, x-secret"),
                ("x-secret", "1"),
                ("keep-alive", "timeout=5"),
                ("x-forwarded-for", "10.0.0.1"),
            ],
        );
        let info = ForwardInfo {
            client_ip: "192.0.2.7".parse().unwrap(),
            tls: true,
            request_id: Some("abc".into()),
        };
        assert_eq!(rewrite(&p, &mut req, &info).unwrap(), "http://up:8080");
        assert_eq!(req.uri, "http://up:8080/users?x=1");
        let h = &req.headers;
        assert_eq!(h["host"], "up:8080");
        assert_eq!(h["x-forwarded-host"], "example.com");
        assert_eq!(h["x-forwarded-for"], "10.0.0.1, 192.0.2.7");
        assert_eq!(h["x-forwarded-proto"], "https");
        assert_eq!(h["x-request-id"], "abc");
        assert!(!h.contains_key("x-secret"));
        assert!(!h.contains_key("keep-alive"));
        assert!(!h.contains_key("connection"));
        assert_eq!(
            h["forwarded"],
            "for=192.0.2.7;proto=https;host=\"example.com\""
        );
        assert_eq!(h["via"], "1.1 unsworn");
    }

    #[test]
    fn adds_to_what_earlier_hops_sent() {
        let p = proxy(&["http://up:8080"]);
        let mut req = parts(
            "/api/users",
            &[
                ("host", "example.com:8443"),
                ("x-forwarded-for", "10.0.0.1"),
                ("x-forwarded-for", "10.0.0.2"),
                ("forwarded", "for=10.0.0.1"),
                ("via", "1.1 edge"),
                ("te", "trailers, deflate"),
            ],
        );
        req.version = http::Version::HTTP_2;
        let info = ForwardInfo {
            client_ip: "2001:db8::7".parse().unwrap(),
            tls: false,
            request_id: None,
        };
        rewrite(&p, &mut req, &info).unwrap();
        let h = &req.headers;
        assert_eq!(h["x-forwarded-for"], "10.0.0.1, 10.0.0.2, 2001:db8::7");
        assert_eq!(h.get_all("x-forwarded-for").iter().count(), 1);
        assert_eq!(
            h["forwarded"],
            "for=10.0.0.1, for=\"[2001:db8::7]\";proto=http;host=\"example.com:8443\""
        );
        let via: Vec<_> = h.get_all("via").iter().collect();
        assert_eq!(via, ["1.1 edge", "2 unsworn"]);
        assert_eq!(h["te"], "trailers");

        let mut resp = HeaderMap::new();
        resp.insert("connection", "close".parse().unwrap());
        resp.insert("via", "1.1 origin-cache".parse().unwrap());
        response_headers(&mut resp, http::Version::HTTP_11);
        assert!(!resp.contains_key("connection"));
        let via: Vec<_> = resp.get_all("via").iter().collect();
        assert_eq!(via, ["1.1 origin-cache", "1.1 unsworn"]);
    }

    #[test]
    fn keeps_an_upgrade_and_the_host_when_asked() {
        let mut p = proxy(&["http://up"]);
        p.preserve_host = true;
        let mut req = parts(
            "/api",
            &[
                ("host", "example.com"),
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
            ],
        );
        let info = ForwardInfo {
            client_ip: "::1".parse().unwrap(),
            tls: false,
            request_id: None,
        };
        rewrite(&p, &mut req, &info).unwrap();
        assert_eq!(req.uri, "http://up/");
        assert_eq!(req.headers["host"], "example.com");
        assert_eq!(req.headers["upgrade"], "websocket");
        assert_eq!(req.headers["connection"], "upgrade");
        assert!(!req.headers.contains_key("x-request-id"));
    }

    #[test]
    fn sticky_header_pins_an_upstream() {
        let p = proxy(&["http://a", "http://b", "http://c"]);
        let h = parts("/", &[("x-user", "42")]).headers;
        let first = pick_upstream(&p, &h);
        for _ in 0..5 {
            assert_eq!(pick_upstream(&p, &h), first);
        }
        let none = HeaderMap::new();
        let picks: std::collections::HashSet<_> =
            (0..3).map(|_| pick_upstream(&p, &none)).collect();
        assert_eq!(picks.len(), 3);
    }

    #[test]
    fn bad_upstream_is_a_502() {
        let p = proxy(&["not a url"]);
        let mut req = parts("/api/x", &[]);
        let info = ForwardInfo {
            client_ip: "::1".parse().unwrap(),
            tls: false,
            request_id: None,
        };
        assert_eq!(
            rewrite(&p, &mut req, &info).unwrap_err().status,
            StatusCode::BAD_GATEWAY
        );
    }
}
