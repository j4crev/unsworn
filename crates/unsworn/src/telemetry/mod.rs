//! OpenTelemetry traces: a span for each sampled request, and one for each
//! thing the server asks of something else for it, sent over OTLP/HTTP to
//! the `[outbound]` service `[telemetry]` names. See
//! `docs/telemetry-design.md`.
//!
//! The spans are built by [`unsworn_core::telemetry`], as on every host.
//! Here are the server's clock and random numbers, and the sending.
//!
//! A request costs little here. Whether it's sampled is decided from its
//! headers, a span is put together once what it covers is over, and the span
//! then waits in a queue that one task sends from in batches. Nothing a
//! request does waits for the collector.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, header};
use http_body_util::{BodyExt, Limited};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use unsworn_core::outbound;
use unsworn_core::project::{METRICS_PATH, Outbound, TRACES_PATH, Telemetry};
pub use unsworn_core::telemetry::{
    Attr, Decision, Ended, Outcome, Recorder, SpanRecord, Started, Steps, call_span, cut,
    failed_status, fetch_span, handler_span, outbound_trace, proxy_span, service_span, strip_trace,
    without_credentials,
};
use unsworn_core::telemetry::{Moment, decide, known_method, otlp, resource_attributes};

use crate::server::support::{HttpClient, Metrics, bucket_bounds_secs, full};
use crate::store::Store;

/// A span of the server's that others can go below.
pub type Parent = unsworn_core::telemetry::Parent<Arc<Tracer>>;
/// Something the server is asking of something else, as its span.
pub type ChildSpan = unsworn_core::telemetry::ChildSpan<Arc<Tracer>>;

/// How many spans can wait to be sent. More are dropped.
const QUEUE: usize = 2048;

/// The most spans sent in one request.
const BATCH: usize = 512;

/// How long a batch waits to fill, from its first span.
const DELAY: Duration = Duration::from_secs(5);

/// How often, at most, lost spans are warned about.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// How much of a collector's answer is read.
const MAX_ANSWER: usize = 64 * 1024;

/// Settings that would send spans somewhere the config doesn't name.
const IGNORED_ENV: &[&str] = &[
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
];

