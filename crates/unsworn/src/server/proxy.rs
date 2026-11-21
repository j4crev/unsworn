//! Reverse proxying, including WebSocket upgrades and SSE pass-through.

use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use unsworn_core::pipeline::proxy::{
    CALL_TOKEN, ForwardInfo, is_upgrade, response_headers, rewrite, rewrite_to,
};
use unsworn_core::project::{Proxy, Target};

use super::Shared;
use super::support::{Body, BoxError, Tunnel, problem, problem_response};
use super::upstream::Peer;

/// How long a request waits for an `http` runtime's server, which may be
/// starting for the first time or again after it stopped.
const RUNTIME_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What a function an `http` runtime's server calls for a request starts
/// from.
pub struct ForCalls {
    pub context: serde_json::Map<String, serde_json::Value>,
    pub deadline: std::time::Instant,
}

/// Forwards a request to one of the proxy's upstreams.
pub fn forward(
    shared: std::sync::Arc<Shared>,
    proxy: std::sync::Arc<Proxy>,
    req: Request<Body>,
    info: ForwardInfo,
    for_calls: ForCalls,
) -> futures_util::future::BoxFuture<'static, Response<Body>> {
    Box::pin(forward_inner(shared, proxy, req, info, for_calls))
}

async fn forward_inner(
    shared: std::sync::Arc<Shared>,
    proxy: std::sync::Arc<Proxy>,
    mut req: Request<Body>,
    info: ForwardInfo,
    for_calls: ForCalls,
) -> Response<Body> {
    let upgrade = is_upgrade(req.headers());
    let tunnel = req.extensions().get::<Tunnel>().cloned();
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let (mut parts, body) = req.into_parts();
    // An `http` runtime's server is wherever its pool started it, and
    // counts as busy with this request until it has answered.
    let mut token = None;
    let lease = match &proxy.runtime {
        None => None,
        Some(name) => {
            let pools = shared.generation.load().pools.clone();
            let Some(pool) = pools.get(name) else {
                return problem(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the route's runtime is unavailable",
                );
            };
            match pool.http_lease(RUNTIME_WAIT).await {
                Ok(lease) => {
                    // The server calls functions and outbound services for
                    // this request with this, until its response ends.
                    let request_id = info.request_id.as_ref();
                    let request_id = request_id.and_then(|v| v.to_str().ok());
                    token = pool.call_token(
                        request_id.unwrap_or_default().to_string(),
                        for_calls.context,
                        for_calls.deadline,
                        &parts.headers,
                    );
                    if let Some(token) = &token {
                        parts.headers.insert(&CALL_TOKEN, token.header());
                    }
                    Some(lease)
                }
                Err(e) => {
                    tracing::error!(runtime = %name, "{e}");
                    return problem(
                        StatusCode::SERVICE_UNAVAILABLE,
                        match shared.dev {
                            true => format!("the route's runtime is unavailable: {e}"),
                            false => "the route's runtime is unavailable".to_string(),
                        },
                    );
                }
            }
        }
    };
    let rewritten = match &lease {
        Some(lease) => rewrite_to(&proxy, lease.target(), lease.upstream(), &mut parts, &info)
            .map(|()| (lease.target(), lease.upstream())),
        None => rewrite(&proxy, &mut parts, &info)
            .map(|index| (proxy.target(index), proxy.upstreams[index].as_str())),
    };
    let (to, upstream) = match rewritten {
        Ok(found) => found,
        Err(p) => return problem_response(p),
    };
    let req = Request::from_parts(parts, body);

    // Plain HTTP/1.1 goes over a connection this thread keeps. The general
    // client has TLS and HTTP/2.
    let peer = match to {
        Target::Unix(path) => Some(Peer::Unix(path.clone())),
        _ if req.uri().scheme() == Some(&http::uri::Scheme::HTTP) => {
            req.uri().authority().cloned().map(Peer::Tcp)
        }
        _ => None,
    };
    let sent = match peer {
        Some(peer) => shared.upstreams.send(peer, req).await,
        None => match shared.client.request(req).await {
            Ok(resp) => Ok(resp.map(|body| body.map_err(BoxError::from).boxed_unsync())),
            Err(e) => Err(BoxError::from(e)),
        },
    };
    let mut resp = match sent {
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
                    let (a, b) = (TokioIo::new(client_io), TokioIo::new(upstream_io));
                    match tunnel {
                        Some(tunnel) => tunnel.run(a, b).await,
                        // Only a request that didn't come through the
                        // listener (none today) lacks one.
                        None => {
                            let (mut a, mut b) = (a, b);
                            let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                        }
                    }
                }
                Err(e) => tracing::warn!("upgrade failed: {e}"),
            }
        });
        let (parts, _) = resp.into_parts();
        return Response::from_parts(parts, super::support::empty());
    }

    let (mut parts, body) = resp.into_parts();
    response_headers(&mut parts.headers, parts.version);
    // The token works while the response is being sent, and ends with it:
    // a server may call while it streams.
    let body = match token {
        Some(token) => body
            .map_frame(move |frame| {
                let _ = &token;
                frame
            })
            .boxed_unsync(),
        None => body,
    };
    Response::from_parts(parts, body)
}
