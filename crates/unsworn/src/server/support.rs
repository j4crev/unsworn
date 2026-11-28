//! Shared pieces for the server: bodies, problem responses, the HTTP client
//! and metrics.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Response, StatusCode, header};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use serde_json::Value;
use unsworn_core::pipeline::Problem;
use unsworn_core::telemetry::KNOWN_METHODS;

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

/// A small number for the calling thread, the same every time: its place
/// in state that's kept per thread so threads don't wait on each other.
pub fn thread_slot() -> usize {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static THREADS: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    SLOT.with(|slot| {
        if slot.get() == usize::MAX {
            slot.set(THREADS.fetch_add(1, Ordering::Relaxed));
        }
        slot.get()
    })
}

/// How many slots per-thread state should have.
pub fn thread_slots() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// How many buckets a series sorts its requests' durations into: four
/// for each doubling from 50 µs, which reaches past an hour.
pub const BUCKETS: usize = 104;

/// The bucket a duration falls in.
fn bucket(elapsed: Duration) -> usize {
    let micros = elapsed.as_micros() as f64;
    if micros < 50.0 {
        return 0;
    }
    (((micros / 50.0).log2() * 4.0) as usize + 1).min(BUCKETS - 1)
}

/// The longest duration a bucket holds, in milliseconds.
pub fn bucket_bound_ms(index: usize) -> f64 {
    0.05 * 2f64.powf(index as f64 / 4.0)
}

/// The explicit upper bounds of the histogram buckets, in seconds, for OTLP.
/// `BUCKETS - 1` bounds divide durations into `BUCKETS` buckets, the last
/// being the overflow bucket.
pub fn bucket_bounds_secs() -> Vec<f64> {
    (0..BUCKETS - 1)
        .map(|i| bucket_bound_ms(i) / 1000.0)
        .collect()
}

/// Totals for one `(method, route, status)` series across all shards.
pub struct SeriesTotals {
    pub count: u64,
    pub seconds: f64,
    pub handler_seconds: f64,
    pub buckets: Vec<u64>,
}

/// One route's requests with one method and status.
struct Series {
    method: &'static str,
    status: u16,
    count: u64,
    seconds: f64,
    /// The part of `seconds` spent waiting for handlers: the route's own
    /// and any `before` handlers.
    handler_seconds: f64,
    /// The runtime that ran the route's handler, if one did.
    runtime: Option<Arc<str>>,
    /// How many requests took each length of time.
    buckets: Box<[u64; BUCKETS]>,
}

/// One route's requests with one method, whatever their status. See
/// [`Metrics::routes`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteStats {
    pub method: &'static str,
    pub route: String,
    pub runtime: Option<String>,
    pub count: u64,
    /// Those answered `500` or above.
    pub errors: u64,
    pub seconds: f64,
    pub handler_seconds: f64,
    pub buckets: Vec<u64>,
}

/// Request counts and durations, exposed in Prometheus format. Each thread
/// counts in its own map (see [`thread_slot`]), and `render` adds them up.
pub struct Metrics {
    shards: Box<[Mutex<HashMap<Box<str>, Vec<Series>>>]>,
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics {
            shards: (0..thread_slots()).map(|_| Mutex::default()).collect(),
        }
    }
}

impl Metrics {
    /// Counts one request. `handler` is how much of `elapsed` went to
    /// waiting for handlers, and `runtime` the one that ran the route's.
    pub fn record(
        &self,
        method: &str,
        route: &str,
        status: u16,
        elapsed: Duration,
        handler: Duration,
        runtime: Option<&Arc<str>>,
    ) {
        // A client can send any token as a method. Each one would be a new
        // series, kept for the life of the process.
        let method = KNOWN_METHODS
            .iter()
            .find(|known| **known == method)
            .copied()
            .unwrap_or("OTHER");
        let mut shard = self.shards[thread_slot() % self.shards.len()]
            .lock()
            .unwrap();
        let series = match shard.get_mut(route) {
            Some(series) => series,
            None => shard.entry(route.into()).or_default(),
        };
        let found = series
            .iter()
            .position(|s| s.status == status && s.method == method);
        let index = found.unwrap_or_else(|| {
            series.push(Series {
                method,
                status,
                count: 0,
                seconds: 0.0,
                handler_seconds: 0.0,
                runtime: None,
                buckets: Box::new([0; BUCKETS]),
            });
            series.len() - 1
        });
        let s = &mut series[index];
        s.count += 1;
        s.seconds += elapsed.as_secs_f64();
        s.handler_seconds += handler.as_secs_f64();
        s.buckets[bucket(elapsed)] += 1;
        if s.runtime.is_none()
            && let Some(runtime) = runtime
        {
            s.runtime = Some(runtime.clone());
        }
    }

