//! The local endpoint an `http` runtime's server calls functions and
//! outbound services through: what `ctx.call` and `ctx.fetch` are for a
//! handler, as plain HTTP, so a server needs no SDK to use them.
//!
//! Each request Unsworn forwards to the server carries a token in the
//! `Unsworn-Call` header. The server sends it back with each call, and
//! Unsworn finds the request it's for: the context the function gets,
//! the time left, the trace. A token stops working when its request's
//! response ends, and the calls still running for it are cancelled.
//!
//! The endpoint listens on a Unix socket in Unsworn's private directory
//! and on a port on `127.0.0.1`, since not every HTTP client can use a
//! socket. Anything on the machine can reach the port, so the token is
//! what lets a call in.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use unsworn_core::pipeline::Problem;
use unsworn_core::pipeline::proxy::{CALL_TOKEN, CONTEXT, strip_hop_by_hop};
use unsworn_core::trace::TraceContext;

use super::{Caller, Pool};
use crate::server::support::{Body, BoxError, full};

/// The header on an answer Unsworn made itself, not the function or the
/// service: the call couldn't be made, and the value says why.
const ERROR: http::HeaderName = http::HeaderName::from_static("unsworn-error");

/// The largest input a function call may send.
const MAX_INPUT: usize = unsworn_proto::DEFAULT_MAX_FRAME;

/// Where a pool's endpoint listens. Stops listening when dropped.
pub(super) struct Endpoint {
    pub socket: PathBuf,
    pub url: String,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Endpoint {
    /// Listens for `pool`'s servers on `socket` and on a free port.
    pub fn start(pool: &Arc<Pool>, socket: PathBuf) -> std::io::Result<Endpoint> {
        let _ = std::fs::remove_file(&socket);
        let unix = tokio::net::UnixListener::bind(&socket)?;
        let tcp = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        tcp.set_nonblocking(true)?;
        let tcp = tokio::net::TcpListener::from_std(tcp)?;
        let url = format!("http://{}", tcp.local_addr()?);

        let weak = Arc::downgrade(pool);
        let on_socket = tokio::spawn({
            let pool = weak.clone();
            async move {
                while let Ok((io, _)) = unix.accept().await {
                    serve(io, pool.clone());
                }
            }
        });
        let on_port = tokio::spawn(async move {
            while let Ok((io, _)) = tcp.accept().await {
                let _ = io.set_nodelay(true);
                serve(io, weak.clone());
            }
        });
        Ok(Endpoint {
            socket,
            url,
            tasks: vec![on_socket.abort_handle(), on_port.abort_handle()],
        })
    }
}

/// A request in flight that a server may call for.
pub(super) struct Entry {
    caller: Caller,
    /// Changes (with an error) when the request ends.
    ended: watch::Receiver<()>,
}

/// One forwarded request's token. The token works until this is dropped,
/// which cancels the calls still running for it.
pub struct CallToken {
    pool: Weak<Pool>,
    token: String,
    _ended: watch::Sender<()>,
}

impl CallToken {
    pub fn header(&self) -> HeaderValue {
        HeaderValue::from_str(&self.token).expect("a token is hex")
    }
}

impl Drop for CallToken {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.http_callers.lock().unwrap().remove(&self.token);
        }
    }
}

impl Pool {
    /// A token for a request forwarded to this `http` runtime's server, to
    /// call functions and outbound services with while it's in flight.
    /// `None` when the project has neither, so there's nothing to call.
    pub fn call_token(
        self: &Arc<Self>,
        request_id: String,
        context: serde_json::Map<String, Value>,
        deadline: std::time::Instant,
        headers: &http::HeaderMap,
    ) -> Option<CallToken> {
        self.peers.get()?;
        let token = random_token()?;
        let (tx, rx) = watch::channel(());
        let caller = Caller {
            request_id,
            // What a function sees as `ctx.caller.from`.
            handler: format!("runtime:{}", self.name),
            context,
            deadline,
            trace: TraceContext::from_headers(|name| {
                headers.get(name).and_then(|v| v.to_str().ok())
            }),
        };
        self.http_callers
            .lock()
            .unwrap()
            .insert(token.clone(), Entry { caller, ended: rx });
        Some(CallToken {
            pool: Arc::downgrade(self),
            token,
            _ended: tx,
        })
    }

    /// The endpoint for this pool's servers, started the first time one
    /// asks. `None` if it couldn't listen, which is logged.
    pub(super) fn endpoint(self: &Arc<Self>) -> Option<&Endpoint> {
        self.endpoint
            .get_or_init(|| {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let socket = self
                    .opts
                    .socket_dir
                    .join(format!("{}-calls-{n}.sock", self.name));
                Endpoint::start(self, socket)
                    .inspect_err(|e| {
                        tracing::error!(
                            runtime = %self.name,
                            "can't listen for calls from the runtime's server: {e}"
                        );
                    })
                    .ok()
            })
            .as_ref()
    }
}

