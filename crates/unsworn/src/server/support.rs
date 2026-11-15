//! Shared pieces for the server: bodies, problem responses, the HTTP client
//! and metrics.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Response, StatusCode, header};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use serde_json::Value;
use unsworn_core::pipeline::Problem;

use crate::store::Store;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = UnsyncBoxBody<Bytes, BoxError>;
pub type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|e| match e {})
        .boxed_unsync()
}

pub fn empty() -> Body {
    Empty::new().map_err(|e| match e {}).boxed_unsync()
}

pub fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    let mut resp = Response::new(full(serde_json::to_vec(value).unwrap_or_default()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// An RFC 9457 problem response.
pub fn problem(status: StatusCode, detail: impl Into<String>) -> Response<Body> {
    problem_response(Problem::new(status, detail))
}

pub fn problem_response(problem: Problem) -> Response<Body> {
    let mut resp = Response::new(full(problem.body_bytes()));
    *resp.status_mut() = problem.status;
    *resp.headers_mut() = problem.headers;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(Problem::CONTENT_TYPE),
    );
    resp
}

/// What a request carries for a WebSocket its connection may upgrade to:
/// the connection's place among `[server].max_connections`, which the
/// tunnel keeps until it closes, and how long it may go without traffic.
#[derive(Clone)]
pub struct Tunnel {
    pub permit: std::sync::Arc<tokio::sync::OwnedSemaphorePermit>,
    pub idle: Duration,
}

impl Tunnel {
    /// Copies between the client and the upstream until either side closes
    /// or neither sends anything for `idle`.
    pub async fn run<A, B>(self, client: A, upstream: B)
    where
        A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let _permit = self.permit;
        let start = std::time::Instant::now();
        let last = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut a = Activity::new(client, start, last.clone());
        let mut b = Activity::new(upstream, start, last.clone());
        let copy = tokio::io::copy_bidirectional(&mut a, &mut b);
        tokio::pin!(copy);
        let check = (self.idle / 4).max(Duration::from_millis(10));
        loop {
            tokio::select! {
                _ = &mut copy => return,
                _ = tokio::time::sleep(check) => {
                    let since = Duration::from_millis(last.load(std::sync::atomic::Ordering::Relaxed));
                    if start.elapsed().saturating_sub(since) >= self.idle {
                        tracing::debug!("closing a WebSocket idle for {}s", self.idle.as_secs());
                        return;
                    }
                }
            }
        }
    }
}

/// An I/O stream that notes when it last read anything, in milliseconds
/// since `start`.
struct Activity<T> {
    io: T,
    start: std::time::Instant,
    last: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<T> Activity<T> {
    fn new(
        io: T,
        start: std::time::Instant,
        last: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Activity { io, start, last }
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Activity<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.io).poll_read(cx, buf);
        if matches!(poll, std::task::Poll::Ready(Ok(()))) && buf.filled().len() > before {
            let now = self.start.elapsed().as_millis() as u64;
            self.last.store(now, std::sync::atomic::Ordering::Relaxed);
        }
        poll
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Activity<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

pub fn build_client() -> HttpClient {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .build(https)
}

/// The methods metrics name. Any other is counted as `OTHER`.
const KNOWN_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT", "TRACE",
];

/// Request counts and durations, exposed in Prometheus format.
#[derive(Default)]
pub struct Metrics {
    requests: Mutex<HashMap<(String, String, u16), (u64, f64)>>,
}

impl Metrics {
    pub fn record(&self, method: &str, route: &str, status: u16, elapsed: Duration) {
        // A client can send any token as a method. Each one would be a new
        // series, kept for the life of the process.
        let method = if KNOWN_METHODS.contains(&method) {
            method
        } else {
            "OTHER"
        };
        let mut m = self.requests.lock().unwrap();
        let entry = m
            .entry((method.to_string(), route.to_string(), status))
            .or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += elapsed.as_secs_f64();
    }

    pub fn render(&self, runtimes: &[(String, bool, usize, usize)], store: &Store) -> String {
        let mut out = String::new();
        out.push_str("# HELP unsworn_requests_total Requests handled.\n");
        out.push_str("# TYPE unsworn_requests_total counter\n");
        let m = self.requests.lock().unwrap();
        let mut rows: Vec<_> = m.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        for ((method, route, status), (count, _)) in &rows {
            out.push_str(&format!(
                "unsworn_requests_total{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {count}\n",
                escape(route)
            ));
        }
        out.push_str("# HELP unsworn_request_duration_seconds_sum Total time spent on requests.\n");
        out.push_str("# TYPE unsworn_request_duration_seconds_sum counter\n");
        for ((method, route, status), (_, secs)) in &rows {
            out.push_str(&format!(
                "unsworn_request_duration_seconds_sum{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {secs:.6}\n",
                escape(route)
            ));
        }
        out.push_str("# HELP unsworn_runtime_workers Healthy workers per runtime.\n");
        out.push_str("# TYPE unsworn_runtime_workers gauge\n");
        for (name, _, alive, _) in runtimes {
            out.push_str(&format!(
                "unsworn_runtime_workers{{runtime=\"{name}\"}} {alive}\n"
            ));
        }
        out.push_str("# HELP unsworn_runtime_crashes_recent Worker exits in the last minute.\n");
        out.push_str("# TYPE unsworn_runtime_crashes_recent gauge\n");
        for (name, _, _, crashes) in runtimes {
            out.push_str(&format!(
                "unsworn_runtime_crashes_recent{{runtime=\"{name}\"}} {crashes}\n"
            ));
        }
        let backend = store.backend_name();
        out.push_str("# HELP unsworn_store_up Whether the last store operation succeeded.\n");
        out.push_str("# TYPE unsworn_store_up gauge\n");
        out.push_str(&format!(
            "unsworn_store_up{{backend=\"{backend}\"}} {}\n",
            u8::from(store.is_up())
        ));
        out.push_str("# HELP unsworn_store_errors_total Failed store operations.\n");
        out.push_str("# TYPE unsworn_store_errors_total counter\n");
        out.push_str(&format!(
            "unsworn_store_errors_total{{backend=\"{backend}\"}} {}\n",
            store.error_count()
        ));
        out
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_methods_share_one_series() {
        let metrics = Metrics::default();
        for method in ["GET", "PROPFIND", "AAAA", "AAAB"] {
            metrics.record(method, "(none)", 404, Duration::ZERO);
        }
        let series = metrics.requests.lock().unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[&("OTHER".into(), "(none)".into(), 404)].0, 3);
    }
}