    /// Every route's requests so far, by route and method, for
    /// `/_unsworn/stats`.
    pub fn routes(&self) -> Vec<RouteStats> {
        let mut found: std::collections::BTreeMap<(String, &'static str), RouteStats> =
            std::collections::BTreeMap::new();
        for shard in &self.shards {
            for (route, series) in shard.lock().unwrap().iter() {
                for s in series {
                    let stats = found
                        .entry((route.to_string(), s.method))
                        .or_insert_with(|| RouteStats {
                            method: s.method,
                            route: route.to_string(),
                            runtime: None,
                            count: 0,
                            errors: 0,
                            seconds: 0.0,
                            handler_seconds: 0.0,
                            buckets: vec![0; BUCKETS],
                        });
                    stats.count += s.count;
                    if s.status >= 500 {
                        stats.errors += s.count;
                    }
                    stats.seconds += s.seconds;
                    stats.handler_seconds += s.handler_seconds;
                    for (total, n) in stats.buckets.iter_mut().zip(s.buckets.iter()) {
                        *total += n;
                    }
                    if stats.runtime.is_none() {
                        stats.runtime = s.runtime.as_deref().map(String::from);
                    }
                }
            }
        }
        found.into_values().collect()
    }

    /// Every series' count, total seconds, handler seconds and histogram
    /// buckets, by method, route and status.
    pub fn series(&self) -> std::collections::BTreeMap<(&'static str, String, u16), SeriesTotals> {
        let mut totals: std::collections::BTreeMap<(&'static str, String, u16), SeriesTotals> =
            std::collections::BTreeMap::new();
        for shard in &self.shards {
            for (route, series) in shard.lock().unwrap().iter() {
                for s in series {
                    let total = totals
                        .entry((s.method, route.to_string(), s.status))
                        .or_insert_with(|| SeriesTotals {
                            count: 0,
                            seconds: 0.0,
                            handler_seconds: 0.0,
                            buckets: vec![0; BUCKETS],
                        });
                    total.count += s.count;
                    total.seconds += s.seconds;
                    total.handler_seconds += s.handler_seconds;
                    for (acc, n) in total.buckets.iter_mut().zip(s.buckets.iter()) {
                        *acc += n;
                    }
                }
            }
        }
        totals
    }

    /// Every series' count, total seconds and seconds in handlers, by
    /// method, route and status.
    fn totals(&self) -> std::collections::BTreeMap<(&'static str, String, u16), (u64, f64, f64)> {
        let mut totals = std::collections::BTreeMap::new();
        for shard in &self.shards {
            for (route, series) in shard.lock().unwrap().iter() {
                for s in series {
                    let total = totals
                        .entry((s.method, route.to_string(), s.status))
                        .or_insert((0, 0.0, 0.0));
                    total.0 += s.count;
                    total.1 += s.seconds;
                    total.2 += s.handler_seconds;
                }
            }
        }
        totals
    }

    pub fn render(&self, runtimes: &[(String, bool, usize, usize)], store: &Store) -> String {
        let mut out = String::new();
        out.push_str("# HELP unsworn_requests_total Requests handled.\n");
        out.push_str("# TYPE unsworn_requests_total counter\n");
        let rows = self.totals();
        for ((method, route, status), (count, _, _)) in &rows {
            out.push_str(&format!(
                "unsworn_requests_total{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {count}\n",
                escape(route)
            ));
        }
        out.push_str("# HELP unsworn_request_duration_seconds_sum Total time spent on requests.\n");
        out.push_str("# TYPE unsworn_request_duration_seconds_sum counter\n");
        for ((method, route, status), (_, secs, _)) in &rows {
            out.push_str(&format!(
                "unsworn_request_duration_seconds_sum{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {secs:.6}\n",
                escape(route)
            ));
        }
        out.push_str(
            "# HELP unsworn_handler_duration_seconds_sum The part of that time spent waiting for handlers.\n",
        );
        out.push_str("# TYPE unsworn_handler_duration_seconds_sum counter\n");
        for ((method, route, status), (_, _, secs)) in rows.iter().filter(|(_, v)| v.2 > 0.0) {
            out.push_str(&format!(
                "unsworn_handler_duration_seconds_sum{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {secs:.6}\n",
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

/// How a route's shadow handler has done against the route's own.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ShadowStats {
    pub method: String,
    pub route: String,
    /// The shadow handler, and the runtime that runs it.
    pub handler: String,
    pub runtime: String,
    /// Requests both handlers answered.
    pub compared: u64,
    /// Those they answered the same.
    pub same: u64,
    /// Requests the shadow failed, timed out on or couldn't take.
    pub failed: u64,
    /// The time the route's own handler took for those requests, and the
    /// shadow's.
    pub primary_seconds: f64,
    pub shadow_seconds: f64,
    /// Where the latest pair of answers that differed did.
    pub last_difference: Option<String>,
}

/// Every route's [`ShadowStats`].
#[derive(Default)]
pub struct Shadows {
    rows: Mutex<std::collections::BTreeMap<(String, String), ShadowStats>>,
}

impl Shadows {
    /// Counts one copied request. `outcome` is whether the shadow's answer
    /// was the same, or `None` when it gave none. Returns how many of the
    /// route's requests have gone wrong so far, this one included, when
    /// this one did.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        method: &str,
        route: &str,
        handler: &str,
        runtime: &str,
        outcome: Option<Result<(), &str>>,
        primary: Duration,
        shadow: Duration,
    ) -> Option<u64> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .entry((route.to_string(), method.to_string()))
            .or_insert_with(|| ShadowStats {
                method: method.to_string(),
                route: route.to_string(),
                ..ShadowStats::default()
            });
        // A reload may have pointed the route at another shadow.
        if row.handler != handler {
            row.handler = handler.to_string();
            row.runtime = runtime.to_string();
        }
        match outcome {
            None => row.failed += 1,
            Some(result) => {
                row.compared += 1;
                row.primary_seconds += primary.as_secs_f64();
                row.shadow_seconds += shadow.as_secs_f64();
                match result {
                    Ok(()) => row.same += 1,
                    Err(what) => row.last_difference = Some(what.to_string()),
                }
            }
        }
        matches!(outcome, None | Some(Err(_))).then(|| row.failed + row.compared - row.same)
    }

    pub fn all(&self) -> Vec<ShadowStats> {
        self.rows.lock().unwrap().values().cloned().collect()
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
            metrics.record(method, "(none)", 404, Duration::ZERO, Duration::ZERO, None);
        }
        let series = metrics.totals();
        assert_eq!(series.len(), 2);
        assert_eq!(series[&("OTHER", "(none)".into(), 404)].0, 3);
    }
}
