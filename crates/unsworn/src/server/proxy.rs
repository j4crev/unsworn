//! Reverse proxying, including WebSocket upgrades and SSE pass-through.

use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri, header};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use unsworn_core::project::Proxy;

use super::support::{Body, BoxError, HttpClient, problem};

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

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // Headers named in `Connection` are hop-by-hop too.
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

fn is_upgrade(headers: &HeaderMap) -> bool {
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

fn pick_upstream<'a>(proxy: &'a Proxy, headers: &HeaderMap) -> &'a str {
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

pub struct ForwardInfo {
    pub client_ip: IpAddr,
    pub tls: bool,
    pub request_id: String,
}

/// Forwards a request to one of the proxy's upstreams.
pub fn forward(
    client: HttpClient,
    proxy: std::sync::Arc<Proxy>,
    req: Request<Body>,
    info: ForwardInfo,
) -> futures_util::future::BoxFuture<'static, Response<Body>> {
    Box::pin(forward_inner(client, proxy, req, info))
}

async fn forward_inner(
    client: HttpClient,
    proxy: std::sync::Arc<Proxy>,
    mut req: Request<Body>,
    info: ForwardInfo,
) -> Response<Body> {
    let upstream = pick_upstream(&proxy, req.headers()).to_string();
    let upgrade = is_upgrade(req.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));

    let path_and_query = req
        .uri()
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
    let uri: Uri = match target.parse() {
        Ok(u) => u,
        Err(e) => {
            return problem(
                StatusCode::BAD_GATEWAY,
                format!("bad upstream URL {target}: {e}"),
            );
        }
    };

    let original_host = req.headers().get(header::HOST).cloned();
    let headers = req.headers_mut();
    let upgrade_value = headers.get(header::UPGRADE).cloned();
    strip_hop_by_hop(headers);
    if let Some(u) = upgrade_value.filter(|_| upgrade) {
        headers.insert(header::UPGRADE, u);
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
    let xff = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {}", info.client_ip),
        None => info.client_ip.to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&xff) {
        headers.insert("x-forwarded-for", v);
    }
    headers.insert(
        "x-forwarded-proto",
        HeaderValue::from_static(if info.tls { "https" } else { "http" }),
    );
    if let Some(h) = &original_host {
        headers.insert("x-forwarded-host", h.clone());
    }
    if let Ok(v) = HeaderValue::from_str(&info.request_id) {
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
    *req.uri_mut() = uri;
    *req.version_mut() = http::Version::HTTP_11;

    let mut resp = match client.request(req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(upstream = %upstream, "proxy error: {e}");
            return problem(
                StatusCode::BAD_GATEWAY,
                format!("the upstream at {upstream} couldn't be reached"),
            );
        }
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client_upgrade) = client_upgrade
    {
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            match tokio::try_join!(client_upgrade, upstream_upgrade) {
                Ok((client_io, upstream_io)) => {
                    let mut a = TokioIo::new(client_io);
                    let mut b = TokioIo::new(upstream_io);
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
                Err(e) => tracing::warn!("upgrade failed: {e}"),
            }
        });
        let (parts, _) = resp.into_parts();
        return Response::from_parts(parts, super::support::empty());
    }

    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    Response::from_parts(
        parts,
        body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync(),
    )
}