/// The next of this thread's random numbers (SplitMix64). Ids have to
/// differ, not to be secret: they're sent to everything a request reaches.
fn random() -> u64 {
    thread_local! {
        static STATE: Cell<Option<u64>> = const { Cell::new(None) };
    }
    STATE.with(|state| {
        let next = state
            .get()
            .unwrap_or_else(seed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        state.set(Some(next));
        let mut z = next;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    })
}

fn seed() -> u64 {
    let mut bytes = [0; 8];
    match SystemRandom::new().fill(&mut bytes) {
        Ok(()) => u64::from_ne_bytes(bytes),
        // The standard library's hashers are seeded by the system too.
        Err(_) => RandomState::new().hash_one(std::thread::current().id()),
    }
}

/// A time as OTLP counts it: nanoseconds since the Unix epoch.
pub fn unix_nano(time: SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
}

/// Where one project's spans go, and what they say they come from.
struct Sink {
    outbound: Arc<Outbound>,
    /// The `Resource` message, put together once.
    resource: Bytes,
}

/// A span waiting to be sent, and where to.
type Queued = (Arc<Sink>, SpanRecord);

#[derive(Clone)]
struct Queue {
    spans: mpsc::Sender<Queued>,
    /// Spans there was no room for.
    dropped: Arc<AtomicU64>,
}

/// Samples and records one loaded project's requests.
pub struct Tracer {
    sample: f64,
    metrics: bool,
    sink: Arc<Sink>,
    queue: Queue,
}

impl Tracer {
    /// Whether a request is sampled. `trusted` says its peer is one of
    /// `[server].trusted_proxies`.
    pub fn start(&self, headers: &HeaderMap, trusted: bool) -> Decision {
        decide(self.sample, headers, trusted, &mut random)
    }

    /// Whether `[telemetry].metrics` is enabled.
    pub fn sends_metrics(&self) -> bool {
        self.metrics
    }

    /// Snapshots the server's metrics and sends them to the collector over
    /// OTLP/HTTP (`POST /v1/metrics`).
    pub async fn export_metrics(
        &self,
        client: &HttpClient,
        metrics: &Metrics,
        runtimes: &[(String, bool, usize, usize)],
        store: &Store,
        started_unix_nano: u64,
    ) {
        if !self.metrics {
            return;
        }
        let now = unix_nano(SystemTime::now());
        let payload = build_metrics(metrics, runtimes, store, started_unix_nano, now);
        let body = otlp::metrics_request(&self.sink.resource, &payload);
        if let Err(why) = post(client, &self.sink.outbound, METRICS_PATH, body).await {
            tracing::warn!(
                target: "telemetry",
                "metrics weren't exported: [outbound.{}] {why}",
                self.sink.outbound.name
            );
        }
    }
}

impl Recorder for Tracer {
    /// Queues a span to be sent. Never waits.
    fn record(&self, span: SpanRecord) {
        if self
            .queue
            .spans
            .try_send((self.sink.clone(), span))
            .is_err()
        {
            self.queue.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn now(&self) -> Moment {
        static EPOCH: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
        Moment {
            unix_nano: unix_nano(SystemTime::now()),
            monotonic: EPOCH.elapsed(),
        }
    }

    fn random(&self) -> u64 {
        random()
    }
}

/// Sends the spans of every tracer it makes. One for the process, so spans
/// waiting when the project is reloaded are still sent.
pub struct Exporter {
    queue: Queue,
    flushes: mpsc::Sender<oneshot::Sender<()>>,
    /// What does the sending, until the first tracer starts it.
    idle: Mutex<Option<Sending>>,
}

impl Exporter {
    /// `client` follows no redirects, so spans go only to the URL the
    /// config names.
    pub fn new(client: HttpClient) -> Exporter {
        let (spans, queued) = mpsc::channel(QUEUE);
        let (flushes, asked) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        Exporter {
            queue: Queue {
                spans,
                dropped: dropped.clone(),
            },
            flushes,
            idle: Mutex::new(Some(Sending {
                client,
                spans: queued,
                flushes: asked,
                lost: Lost {
                    dropped,
                    failed: 0,
                    why: String::new(),
                    warned: None,
                },
            })),
        }
    }

    /// A tracer for a project with `[telemetry]`. Call it inside the tokio
    /// runtime: the first one starts the task that sends.
    pub fn tracer(&self, telemetry: &Telemetry) -> Arc<Tracer> {
        if let Some(sending) = self.idle.lock().unwrap().take() {
            tokio::spawn(sending.run());
        }
        let ignored: Vec<&str> = IGNORED_ENV
            .iter()
            .copied()
            .filter(|name| std::env::var_os(name).is_some())
            .collect();
        if !ignored.is_empty() {
            tracing::warn!(
                target: "telemetry",
                "ignoring {}: spans go to [outbound.{}]",
                ignored.join(", "),
                telemetry.outbound.name
            );
        }
        let env = |name| std::env::var(name).ok();
        let attributes = resource_attributes(
            telemetry.service_name.as_deref(),
            env("OTEL_SERVICE_NAME").as_deref(),
            env("OTEL_RESOURCE_ATTRIBUTES").as_deref(),
        );
        Arc::new(Tracer {
            sample: telemetry.sample,
            metrics: telemetry.metrics,
            sink: Arc::new(Sink {
                outbound: telemetry.outbound.clone(),
                resource: otlp::resource(&attributes).into(),
            }),
            queue: self.queue.clone(),
        })
    }

    /// Sends what's waiting, taking at most `timeout`. Call before the
    /// process exits.
    pub async fn flush(&self, timeout: Duration) {
        if self.idle.lock().unwrap().is_some() {
            return;
        }
        let (done, sent) = oneshot::channel();
        let flushed = async {
            if self.flushes.send(done).await.is_ok() {
                let _ = sent.await;
            }
        };
        if tokio::time::timeout(timeout, flushed).await.is_err() {
            tracing::warn!(target: "telemetry", "not every span was sent before shutting down");
        }
    }
}

/// Spans that never arrived, since the last warning about them.
struct Lost {
    dropped: Arc<AtomicU64>,
    /// Spans in requests that failed.
    failed: u64,
    /// How the last of those failed.
    why: String,
    warned: Option<Instant>,
}

impl Lost {
    /// When there's next something to warn about.
    fn due(&self) -> Option<Instant> {
        let any = self.failed > 0 || self.dropped.load(Ordering::Relaxed) > 0;
        any.then(|| self.warned.map_or_else(Instant::now, |at| at + WARN_EVERY))
    }

    /// Warns about them, unless it did a moment ago and `now` isn't set.
    fn warn(&mut self, now: bool) {
        let Some(due) = self.due() else { return };
        if !now && due > Instant::now() {
            return;
        }
        let failed = std::mem::take(&mut self.failed);
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        let why = std::mem::take(&mut self.why);
        if failed > 0 {
            tracing::warn!(target: "telemetry", failed, dropped, "spans weren't exported: {why}");
        } else {
            tracing::warn!(
                target: "telemetry",
                failed,
                dropped,
                "spans were dropped: they came faster than they could be sent"
            );
        }
        self.warned = Some(Instant::now());
    }
}

/// The task that sends spans.
struct Sending {
    client: HttpClient,
    spans: mpsc::Receiver<Queued>,
    flushes: mpsc::Receiver<oneshot::Sender<()>>,
    lost: Lost,
}

impl Sending {
    async fn run(mut self) {
        let mut batch = Vec::new();
        // When the batch is sent, however full it is.
        let mut due = None;
        loop {
            let warning = self.lost.due();
            tokio::select! {
                span = self.spans.recv() => {
                    let Some(span) = span else { return };
                    if batch.is_empty() {
                        due = Some(Instant::now() + DELAY);
                    }
                    batch.push(span);
                    if batch.len() >= BATCH {
                        self.send(&mut batch).await;
                        due = None;
                    }
                }
                () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                    self.send(&mut batch).await;
                    due = None;
                }
                Some(done) = self.flushes.recv() => {
                    while let Ok(span) = self.spans.try_recv() {
                        batch.push(span);
                        if batch.len() >= BATCH {
                            self.send(&mut batch).await;
                        }
                    }
                    self.send(&mut batch).await;
                    due = None;
                    self.lost.warn(true);
                    let _ = done.send(());
                }
                // Only wakes the loop, for the warning below.
                () = tokio::time::sleep_until(warning.unwrap_or_else(Instant::now)), if warning.is_some() => {}
            }
            self.lost.warn(false);
        }
    }

    /// Sends a batch and empties it: one request for each place its spans
    /// go, which is more than one only across a reload.
    async fn send(&mut self, batch: &mut Vec<Queued>) {
        let mut rest = std::mem::take(batch);
        while let Some((sink, _)) = rest.first() {
            let sink = sink.clone();
            let (spans, others): (Vec<Queued>, Vec<Queued>) = rest
                .into_iter()
                .partition(|(other, _)| Arc::ptr_eq(other, &sink));
            rest = others;
            let body = otlp::request(&sink.resource, spans.iter().map(|(_, span)| span));
            if let Err(why) = post(&self.client, &sink.outbound, TRACES_PATH, body).await {
                self.lost.failed += spans.len() as u64;
                self.lost.why = format!("[outbound.{}] {why}", sink.outbound.name);
            }
        }
    }
}

/// Builds the server's OTLP metrics snapshot.
fn build_metrics(
    metrics: &Metrics,
    runtimes: &[(String, bool, usize, usize)],
    store: &Store,
    start_unix_nano: u64,
    time_unix_nano: u64,
) -> Vec<otlp::Metric> {
    let mut out = Vec::new();
    let series = metrics.series();
    if !series.is_empty() {
        let bounds = bucket_bounds_secs();
        let mut durations = Vec::with_capacity(series.len());
        let mut handler_times = Vec::new();
        for ((method, route, status), totals) in series {
            let mut attrs = Vec::with_capacity(3);
            attrs.push(("http.request.method", Attr::Static(known_method(method))));
            if route != "(none)" {
                attrs.push(("http.route", Attr::Str(route)));
            }
            attrs.push(("http.response.status_code", Attr::Int(i64::from(status))));
            if totals.handler_seconds > 0.0 {
                handler_times.push(otlp::NumberPoint {
                    attributes: attrs.clone(),
                    start_unix_nano,
                    time_unix_nano,
                    value: otlp::Number::Double(totals.handler_seconds),
                });
            }
            durations.push(otlp::HistogramPoint {
                attributes: attrs,
                start_unix_nano,
                time_unix_nano,
                count: totals.count,
                sum: totals.seconds,
                bounds: bounds.clone(),
                buckets: totals.buckets,
            });
        }
        out.push(otlp::Metric {
            name: "http.server.request.duration",
            unit: "s",
            data: otlp::MetricData::Histogram(durations),
        });
        if !handler_times.is_empty() {
            out.push(otlp::Metric {
                name: "unsworn.handler.time",
                unit: "s",
                data: otlp::MetricData::Sum(handler_times),
            });
        }
    }
    if !runtimes.is_empty() {
        let mut workers = Vec::with_capacity(runtimes.len());
        let mut crashes = Vec::with_capacity(runtimes.len());
        for (name, _, alive, recent_crashes) in runtimes {
            let attr = vec![("unsworn.runtime", Attr::Str(name.clone()))];
            workers.push(otlp::NumberPoint {
                attributes: attr.clone(),
                start_unix_nano: 0,
                time_unix_nano,
                value: otlp::Number::Int(*alive as i64),
            });
            crashes.push(otlp::NumberPoint {
                attributes: attr,
                start_unix_nano: 0,
                time_unix_nano,
                value: otlp::Number::Int(*recent_crashes as i64),
            });
        }
        out.push(otlp::Metric {
            name: "unsworn.runtime.workers",
            unit: "",
            data: otlp::MetricData::Gauge(workers),
        });
        out.push(otlp::Metric {
            name: "unsworn.runtime.crashes",
            unit: "",
            data: otlp::MetricData::Gauge(crashes),
        });
    }
    let backend = store.backend_name();
    out.push(otlp::Metric {
        name: "unsworn.store.up",
        unit: "",
        data: otlp::MetricData::Gauge(vec![otlp::NumberPoint {
            attributes: vec![("unsworn.store.backend", Attr::Static(backend))],
            start_unix_nano: 0,
            time_unix_nano,
            value: otlp::Number::Int(i64::from(store.is_up())),
        }]),
    });
    out.push(otlp::Metric {
        name: "unsworn.store.errors",
        unit: "",
        data: otlp::MetricData::Sum(vec![otlp::NumberPoint {
            attributes: vec![("unsworn.store.backend", Attr::Static(backend))],
            start_unix_nano,
            time_unix_nano,
            value: otlp::Number::Int(store.error_count() as i64),
        }]),
    });
    out
}

/// Sends one request to the collector, built as a handler's `fetch` is,
/// with the service's headers and `timeout`. Anything but a `2xx`, a
/// redirect included, is a failure.
async fn post(
    client: &HttpClient,
    service: &Outbound,
    path: &str,
    body: Vec<u8>,
) -> Result<(), String> {
    let planned = outbound::plan(service, "POST", path, &BTreeMap::new(), None)
        .map_err(|(_, why)| why)?;
    let mut request = http::Request::post(planned.url);
    for (name, value) in &planned.headers {
        request = request.header(name, value);
    }
    let mut request = request
        .body(full(body))
        .map_err(|e| format!("can't be sent to: {e}"))?;
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-protobuf"),
    );
    let answer = async {
        let response = client
            .request(request)
            .await
            .map_err(|e| format!("couldn't be reached: {e}"))?;
        let status = response.status();
        // Read so the connection can be used again.
        let _ = Limited::new(response.into_body(), MAX_ANSWER)
            .collect()
            .await;
        match status.is_success() {
            true => Ok(()),
            false => Err(format!("answered {status}")),
        }
    };
    tokio::time::timeout(service.timeout, answer)
        .await
        .map_err(|_| "took too long to answer".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_threads_numbers_differ() {
        let drawn: std::collections::HashSet<u64> = (0..1000).map(|_| random()).collect();
        assert_eq!(drawn.len(), 1000);
        let other = std::thread::spawn(random).join().unwrap();
        assert!(!drawn.contains(&other));
    }
}
