//! OpenTelemetry traces: a span for each sampled request, and one for each
//! thing the server asks of something else for it, sent over OTLP/HTTP to
//! the `[outbound]` service `[telemetry]` names. See
//! `docs/telemetry-design.md`.
//!
//! A request costs little here. Whether it's sampled is decided from its
//! headers, a span is put together once what it covers is over, and the span
//! then waits in a queue that one task sends from in batches. Nothing a
//! request does waits for the collector.

mod otlp;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::hash::{BuildHasher, RandomState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, header};
use http_body_util::{BodyExt, Limited};
use percent_encoding::percent_decode_str;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use unsworn_core::VERSION;
use unsworn_core::outbound;
use unsworn_core::pipeline::Step;
use unsworn_core::project::{Outbound, TRACES_PATH, Telemetry};
use unsworn_core::trace::{TRACEPARENT, TraceContext, valid_traceparent};

use crate::server::support::{HttpClient, KNOWN_METHODS, full};

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

/// How much a span keeps of a path, a name or an error. Someone else picks
/// their length, and spans wait in memory.
const MAX_PATH: usize = 2048;

/// What a span says when what it covers was given up on before it ended.
const CANCELLED: &str = "cancelled before an answer: the request timed out or ended";

/// What a request's own span says when it was never answered.
const ABANDONED: &str = "cancelled before an answer: the client went away";

/// Settings that would send spans somewhere the config doesn't name.
const IGNORED_ENV: &[&str] = &[
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanId(pub [u8; 8]);

impl TraceId {
    /// A new one. All zeros means no trace, so that's never given.
    fn random() -> TraceId {
        loop {
            let mut id = [0; 16];
            id[..8].copy_from_slice(&random().to_be_bytes());
            id[8..].copy_from_slice(&random().to_be_bytes());
            if id != [0; 16] {
                return TraceId(id);
            }
        }
    }

    pub fn hex(&self) -> String {
        hex(&self.0)
    }
}

impl SpanId {
    /// A new one, never all zeros.
    fn random() -> SpanId {
        loop {
            let id = random().to_be_bytes();
            if id != [0; 8] {
                return SpanId(id);
            }
        }
    }
}

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

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0xf)]));
    }
    out
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let mut out = [0; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A sampled request's place in its trace.
#[derive(Debug, Clone, Copy)]
pub struct Started {
    pub trace_id: TraceId,
    /// The span of whatever sent the request, when it came with one.
    pub parent: Option<SpanId>,
    /// The request's own span.
    pub span_id: SpanId,
    /// The trace starts here: the request came without a `traceparent` a
    /// receiver would accept.
    pub new_root: bool,
}

impl Started {
    /// The header for whatever the request reaches from here, which makes
    /// this span its parent.
    pub fn traceparent(&self) -> String {
        sampled_traceparent(self.trace_id, self.span_id)
    }
}

/// The header that makes the span `span_id` the parent of what gets it.
fn sampled_traceparent(trace_id: TraceId, span_id: SpanId) -> String {
    format!("00-{}-{}-01", trace_id.hex(), hex(&span_id.0))
}

/// A `traceparent` a receiver would accept: its trace id, its parent id and
/// whether it's marked sampled.
fn parse(value: &str) -> Option<(TraceId, SpanId, bool)> {
    let value = value.trim();
    if !valid_traceparent(value) {
        return None;
    }
    // `<version>-<trace id>-<parent id>-<flags>`, as just checked.
    let flags = u8::from_str_radix(&value[53..55], 16).ok()?;
    Some((
        TraceId(unhex(&value[3..35])?),
        SpanId(unhex(&value[36..52])?),
        flags & 1 == 1,
    ))
}

/// Whether a request is sampled. One that came with a `traceparent` follows
/// its sampled flag. `sample` of the others start a trace.
fn decide(sample: f64, headers: &HeaderMap) -> Option<Started> {
    let incoming = headers
        .get(TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .and_then(parse);
    if let Some((trace_id, parent, sampled)) = incoming {
        return sampled.then(|| Started {
            trace_id,
            parent: Some(parent),
            span_id: SpanId::random(),
            new_root: false,
        });
    }
    if sample <= 0.0 {
        return None;
    }
    let trace_id = TraceId::random();
    // Decided by the id's last 53 bits' worth, as a number from 0 to 1.
    let mut low = [0; 8];
    low.copy_from_slice(&trace_id.0[8..]);
    let share = (u64::from_be_bytes(low) >> 11) as f64 / (1u64 << 53) as f64;
    (sample >= 1.0 || share < sample).then(|| Started {
        trace_id,
        parent: None,
        span_id: SpanId::random(),
        new_root: true,
    })
}

/// What the server knows about a request once it's over.
pub struct Ended<'a> {
    pub started_at: SystemTime,
    pub elapsed: Duration,
    pub method: &'a str,
    /// Without the query string.
    pub path: &'a str,
    pub tls: bool,
    pub client: IpAddr,
    pub version: http::Version,
    pub request_id: &'a str,
    /// `None` when the client went away before there was one.
    pub answer: Option<Outcome<'a>>,
}

/// How a request was answered.
pub struct Outcome<'a> {
    /// The matched route, or `(none)`.
    pub route: &'a str,
    pub status: u16,
    pub runtime: Option<&'a Arc<str>>,
    pub in_handlers: Duration,
}

