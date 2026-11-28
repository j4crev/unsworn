//! Spans: what a host records about a sampled request, built the same way
//! by every host. See `docs/telemetry-design.md`.
//!
//! The core reads no clock and draws no random numbers, so a host gives it
//! both through [`Recorder`], and sends what's recorded to the `[outbound]`
//! service `[telemetry]` names, encoded by [`otlp`].

pub mod otlp;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{HeaderMap, Uri};
use percent_encoding::percent_decode_str;

use crate::VERSION;
use crate::pipeline::Step;
use crate::trace::{TRACEPARENT, TRACESTATE, TraceContext, valid_traceparent};

/// The methods spans and metrics name.
pub const KNOWN_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT", "TRACE",
];

/// How much a span keeps of a path, a name or an error. Someone else picks
/// their length, and spans wait in memory.
const MAX_TEXT: usize = 2048;

/// What a span says when what it covers was given up on before it ended.
const CANCELLED: &str = "cancelled before an answer: the request timed out or ended";

/// What a request's own span says when it was never answered.
const ABANDONED: &str = "cancelled before an answer: the client went away";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanId(pub [u8; 8]);

impl TraceId {
    /// A new one. All zeros means no trace, so that's never given.
    fn random(random: &mut dyn FnMut() -> u64) -> TraceId {
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

    pub fn from_hex(text: &str) -> Option<TraceId> {
        if text.len() != 32 {
            return None;
        }
        let bytes = unhex::<16>(text)?;
        (bytes != [0; 16]).then_some(TraceId(bytes))
    }
}

impl SpanId {
    /// A new one, never all zeros.
    fn random(random: &mut dyn FnMut() -> u64) -> SpanId {
        loop {
            let id = random().to_be_bytes();
            if id != [0; 8] {
                return SpanId(id);
            }
        }
    }

    pub fn hex(&self) -> String {
        hex(&self.0)
    }

