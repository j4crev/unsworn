//! How a `proxy` route rewrites a request for its upstream: which upstream,
//! the target URL, and the forwarding headers. Hosts send the result.

use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::uri::PathAndQuery;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};

use super::Problem;
use crate::project::{Proxy, Target};

static ROUND_ROBIN: AtomicUsize = AtomicUsize::new(0);

static KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");

/// Removes hop-by-hop headers, including those named in `Connection`.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    if headers.contains_key(header::CONNECTION) {
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
        headers.remove(header::CONNECTION);
    }
    headers.remove(&KEEP_ALIVE);
    headers.remove(header::PROXY_AUTHENTICATE);
    headers.remove(header::PROXY_AUTHORIZATION);
    headers.remove(header::TE);
    headers.remove(header::TRAILER);
    headers.remove(header::TRANSFER_ENCODING);
    headers.remove(header::UPGRADE);
}

/// Adds Unsworn to `Via`, which every proxy does to each message it
/// forwards (RFC 9110 section 7.6.3). `version` is the protocol the message
/// was received in.
fn add_via(headers: &mut HeaderMap, version: http::Version) {
    let via = match version {
        http::Version::HTTP_10 => "1.0 unsworn",
        http::Version::HTTP_2 => "2 unsworn",
        http::Version::HTTP_3 => "3 unsworn",
        _ => "1.1 unsworn",
    };
    headers.append(header::VIA, HeaderValue::from_static(via));
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
    if !headers.contains_key(name) {
        return None;
    }
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// This hop's `Forwarded` element (RFC 7239), after `prev`, the elements
/// the request arrived with.
fn forwarded(prev: Option<String>, info: &ForwardInfo, host: Option<&HeaderValue>) -> String {
    use std::fmt::Write;
    let mut out = match prev {
        Some(mut prev) => {
            prev.push_str(", ");
            prev
        }
        None => String::with_capacity(96),
    };
    let _ = match info.client_ip {
        IpAddr::V4(ip) => write!(out, "for={ip}"),
        IpAddr::V6(ip) => write!(out, "for=\"[{ip}]\""),
    };
    out.push_str(if info.tls {
        ";proto=https"
    } else {
        ";proto=http"
    });
    if let Some(host) = host.and_then(|h| h.to_str().ok()) {
        out.push_str(";host=\"");
        if host.contains(['\\', '"']) {
            out.push_str(&host.replace('\\', "\\\\").replace('"', "\\\""));
        } else {
            out.push_str(host);
        }
        out.push('"');
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
    &proxy.upstreams[pick(proxy, headers)]
}

/// [`pick_upstream`]'s choice, as its place in `proxy.upstreams`.
fn pick(proxy: &Proxy, headers: &HeaderMap) -> usize {
    let n = proxy.upstreams.len();
    if n == 1 {
        return 0;
    }
    if let Some(name) = &proxy.sticky_header
        && let Some(value) = headers.get(name.as_str())
    {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        value.as_bytes().hash(&mut h);
        return (h.finish() as usize) % n;
    }
    ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) % n
}

/// Facts about the incoming request the forwarding headers carry.
#[derive(Debug, Clone)]
pub struct ForwardInfo {
    pub client_ip: IpAddr,
    /// The client connected over HTTPS.
    pub tls: bool,
    pub request_id: Option<HeaderValue>,
    /// The request came from a proxy Unsworn trusts (`[server].trusted_proxies`),
    /// so the forwarding headers it carries are kept and added to. Otherwise
    /// the client wrote them, and they're replaced.
    pub trusted_peer: bool,
}

static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
static X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
static X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
static X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");
static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Forwarding headers a proxy in front of Unsworn sets, which a client
/// could otherwise set to anything: an upstream that reads the leftmost
/// `X-Forwarded-For`, or `X-Real-IP`, would take the client's word for its
/// address.
static FROM_PROXIES: [HeaderName; 8] = [
    HeaderName::from_static("x-forwarded-for"),
    header::FORWARDED,
    HeaderName::from_static("x-forwarded-host"),
    HeaderName::from_static("x-forwarded-proto"),
    HeaderName::from_static("x-forwarded-port"),
    HeaderName::from_static("x-forwarded-prefix"),
    HeaderName::from_static("x-forwarded-ssl"),
    HeaderName::from_static("x-real-ip"),
];

/// Headers some frameworks (Symfony, IIS URL Rewrite) read as the request's
/// real path. Unsworn matched the request by its own path, so an upstream
/// that honored one of these could serve a route whose policies never ran.
static PATH_OVERRIDES: [HeaderName; 2] = [
    HeaderName::from_static("x-original-url"),
    HeaderName::from_static("x-rewrite-url"),
];

/// The request's path and query as the upstream should see them: without
/// the proxy's `strip_prefix`.
fn upstream_path(proxy: &Proxy, uri: &Uri) -> Result<Option<PathAndQuery>, http::Error> {
    let Some(path_and_query) = uri.path_and_query() else {
        return Ok(None);
    };
    match &proxy.strip_prefix {
        Some(prefix) if path_and_query.as_str().starts_with(prefix.as_str()) => {
            let rest = &path_and_query.as_str()[prefix.len()..];
            let rest = if rest.starts_with('/') {
                rest.to_string()
            } else {
                format!("/{rest}")
            };
            Ok(Some(PathAndQuery::try_from(rest)?))
        }
        _ => Ok(Some(path_and_query.clone())),
    }
}

/// The URL to send the request to: the upstream's, then the path and query.
/// An upstream on a Unix socket has no URL, so its requests name the path
/// alone.
fn target(proxy: &Proxy, index: usize, uri: &Uri) -> Result<Uri, http::Error> {
    let path_and_query = upstream_path(proxy, uri)?;
    match proxy.target(index) {
        // The parts go together as they are.
        Target::Origin(origin) => {
            let mut parts = http::uri::Parts::default();
            parts.scheme = Some(origin.scheme.clone());
            parts.authority = Some(origin.authority.clone());
            parts.path_and_query =
                Some(path_and_query.unwrap_or_else(|| PathAndQuery::from_static("/")));
            Ok(Uri::from_parts(parts)?)
        }
        // The upstream's own path goes in front.
        Target::Url => {
            let path = path_and_query.as_ref().map_or("/", PathAndQuery::as_str);
            Ok(format!("{}{path}", proxy.upstreams[index]).parse::<Uri>()?)
        }
        Target::Unix(_) => Ok(match path_and_query {
            Some(path_and_query) => Uri::from(path_and_query),
            None => Uri::from_static("/"),
        }),
    }
}

/// Rewrites a request for one of the proxy's upstreams: the target URI,
/// hop-by-hop headers removed (an upgrade's and `TE: trailers` kept),
/// `Forwarded`, `X-Forwarded-*`, `X-Real-IP`, `Via`, `X-Request-Id` and
/// `Host`. Forwarding headers from a peer that isn't a trusted proxy, and
/// path overrides such as `X-Original-URL`, are removed first. Returns
/// the place in `proxy.upstreams` of the upstream it picked, or `502` when
/// the target isn't a valid URL.
pub fn rewrite(
    proxy: &Proxy,
    parts: &mut http::request::Parts,
    info: &ForwardInfo,
) -> Result<usize, Problem> {
    let index = pick(proxy, &parts.headers);
    let upstream = proxy.upstreams[index].as_str();
    let upgrade = is_upgrade(&parts.headers);
    let uri = target(proxy, index, &parts.uri).map_err(|e| {
        Problem::new(
            StatusCode::BAD_GATEWAY,
            format!("bad upstream URL {upstream}{}: {e}", parts.uri.path()),
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
    for name in &PATH_OVERRIDES {
        headers.remove(name);
    }
    if !info.trusted_peer {
        for name in &FROM_PROXIES {
            headers.remove(name);
        }
    }
    if let Some(u) = upgrade_value.filter(|_| upgrade) {
        headers.insert(header::UPGRADE, u);
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
    if trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
    add_via(headers, parts.version);
    let client_ip = HeaderValue::try_from(info.client_ip.to_string()).ok();
    let xff = match joined(headers, &X_FORWARDED_FOR) {
        Some(prev) => HeaderValue::try_from(format!("{prev}, {}", info.client_ip)).ok(),
        None => client_ip.clone(),
    };
    if let Some(v) = xff {
        headers.insert(&X_FORWARDED_FOR, v);
    }
    let forwarded = forwarded(
        joined(headers, &header::FORWARDED),
        info,
        original_host.as_ref(),
    );
    if let Ok(v) = HeaderValue::try_from(forwarded) {
        headers.insert(header::FORWARDED, v);
    }
    headers.insert(
        &X_FORWARDED_PROTO,
        HeaderValue::from_static(if info.tls { "https" } else { "http" }),
    );
    if let Some(v) = client_ip {
        headers.insert(&X_REAL_IP, v);
    }
    if let Some(h) = original_host {
        headers.insert(&X_FORWARDED_HOST, h);
    }
    if let Some(id) = &info.request_id {
        headers.insert(&X_REQUEST_ID, id.clone());
    }
    if !proxy.preserve_host {
        let host = match proxy.target(index) {
            Target::Origin(origin) => Some(origin.host.clone()),
            Target::Url => uri
                .authority()
                .and_then(|a| HeaderValue::from_str(a.as_str()).ok()),
            // A socket has no name of its own. This is what nginx sends.
            Target::Unix(_) => Some(HeaderValue::from_static("localhost")),
        };
        match host {
            Some(host) => {
                headers.insert(header::HOST, host);
            }
            None => {
                headers.remove(header::HOST);
            }
        }
    }
    parts.uri = uri;
    parts.version = http::Version::HTTP_11;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(upstreams: &[&str]) -> Proxy {
        Proxy::new(
            upstreams.iter().map(|u| u.to_string()).collect(),
            Some("/api".into()),
            Some("x-user".into()),
            false,
        )
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
            request_id: Some(HeaderValue::from_static("abc")),
            trusted_peer: true,
        };
        assert_eq!(rewrite(&p, &mut req, &info).unwrap(), 0);
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
            trusted_peer: true,
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
    fn replaces_what_an_untrusted_client_sent() {
        let p = proxy(&["http://up:8080"]);
        let mut req = parts(
            "/api/users",
            &[
                ("host", "example.com"),
                ("x-forwarded-for", "127.0.0.1"),
                ("forwarded", "for=127.0.0.1"),
                ("x-real-ip", "127.0.0.1"),
                ("x-forwarded-port", "443"),
                ("x-forwarded-prefix", "/admin"),
                ("x-original-url", "/admin/users"),
                ("x-rewrite-url", "/admin/users"),
            ],
        );
        let info = ForwardInfo {
            client_ip: "198.51.100.4".parse().unwrap(),
            tls: true,
            request_id: None,
            trusted_peer: false,
        };
        rewrite(&p, &mut req, &info).unwrap();
        let h = &req.headers;
        assert_eq!(h["x-forwarded-for"], "198.51.100.4");
        assert_eq!(h["x-real-ip"], "198.51.100.4");
        assert_eq!(
            h["forwarded"],
            "for=198.51.100.4;proto=https;host=\"example.com\""
        );
        for gone in [
            "x-forwarded-port",
            "x-forwarded-prefix",
            "x-original-url",
            "x-rewrite-url",
        ] {
            assert!(!h.contains_key(gone), "{gone}");
        }

        // A trusted proxy's headers are kept, but a path override never is.
        let mut req = parts(
            "/api/users",
            &[("x-forwarded-prefix", "/v1"), ("x-original-url", "/admin")],
        );
        let info = ForwardInfo {
            trusted_peer: true,
            ..info
        };
        rewrite(&p, &mut req, &info).unwrap();
        assert_eq!(req.headers["x-forwarded-prefix"], "/v1");
        assert!(!req.headers.contains_key("x-original-url"));
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
            trusted_peer: false,
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
            trusted_peer: false,
        };
        assert_eq!(
            rewrite(&p, &mut req, &info).unwrap_err().status,
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn an_upstream_with_a_path_keeps_it_in_front() {
        let p = Proxy::new(vec!["http://up:8080/base".into()], None, None, false);
        let mut req = parts("/users?x=1", &[("host", "example.com")]);
        let info = ForwardInfo {
            client_ip: "192.0.2.7".parse().unwrap(),
            tls: false,
            request_id: None,
            trusted_peer: false,
        };
        rewrite(&p, &mut req, &info).unwrap();
        assert_eq!(req.uri, "http://up:8080/base/users?x=1");
        assert_eq!(req.headers["host"], "up:8080");
    }

    #[test]
    fn a_unix_socket_upstream_gets_the_path_alone() {
        let p = Proxy::new(
            vec!["unix:/run/app.sock".into()],
            Some("/api".into()),
            None,
            false,
        );
        assert!(matches!(p.target(0), Target::Unix(path) if &**path == "/run/app.sock"));
        let mut req = parts("/api/users?x=1", &[("host", "example.com")]);
        let info = ForwardInfo {
            client_ip: "192.0.2.7".parse().unwrap(),
            tls: false,
            request_id: None,
            trusted_peer: false,
        };
        assert_eq!(rewrite(&p, &mut req, &info).unwrap(), 0);
        assert_eq!(req.uri, "/users?x=1");
        assert_eq!(req.headers["host"], "localhost");
        assert_eq!(req.headers["x-forwarded-host"], "example.com");

        let p = Proxy::new(vec!["unix:/run/app.sock".into()], None, None, true);
        let mut req = parts("/users", &[("host", "example.com")]);
        rewrite(&p, &mut req, &info).unwrap();
        assert_eq!(req.headers["host"], "example.com");
    }
}