/// 128 random bits, in hex. `None` only if the system has no randomness
/// to give.
fn random_token() -> Option<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn serve<IO>(io: IO, pool: Weak<Pool>)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let service = hyper::service::service_fn(move |req| {
            let pool = pool.clone();
            async move { Ok::<_, Infallible>(handle(pool, req).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(io), service)
            .await;
    });
}

/// An answer Unsworn makes itself: a problem document, marked with
/// `Unsworn-Error` so a server can tell it from the function's or the
/// service's own answer with the same status.
fn failure(status: u16, detail: &str, extra: Value) -> Response<Body> {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut body = Problem::new(status, detail).body();
    if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) {
        body.extend(extra);
    }
    let mut resp = Response::new(full(body.to_string()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(Problem::CONTENT_TYPE),
    );
    let why = HeaderValue::from_str(detail).unwrap_or(HeaderValue::from_static("1"));
    resp.headers_mut().insert(ERROR, why);
    resp
}

async fn handle(pool: Weak<Pool>, req: Request<Incoming>) -> Response<Body> {
    let Some(pool) = pool.upgrade() else {
        return failure(503, "the runtime is shutting down", Value::Null);
    };
    let Some(pools) = pool.peers.get().and_then(Weak::upgrade) else {
        return failure(
            404,
            "this project has no functions or outbound services",
            Value::Null,
        );
    };
    let Some(token) = req.headers().get(&CALL_TOKEN) else {
        return failure(
            401,
            "send the Unsworn-Call header of the request this call is made for",
            Value::Null,
        );
    };
    let found = token.to_str().ok().and_then(|token| {
        let callers = pool.http_callers.lock().unwrap();
        callers
            .get(token)
            .map(|entry| (entry.caller.clone(), entry.ended.clone()))
    });
    let Some((caller, mut ended)) = found else {
        return failure(
            409,
            "the Unsworn-Call token isn't one for a request in flight",
            Value::Null,
        );
    };

    let path = req.uri().path().to_string();
    let work = async {
        if let Some(name) = path.strip_prefix("/call/") {
            call(&pools, name.to_string(), req, caller).await
        } else if let Some(rest) = path.strip_prefix("/fetch/") {
            let (service, rest) = rest.split_once('/').unwrap_or((rest, ""));
            let mut path = format!("/{rest}");
            if let Some(query) = req.uri().query() {
                path = format!("{path}?{query}");
            }
            fetch(&pools, service.to_string(), path, req, &caller).await
        } else {
            failure(
                404,
                "the endpoint has POST /call/{function} and /fetch/{service}/{path}",
                Value::Null,
            )
        }
    };
    tokio::select! {
        resp = work => resp,
        // The request's response is over: what's still running for it is
        // of no use to anyone.
        _ = ended.changed() => failure(
            409,
            "the request this call was made for has ended",
            Value::Null,
        ),
    }
}

/// `POST /call/{function}`: the body is the input, and the answer is the
/// function's status and JSON.
async fn call(
    pools: &super::Pools,
    name: String,
    req: Request<Incoming>,
    caller: Caller,
) -> Response<Body> {
    if req.method() != Method::POST {
        let mut resp = failure(405, "a function is called with POST", Value::Null);
        resp.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return resp;
    }
    let body = match http_body_util::Limited::new(req.into_body(), MAX_INPUT)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            return failure(413, "the input is too large", Value::Null);
        }
        Err(e) => {
            return failure(
                400,
                &format!("the input couldn't be read: {e}"),
                Value::Null,
            );
        }
    };
    let input = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(input) => input,
            Err(e) => {
                return failure(400, &format!("the input isn't JSON: {e}"), Value::Null);
            }
        }
    };
    match pools.run_function(&name, &input, caller).await {
        Ok((status, json)) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            let mut resp = Response::new(full(json.to_string()));
            *resp.status_mut() = status;
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            resp
        }
        Err(f) => failure(f.status, &f.error, f.json),
    }
}

/// `/fetch/{service}/{path}`: the request goes to the service with its
/// own headers added, and its answer comes back as it is.
async fn fetch(
    pools: &super::Pools,
    service: String,
    path: String,
    req: Request<Incoming>,
    caller: &Caller,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in &parts.headers {
        // The token is for Unsworn, and the context for the server.
        if name == CALL_TOKEN || name == CONTEXT {
            continue;
        }
        let Ok(value) = value.to_str() else { continue };
        headers
            .entry(name.as_str().to_string())
            .and_modify(|all| {
                all.push_str(", ");
                all.push_str(value);
            })
            .or_insert_with(|| value.to_string());
    }
    let body = body.map_err(BoxError::from).boxed_unsync();
    let sent = pools
        .send_outbound(
            &service,
            parts.method.as_str(),
            &path,
            &headers,
            body,
            caller,
        )
        .await;
    match sent {
        Ok((mut parts, body)) => {
            strip_hop_by_hop(&mut parts.headers);
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, full(body))
        }
        Err((status, error)) => failure(status, &error, Value::Null),
    }
}
