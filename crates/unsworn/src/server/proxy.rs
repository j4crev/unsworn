//! Reverse proxying, including WebSocket upgrades and SSE pass-through.

use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use unsworn_core::pipeline::proxy::{ForwardInfo, is_upgrade, response_headers, rewrite};
use unsworn_core::project::Proxy;

use super::support::{Body, BoxError, HttpClient, Tunnel, problem, problem_response};

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
    let upgrade = is_upgrade(req.headers());
    let tunnel = req.extensions().get::<Tunnel>().cloned();
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let (mut parts, body) = req.into_parts();
    let upstream = match rewrite(&proxy, &mut parts, &info) {
        Ok(u) => u,
        Err(p) => return problem_response(p),
    };
    let req = Request::from_parts(parts, body);

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
    Response::from_parts(
        parts,
        body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync(),
    )
}