    pub fn from_hex(text: &str) -> Option<SpanId> {
        if text.len() != 16 {
            return None;
        }
        let bytes = unhex::<8>(text)?;
        (bytes != [0; 8]).then_some(SpanId(bytes))
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
pub fn sampled_traceparent(trace_id: TraceId, span_id: SpanId) -> String {
    format!("00-{}-{}-01", trace_id.hex(), span_id.hex())
}

/// A `traceparent` a receiver would accept: its trace id, its parent id and
/// whether it's marked sampled.
pub fn parse(value: &str) -> Option<(TraceId, SpanId, bool)> {
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

/// What's decided about a request.
#[derive(Debug, Clone)]
pub enum Decision {
    /// It's recorded.
    Sampled(Started),
    /// It isn't.
    NotSampled {
        /// The `traceparent` to pass on in place of the one that came: the
        /// same one, marked unsampled. Only when someone whose word isn't
        /// taken marked it sampled, so that what's behind the host doesn't
        /// record a request the host declined to.
        cleared: Option<String>,
    },
}

/// Whether a request is sampled. A `traceparent` from a `trusted` peer, one
/// of `[server].trusted_proxies`, follows its sampled flag. From anyone
/// else it only says where the request's span goes in its trace, and
/// `sample` is the share taken, as it is of requests without one.
pub fn decide(
    sample: f64,
    headers: &HeaderMap,
    trusted: bool,
    random: &mut dyn FnMut() -> u64,
) -> Decision {
    let incoming = headers
        .get(TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .and_then(parse);
    if let Some((trace_id, parent, flagged)) = incoming {
        let sampled = match trusted {
            true => flagged,
            // A fresh draw: whoever sent the trace id could have picked it
            // to be taken.
            false => sample > 0.0 && taken(sample, random()),
        };
        if sampled {
            return Decision::Sampled(Started {
                trace_id,
                parent: Some(parent),
                span_id: SpanId::random(random),
                new_root: false,
            });
        }
        let cleared =
            (!trusted && flagged).then(|| format!("00-{}-{}-00", trace_id.hex(), parent.hex()));
        return Decision::NotSampled { cleared };
    }
    if sample > 0.0 {
        let trace_id = TraceId::random(random);
        // Decided by the id itself, its last 8 bytes.
        let mut low = [0; 8];
        low.copy_from_slice(&trace_id.0[8..]);
        if taken(sample, u64::from_be_bytes(low)) {
            return Decision::Sampled(Started {
                trace_id,
                parent: None,
                span_id: SpanId::random(random),
                new_root: true,
            });
        }
    }
    Decision::NotSampled { cleared: None }
}

/// Whether a draw falls in the share `sample` takes.
fn taken(sample: f64, draw: u64) -> bool {
    // The draw's top 53 bits, as a number from 0 up to 1.
    let share = (draw >> 11) as f64 / (1u64 << 53) as f64;
    sample >= 1.0 || share < sample
}

/// What a host knows about a request once it's over.
pub struct Ended<'a> {
    /// When it started, in nanoseconds since the Unix epoch.
    pub start_unix_nano: u64,
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
    pub runtime: Option<&'a str>,
    pub in_handlers: Duration,
}

/// What a span is to the host. The numbers are OTLP's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A request the host answered.
    Server = 2,
    /// Something the host asked of something else.
    Client = 3,
}

/// An attribute's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    Str(String),
    Static(&'static str),
    Int(i64),
    Double(f64),
    Bool(bool),
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
    /// Notes a step taken at `time_unix_nano`.
    pub fn note(&self, step: Step<'_>, time_unix_nano: u64) {
        let said = step.describe();
        let event = Event {
            name: said.event,
            time_unix_nano,
            attributes: said.says.into_iter().collect(),
        };
        self.noted.lock().unwrap().push((event, said.sets));
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

/// At most 2,048 bytes of `text`, cut between characters.
pub fn cut(text: &str) -> &str {
    let mut end = text.len().min(MAX_TEXT);
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

/// When a span that started at `start_unix_nano` and took `elapsed` ended.
fn end_time(start_unix_nano: u64, elapsed: Duration) -> u64 {
    let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    start_unix_nano.saturating_add(elapsed)
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
            start_unix_nano: ended.start_unix_nano,
            end_unix_nano: end_time(ended.start_unix_nano, ended.elapsed),
            attributes,
            events: Vec::new(),
            error: match answer {
                Some(answer) => failed_status(answer.status),
                None => Some(ABANDONED.to_string()),
            },
        }
    }
}

/// A moment, as a host's clocks give it.
#[derive(Debug, Clone, Copy)]
pub struct Moment {
    /// The wall clock, in nanoseconds since the Unix epoch.
    pub unix_nano: u64,
    /// A clock that never goes back, from any fixed start.
    pub monotonic: Duration,
}

/// Where a host's spans go, with the clock and the randomness they need.
pub trait Recorder {
    /// Takes a span to be sent. Never waits.
    fn record(&self, span: SpanRecord);
    fn now(&self) -> Moment;
    /// A number to make ids from. Ids have to differ, not to be secret.
    fn random(&self) -> u64;
}

impl<T: Recorder + ?Sized> Recorder for Arc<T> {
    fn record(&self, span: SpanRecord) {
        (**self).record(span);
    }

    fn now(&self) -> Moment {
        (**self).now()
    }

    fn random(&self) -> u64 {
        (**self).random()
    }
}

/// A recorded span that others can go below.
#[derive(Clone)]
pub struct Parent<R> {
    recorder: R,
    trace_id: TraceId,
    span_id: SpanId,
    tracestate: Option<String>,
}

impl<R: Recorder + Clone> Parent<R> {
    /// A request's own span. What goes below it is sent with the request's
    /// headers, which have its `tracestate` already.
    pub fn of(recorder: &R, span: &Started) -> Parent<R> {
        Parent {
            recorder: recorder.clone(),
            trace_id: span.trace_id,
            span_id: span.span_id,
            tracestate: None,
        }
    }

    /// The span a trace names, when it's a sampled one. It's one of the
    /// host's own, which a handler was told is its parent, or one the
    /// handler named for a call or a fetch. Either way the parent decides:
    /// nothing is recorded below one that isn't marked sampled.
    pub fn from_trace(recorder: &R, trace: Option<&TraceContext>) -> Option<Parent<R>> {
        let trace = trace?;
        let (trace_id, span_id, sampled) = parse(&trace.traceparent)?;
        sampled.then(|| Parent {
            recorder: recorder.clone(),
            trace_id,
            span_id,
            tracestate: trace.tracestate.clone(),
        })
    }

    /// Starts a span below this one, from what one of the builders here,
    /// such as [`handler_span`], says of it. End it with
    /// [`ChildSpan::finish`].
    pub fn child(&self, (name, attributes): (String, Attributes)) -> ChildSpan<R> {
        ChildSpan {
            parent: self.clone(),
            span_id: SpanId::random(&mut || self.recorder.random()),
            name,
            attributes,
            start: self.recorder.now(),
            finished: false,
        }
    }
}

/// A `CLIENT` span that's under way: something the host is asking of
/// something else. Dropped before it's finished, as when the request's time
/// runs out, it's recorded as cancelled.
pub struct ChildSpan<R: Recorder> {
    parent: Parent<R>,
    span_id: SpanId,
    name: String,
    attributes: Attributes,
    start: Moment,
    finished: bool,
}

impl<R: Recorder> ChildSpan<R> {
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

    /// Records the span of something that was answered with `status`.
    pub fn answered(mut self, status: u16) {
        self.set("http.response.status_code", Attr::Int(i64::from(status)));
        self.finish(failed_status(status));
    }

    fn record(&mut self, error: Option<String>) {
        self.finished = true;
        let elapsed = self
            .parent
            .recorder
            .now()
            .monotonic
            .saturating_sub(self.start.monotonic);
        self.parent.recorder.record(SpanRecord {
            trace_id: self.parent.trace_id,
            span_id: self.span_id,
            parent: Some(self.parent.span_id),
            name: std::mem::take(&mut self.name),
            kind: Kind::Client,
            start_unix_nano: self.start.unix_nano,
            end_unix_nano: end_time(self.start.unix_nano, elapsed),
            attributes: std::mem::take(&mut self.attributes),
            events: Vec::new(),
            error: error.map(|why| cut(&why).to_string()),
        });
    }
}

impl<R: Recorder> Drop for ChildSpan<R> {
    fn drop(&mut self) {
        if !self.finished {
            self.record(Some(CANCELLED.to_string()));
        }
    }
}

/// The span of a handler call, or of a `before` handler's.
pub fn handler_span(handler: &str, runtime: &str, handles: bool) -> (String, Attributes) {
    let what = if handles { "handler" } else { "before" };
    let attributes = vec![
        ("unsworn.handler", Attr::Str(handler.to_string())),
        ("unsworn.runtime", Attr::Str(runtime.to_string())),
    ];
    (format!("{what} {handler}"), attributes)
}

/// The span of a request the pipeline makes for itself: a JWKS document or
/// a key verifier's verdict.
pub fn fetch_span(method: &str, uri: &Uri) -> (String, Attributes) {
    let method = known_method(method);
    let mut attributes = vec![("http.request.method", Attr::Static(method))];
    if let Some(host) = uri.host() {
        attributes.push(("server.address", Attr::Str(host.to_string())));
    }
    // Without the query string, which may hold a credential.
    attributes.push(("url.path", Attr::Str(cut(uri.path()).to_string())));
    (method.to_string(), attributes)
}

/// The span of a request forwarded to a `proxy` route's upstream, or to the
/// server of the `http` runtime `runtime`. Which upstream and path it went
/// to is added once that's known.
pub fn proxy_span(method: &str, runtime: Option<&str>) -> (String, Attributes) {
    let method = known_method(method);
    let mut attributes = vec![("http.request.method", Attr::Static(method))];
    if let Some(name) = runtime {
        attributes.push(("unsworn.upstream", Attr::Str(format!("runtime:{name}"))));
    }
    (method.to_string(), attributes)
}

/// The span of a call from the handler `caller` to the function `function`.
pub fn call_span(function: &str, caller: &str) -> (String, Attributes) {
    // A handler can ask for any name.
    let function = cut(function);
    let attributes = vec![
        ("unsworn.function", Attr::Str(function.to_string())),
        ("unsworn.caller", Attr::Str(caller.to_string())),
    ];
    (format!("call {function}"), attributes)
}

/// The span of a handler's request to the outbound service `service`, whose
/// URL is `url` when there's one of that name. `path` is the one asked for.
pub fn service_span(
    method: &str,
    service: &str,
    url: Option<&str>,
    path: &str,
) -> (String, Attributes) {
    let method = known_method(&method.to_ascii_uppercase());
    let mut attributes = vec![
        ("http.request.method", Attr::Static(method)),
        ("unsworn.service", Attr::Str(cut(service).to_string())),
    ];
    let address = url.and_then(|url| url.parse::<Uri>().ok());
    if let Some(host) = address.as_ref().and_then(Uri::host) {
        attributes.push(("server.address", Attr::Str(host.to_string())));
    }
    let path = path.split('?').next().unwrap_or_default();
    attributes.push(("url.path", Attr::Str(cut(path).to_string())));
    (method.to_string(), attributes)
}

/// An upstream as a span names it: without a user and password or a query,
/// which are credentials the collector has no use for.
pub fn without_credentials(upstream: &str) -> String {
    let upstream = upstream.split(['?', '#']).next().unwrap_or_default();
    let Some((scheme, rest)) = upstream.split_once("://") else {
        return upstream.to_string();
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority.rsplit('@').next().unwrap_or_default();
    format!("{scheme}://{host}{path}")
}

/// The trace among the headers a handler gives a `fetch`, if they name one
/// a receiver would accept.
pub fn outbound_trace(headers: &BTreeMap<String, String>) -> Option<TraceContext> {
    TraceContext::from_headers(|name| {
        let found = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name));
        found.map(|(_, value)| value.as_str())
    })
}

/// A handler's headers without its trace's, for a span's own to go in
/// their place.
pub fn strip_trace(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let of_trace = |name: &str| {
        name.eq_ignore_ascii_case(TRACEPARENT) || name.eq_ignore_ascii_case(TRACESTATE)
    };
    headers
        .iter()
        .filter(|(name, _)| !of_trace(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// What a resource says: the service's name, from the config or else the
/// environment, the environment's other attributes, and what sent the
/// spans. `service_env` is `OTEL_SERVICE_NAME`, and `attributes_env` is
/// `OTEL_RESOURCE_ATTRIBUTES`.
pub fn resource_attributes(
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

#[cfg(test)]
mod tests;
