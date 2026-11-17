//! Reverse proxying, including WebSocket upgrades and SSE pass-through.

use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use unsworn_core::pipeline::proxy::{ForwardInfo, is_upgrade, response_headers, rewrite};
use unsworn_core::project::{Proxy, Target};

use super::Shared;
use super::support::{Body, BoxError, Tunnel, problem, problem_response};
use super::upstream::Peer;

/// Forwards a request to one of the proxy's upstreams.
pub fn forward(
    shared: std::sync::Arc<Shared>,
    proxy: std::sync::Arc<Proxy>,
    req: Request<Body>,
    info: ForwardInfo,
) -> futures_util::future::BoxFuture<'static, Response<Body>> {
    Box::pin(forward_inner(shared, proxy, req, info))
}

async fn forward_inner(
    shared: std::sync::Arc<Shared>,
    proxy: std::sync::Arc<Proxy>,
    mut req: Request<Body>,
    info: ForwardInfo,
) -> Response<Body> {
    let upgrade = is_upgrade(req.headers());
    let tunnel = req.extensions().get::<Tunnel>().cloned();
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let (mut parts, body) = req.into_parts();
    let index = match rewrite(&proxy, &mut parts, &info) {
        Ok(index) => index,
        Err(p) => return problem_response(p),
    };
    let upstream = &proxy.upstreams[index];
    let req = Request::from_parts(parts, body);

    // Plain HTTP/1.1 goes over a connection this thread keeps. The general
    // client has TLS and HTTP/2.
    let peer = match proxy.target(index) {
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
    Response::from_parts(parts, body)
}