/// What a span is to the server. The numbers are OTLP's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A request the server answered.
    Server = 2,
    /// Something the server asked of something else.
    Client = 3,
}

/// An attribute's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    Str(String),
    Static(&'static str),
    Int(i64),
    Double(f64),
}

pub type Attributes = Vec<(&'static str, Attr)>;

/// A span, ready to send.
#[derive(Debug)]
pub struct SpanRecord {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent: Option<SpanId>,
    pub name: String,
    pub kind: Kind,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub attributes: Attributes,
    /// What happened during it, and when.
    pub events: Vec<Event>,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

/// Something that happened during a span.
#[derive(Debug)]
pub struct Event {
    pub name: &'static str,
    pub time_unix_nano: u64,
    pub attributes: Attributes,
}

/// What the pipeline decided about a sampled request, as it did: events on
/// its span, and attributes that say how it ended.
#[derive(Default)]
pub struct Steps {
    noted: Mutex<Vec<(Event, Option<(&'static str, Attr)>)>>,
}

impl Steps {
    pub fn note(&self, step: Step<'_>) {
        // The event's name, what it says, and what it sets on the span.
        let (name, says, sets) = match step {
            Step::Authenticated(scheme) => {
                let scheme = ("unsworn.auth.scheme", Attr::Str(cut(scheme).to_string()));
                ("unsworn.authenticated", Some(scheme.clone()), Some(scheme))
            }
            Step::Checked => ("unsworn.checked", None, None),
            Step::Cache(what) => {
                let what = ("unsworn.cache", Attr::Static(what.name()));
                ("unsworn.cache", Some(what.clone()), Some(what))
            }
            Step::ResponseInvalid => (
                "unsworn.response_invalid",
                None,
                Some(("unsworn.response_validation", Attr::Static("failed"))),
            ),
            Step::Rejected(rule) => (
                "unsworn.rejected",
                Some(("unsworn.rule", Attr::Static(rule.name()))),
                Some(("unsworn.rejected_by", Attr::Static(rule.name()))),
            ),
        };
        let event = Event {
            name,
            time_unix_nano: span_times(SystemTime::now(), Duration::ZERO).0,
            attributes: says.into_iter().collect(),
        };
        self.noted.lock().unwrap().push((event, sets));
    }

    /// The events, and the attributes for the span: the last of each name.
    fn take(&self) -> (Vec<Event>, Attributes) {
        let noted = std::mem::take(&mut *self.noted.lock().unwrap());
        let mut attributes: Attributes = Vec::new();
        let mut events = Vec::with_capacity(noted.len());
        for (event, attribute) in noted {
            if let Some((key, value)) = attribute {
                match attributes.iter_mut().find(|(k, _)| *k == key) {
                    Some(found) => found.1 = value,
                    None => attributes.push((key, value)),
                }
            }
            events.push(event);
        }
        (events, attributes)
    }
}

/// A method as spans name it. Anyone who sends a request can put any token
/// there, so one that isn't standard is `_OTHER`.
pub fn known_method(method: &str) -> &'static str {
    KNOWN_METHODS
        .iter()
        .find(|known| **known == method)
        .copied()
        .unwrap_or("_OTHER")
}

/// At most [`MAX_PATH`] bytes of `text`, cut between characters.
pub fn cut(text: &str) -> &str {
    let mut end = text.len().min(MAX_PATH);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Why an answer with `status` fails its span, if it does. A client's error
/// isn't a failure of what answered: only `500` and above are.
pub fn failed_status(status: u16) -> Option<String> {
    (status >= 500).then(|| format!("answered {status}"))
}

/// A span's start and end, as OTLP counts time.
fn span_times(started_at: SystemTime, elapsed: Duration) -> (u64, u64) {
    let since = started_at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let start = u64::try_from(since.as_nanos()).unwrap_or(u64::MAX);
    let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    (start, start.saturating_add(elapsed))
}

impl SpanRecord {
    /// Adds what the pipeline decided while the span went on.
    pub fn with_steps(mut self, steps: &Steps) -> SpanRecord {
        let (events, attributes) = steps.take();
        self.events = events;
        self.attributes.extend(attributes);
        self
    }

    /// The `SERVER` span of a request.
    pub fn new(span: &Started, ended: Ended<'_>) -> SpanRecord {
        let method = known_method(ended.method);
        let answer = ended.answer.as_ref();
        let route = answer
            .map(|answer| answer.route)
            .filter(|route| *route != "(none)");
        let name = match route {
            Some(route) => format!("{method} {route}"),
            None => method.to_string(),
        };
        let (start_unix_nano, end_unix_nano) = span_times(ended.started_at, ended.elapsed);
        let mut attributes = Vec::with_capacity(10);
        attributes.extend([
            ("http.request.method", Attr::Static(method)),
            ("url.path", Attr::Str(cut(ended.path).to_string())),
            (
                "url.scheme",
                Attr::Static(if ended.tls { "https" } else { "http" }),
            ),
        ]);
        if let Some(route) = route {
            attributes.push(("http.route", Attr::Str(route.to_string())));
        }
        let protocol = match ended.version {
            http::Version::HTTP_09 => "0.9",
            http::Version::HTTP_10 => "1.0",
            http::Version::HTTP_2 => "2",
            http::Version::HTTP_3 => "3",
            _ => "1.1",
        };
        if let Some(answer) = answer {
            let status = Attr::Int(i64::from(answer.status));
            attributes.push(("http.response.status_code", status));
        }
        attributes.extend([
            ("client.address", Attr::Str(ended.client.to_string())),
            ("network.protocol.version", Attr::Static(protocol)),
            (
                "unsworn.request_id",
                Attr::Str(ended.request_id.to_string()),
            ),
        ]);
        if let Some(answer) = answer {
            if let Some(runtime) = answer.runtime {
                attributes.push(("unsworn.runtime", Attr::Str(runtime.to_string())));
            }
            attributes.push((
                "unsworn.in_handlers_ms",
                Attr::Double(answer.in_handlers.as_secs_f64() * 1000.0),
            ));
        }
        SpanRecord {
            trace_id: span.trace_id,
            span_id: span.span_id,
            parent: span.parent,
            name,
            kind: Kind::Server,
            start_unix_nano,
            end_unix_nano,
            attributes,
            events: Vec::new(),
            error: match answer {
                Some(answer) => failed_status(answer.status),
                None => Some(ABANDONED.to_string()),
            },
        }
    }
}

/// A recorded span that others can go below.
#[derive(Clone)]
pub struct Parent {
    tracer: Arc<Tracer>,
    trace_id: TraceId,
    span_id: SpanId,
    tracestate: Option<String>,
}

impl Parent {
    /// A request's own span. What goes below it is sent with the request's
    /// headers, which have its `tracestate` already.
    pub fn of(tracer: &Arc<Tracer>, span: &Started) -> Parent {
        Parent {
            tracer: tracer.clone(),
            trace_id: span.trace_id,
            span_id: span.span_id,
            tracestate: None,
        }
    }

    /// The span a handler was told is its parent, when it's a sampled one.
    /// With `[telemetry]` that's always one of the server's own: a sampled
    /// request goes on with the server's span, and any other keeps a header
    /// that isn't marked sampled, or none.
    pub fn from_trace(tracer: &Arc<Tracer>, trace: Option<&TraceContext>) -> Option<Parent> {
        let trace = trace?;
        let (trace_id, span_id, sampled) = parse(&trace.traceparent)?;
        sampled.then(|| Parent {
            tracer: tracer.clone(),
            trace_id,
            span_id,
            tracestate: trace.tracestate.clone(),
        })
    }

    /// Starts a span below this one. End it with [`ChildSpan::finish`].
    pub fn child(&self, name: String, attributes: Attributes) -> ChildSpan {
        ChildSpan {
            parent: self.clone(),
            span_id: SpanId::random(),
            name,
            attributes,
            started_at: SystemTime::now(),
            start: Instant::now(),
            finished: false,
        }
    }
}

/// A `CLIENT` span that's under way: something the server is asking of
/// something else. Dropped before it's finished, as when the request's time
/// runs out, it's recorded as cancelled.
pub struct ChildSpan {
    parent: Parent,
    span_id: SpanId,
    name: String,
    attributes: Attributes,
    started_at: SystemTime,
    start: Instant,
    finished: bool,
}

impl ChildSpan {
    /// The header for what's asked, which makes this span its parent.
    pub fn traceparent(&self) -> String {
        sampled_traceparent(self.parent.trace_id, self.span_id)
    }

    /// [`ChildSpan::traceparent`], with the `tracestate` that goes with it.
    pub fn context(&self) -> TraceContext {
        TraceContext {
            traceparent: self.traceparent(),
            tracestate: self.parent.tracestate.clone(),
        }
    }

    /// Adds an attribute.
    pub fn set(&mut self, key: &'static str, value: Attr) {
        self.attributes.push((key, value));
    }

    /// Records the span. `error` is why it failed, if it did.
    pub fn finish(mut self, error: Option<String>) {
        self.record(error);
    }

    fn record(&mut self, error: Option<String>) {
        self.finished = true;
        let (start_unix_nano, end_unix_nano) = span_times(self.started_at, self.start.elapsed());
        self.parent.tracer.record(SpanRecord {
            trace_id: self.parent.trace_id,
            span_id: self.span_id,
            parent: Some(self.parent.span_id),
            name: std::mem::take(&mut self.name),
            kind: Kind::Client,
            start_unix_nano,
            end_unix_nano,
            attributes: std::mem::take(&mut self.attributes),
            events: Vec::new(),
            error: error.map(|why| cut(&why).to_string()),
        });
    }
}

impl Drop for ChildSpan {
    fn drop(&mut self) {
        if !self.finished {
            self.record(Some(CANCELLED.to_string()));
        }
    }
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
    sink: Arc<Sink>,
    queue: Queue,
}

impl Tracer {
    /// The request's span, if it's sampled.
    pub fn start(&self, headers: &HeaderMap) -> Option<Started> {
        decide(self.sample, headers)
    }

    /// Queues a span to be sent. Never waits.
    pub fn record(&self, span: SpanRecord) {
        if self
            .queue
            .spans
            .try_send((self.sink.clone(), span))
            .is_err()
        {
            self.queue.dropped.fetch_add(1, Ordering::Relaxed);
        }
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

/// What the resource says: the service's name, from the config or else the
/// environment, the environment's other attributes, and what sent the
/// spans.
fn resource_attributes(
    configured: Option<&str>,
    service_env: Option<&str>,
    attributes_env: Option<&str>,
) -> Vec<(String, String)> {
    let decode = |s: &str| {
        percent_decode_str(s.trim())
            .decode_utf8_lossy()
            .into_owned()
    };
    let mut given: BTreeMap<String, String> = attributes_env
        .unwrap_or_default()
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), decode(value)))
        .filter(|(key, _)| !key.is_empty())
        .collect();
    let from_attributes = given.remove("service.name");
    let name = [configured, service_env, from_attributes.as_deref()]
        .into_iter()
        .flatten()
        .find(|name| !name.is_empty())
        .unwrap_or("unsworn");
    let sdk = [
        ("telemetry.sdk.name", "unsworn"),
        ("telemetry.sdk.language", "rust"),
        ("telemetry.sdk.version", VERSION),
    ];
    for (key, _) in sdk {
        given.remove(key);
    }
    let mut out = vec![("service.name".to_string(), name.to_string())];
    out.extend(given);
    out.extend(sdk.map(|(key, value)| (key.to_string(), value.to_string())));
    out
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
            if let Err(why) = post(&self.client, &sink.outbound, body).await {
                self.lost.failed += spans.len() as u64;
                self.lost.why = format!("[outbound.{}] {why}", sink.outbound.name);
            }
        }
    }
}

/// Sends one request to the collector, built as a handler's `fetch` is,
/// with the service's headers and `timeout`. Anything but a `2xx`, a
/// redirect included, is a failure.
async fn post(client: &HttpClient, service: &Outbound, body: Vec<u8>) -> Result<(), String> {
    let planned = outbound::plan(service, "POST", TRACES_PATH, &BTreeMap::new(), None)
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

    const SAMPLED: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    const UNSAMPLED: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

    fn headers(traceparent: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = traceparent {
            headers.insert(TRACEPARENT, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn an_incoming_traceparent_decides() {
        // Whatever `sample` is.
        for sample in [0.0, 1.0] {
            let s = decide(sample, &headers(Some(SAMPLED))).unwrap();
            assert_eq!(s.trace_id.hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
            assert_eq!(hex(&s.parent.unwrap().0), "00f067aa0ba902b7");
            assert_ne!(Some(s.span_id), s.parent);
            assert!(!s.new_root);
            assert!(decide(sample, &headers(Some(UNSAMPLED))).is_none());
        }
        // Other flags don't matter, only the sampled one.
        let flags = |f: &str| decide(0.0, &headers(Some(&format!("{}{f}", &SAMPLED[..53]))));
        assert!(flags("03").is_some());
        assert!(flags("02").is_none());
    }

    #[test]
    fn without_one_sample_decides() {
        for incoming in [None, Some("nonsense")] {
            assert!(decide(0.0, &headers(incoming)).is_none());
            let s = decide(1.0, &headers(incoming)).unwrap();
            assert!(s.new_root);
            assert_eq!(s.parent, None);
        }
        let taken = (0..10_000)
            .filter(|_| decide(0.5, &headers(None)).is_some())
            .count();
        assert!((4_500..=5_500).contains(&taken), "{taken}");
    }

    #[test]
    fn the_traceparent_passed_on_names_this_span() {
        let s = decide(1.0, &headers(None)).unwrap();
        assert_ne!(s.trace_id.0, [0; 16]);
        assert_ne!(s.span_id.0, [0; 8]);
        let sent = s.traceparent();
        assert!(valid_traceparent(&sent), "{sent}");
        assert_eq!(
            sent,
            format!("00-{}-{}-01", s.trace_id.hex(), hex(&s.span_id.0))
        );
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| SpanId::random().0).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn a_spans_name_and_path() {
        let ended = |method, path, answer| {
            let started = decide(1.0, &HeaderMap::new()).unwrap();
            SpanRecord::new(
                &started,
                Ended {
                    started_at: UNIX_EPOCH + Duration::from_secs(1),
                    elapsed: Duration::from_millis(2),
                    method,
                    path,
                    tls: false,
                    client: "127.0.0.1".parse().unwrap(),
                    version: http::Version::HTTP_11,
                    request_id: "r",
                    answer,
                },
            )
        };
        let answered = |method, route, path| {
            let answer = Outcome {
                route,
                status: 200,
                runtime: None,
                in_handlers: Duration::ZERO,
            };
            ended(method, path, Some(answer))
        };
        let span = answered("GET", "/users/{id}", "/users/7");
        assert_eq!(span.name, "GET /users/{id}");
        assert_eq!(span.kind, Kind::Server);
        assert_eq!(span.start_unix_nano, 1_000_000_000);
        assert_eq!(span.end_unix_nano, 1_002_000_000);
        let span = answered("BREW", "(none)", "/pot");
        assert_eq!(span.name, "_OTHER");
        assert_eq!(attr(&span, "http.route"), None);
        // Cut between characters.
        let long = format!("/{}", "\u{e9}".repeat(MAX_PATH));
        let span = answered("GET", "(none)", &long);
        let Some(Attr::Str(path)) = attr(&span, "url.path") else {
            panic!("{span:?}");
        };
        assert_eq!(path.len(), MAX_PATH - 1);
        // Never answered: no route or status to name.
        let span = ended("GET", "/users/7", None);
        assert_eq!(span.name, "GET");
        assert_eq!(attr(&span, "http.response.status_code"), None);
        assert_eq!(attr(&span, "unsworn.in_handlers_ms"), None);
        assert_eq!(span.error.as_deref(), Some(ABANDONED));
    }

    fn attr<'a>(span: &'a SpanRecord, key: &str) -> Option<&'a Attr> {
        span.attributes
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, value)| value)
    }

    /// A tracer whose spans stay in a queue the test reads.
    fn tracer() -> (Arc<Tracer>, mpsc::Receiver<Queued>) {
        let (spans, queued) = mpsc::channel(8);
        let tracer = Tracer {
            sample: 1.0,
            sink: Arc::new(Sink {
                outbound: Arc::new(Outbound::open("otel", "http://collector", Vec::new())),
                resource: Bytes::new(),
            }),
            queue: Queue {
                spans,
                dropped: Arc::default(),
            },
        };
        (Arc::new(tracer), queued)
    }

    fn trace(traceparent: &str) -> TraceContext {
        TraceContext {
            traceparent: traceparent.to_string(),
            tracestate: Some("vendor=1".to_string()),
        }
    }

    #[test]
    fn only_a_sampled_trace_is_a_parent() {
        let (tracer, _queued) = tracer();
        for not in [None, Some(trace(UNSAMPLED)), Some(trace("nonsense"))] {
            assert!(
                Parent::from_trace(&tracer, not.as_ref()).is_none(),
                "{not:?}"
            );
        }
        let parent = Parent::from_trace(&tracer, Some(&trace(SAMPLED))).unwrap();
        assert_eq!(parent.trace_id.hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(hex(&parent.span_id.0), "00f067aa0ba902b7");
    }

    #[test]
    fn a_child_is_passed_on_and_recorded_below_its_parent() {
        let (tracer, mut queued) = tracer();
        let parent = Parent::from_trace(&tracer, Some(&trace(SAMPLED))).unwrap();
        let mut child = parent.child("GET".to_string(), Vec::new());
        let sent = child.traceparent();
        assert_eq!(
            sent,
            format!(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-{}-01",
                hex(&child.span_id.0)
            )
        );
        assert_ne!(child.span_id, parent.span_id);
        assert_eq!(child.context(), trace(&sent));
        child.set("http.response.status_code", Attr::Int(200));
        let id = child.span_id;
        assert!(queued.try_recv().is_err());
        child.finish(None);

        let (_, span) = queued.try_recv().unwrap();
        assert_eq!(span.kind, Kind::Client);
        assert_eq!(span.trace_id, parent.trace_id);
        assert_eq!((span.span_id, span.parent), (id, Some(parent.span_id)));
        assert_eq!(span.name, "GET");
        assert_eq!(
            attr(&span, "http.response.status_code"),
            Some(&Attr::Int(200))
        );
        assert_eq!(span.error, None);
        assert!(span.end_unix_nano >= span.start_unix_nano);
        // Once.
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn steps_are_events_and_say_how_the_request_ended() {
        use unsworn_core::pipeline::{CacheStep, Rule};
        let steps = Steps::default();
        steps.note(Step::Authenticated("key"));
        steps.note(Step::Checked);
        steps.note(Step::Cache(CacheStep::Miss));
        steps.note(Step::Cache(CacheStep::Stored));
        steps.note(Step::Rejected(Rule::ResponseValidation));
        let (events, attributes) = steps.take();
        let names: Vec<&str> = events.iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "unsworn.authenticated",
                "unsworn.checked",
                "unsworn.cache",
                "unsworn.cache",
                "unsworn.rejected"
            ]
        );
        assert_eq!(
            events[4].attributes,
            [("unsworn.rule", Attr::Static("response_validation"))]
        );
        assert!(events[1].attributes.is_empty());
        assert!(
            events
                .windows(2)
                .all(|w| w[0].time_unix_nano <= w[1].time_unix_nano)
        );
        // The last of each, in the order first seen.
        assert_eq!(
            attributes,
            [
                ("unsworn.auth.scheme", Attr::Str("key".into())),
                ("unsworn.cache", Attr::Static("stored")),
                ("unsworn.rejected_by", Attr::Static("response_validation")),
            ]
        );
        assert!(steps.take().0.is_empty());
    }

    #[test]
    fn a_child_that_is_given_up_on_is_recorded_as_cancelled() {
        let (tracer, mut queued) = tracer();
        let parent = Parent::from_trace(&tracer, Some(&trace(SAMPLED))).unwrap();
        drop(parent.child("GET".to_string(), Vec::new()));
        let (_, span) = queued.try_recv().unwrap();
        assert_eq!(span.error.as_deref(), Some(CANCELLED));
    }

    #[test]
    fn resource_attributes_from_the_config_and_environment() {
        let name = |configured, service, attributes| {
            resource_attributes(configured, service, attributes)[0]
                .1
                .clone()
        };
        let attributes = Some("service.name=from-attributes");
        assert_eq!(
            name(Some("configured"), Some("from-env"), attributes),
            "configured"
        );
        assert_eq!(name(None, Some("from-env"), attributes), "from-env");
        assert_eq!(name(None, Some(""), attributes), "from-attributes");
        assert_eq!(name(None, None, None), "unsworn");

        let all = resource_attributes(
            None,
            None,
            Some("team = a%2Cb%20c ,broken,=none,deployment.environment=prod,telemetry.sdk.name=x"),
        );
        let all: Vec<(&str, &str)> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            all,
            [
                ("service.name", "unsworn"),
                ("deployment.environment", "prod"),
                ("team", "a,b c"),
                ("telemetry.sdk.name", "unsworn"),
                ("telemetry.sdk.language", "rust"),
                ("telemetry.sdk.version", VERSION),
            ]
        );
    }
}
