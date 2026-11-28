//! What a host provides to the pipeline: the I/O the core never does itself.

use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::HeaderMap;
use serde_json::{Map, Value};

use super::Problem;
use crate::policy::HandlerRef;
use crate::project::Proxy;
use crate::store::{RateLimiter, ResponseCache};
use crate::telemetry::{self, Attr};
use crate::{MaybeSend, MaybeSync};

/// A body the core passes around without reading: empty, bytes it holds,
/// or the host's own body type (a request still arriving, a streamed
/// handler reply, a proxied response).
pub enum Body<B> {
    Empty,
    Bytes(Bytes),
    Host(B),
}

impl<B> Body<B> {
    /// Bytes, or empty when there are none.
    pub fn bytes(b: Option<Bytes>) -> Self {
        match b {
            Some(b) if !b.is_empty() => Body::Bytes(b),
            _ => Body::Empty,
        }
    }
}

impl<B> std::fmt::Debug for Body<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Empty => f.write_str("Empty"),
            Body::Bytes(b) => write!(f, "Bytes({})", b.len()),
            Body::Host(_) => f.write_str("Host(..)"),
        }
    }
}

/// A response from the pipeline.
pub type Response<B> = http::Response<Body<B>>;

/// A point in time on the host's monotonic clock ([`Host::monotonic`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(pub Duration);

/// What a handler call produced.
pub enum Reply<B> {
    Response {
        response: unsworn_proto::Response,
        /// The response body. For a `json` or `text` response it may be
        /// left empty: the core encodes those.
        body: Body<B>,
    },
    /// A `before` handler let the request through, adding these to the
    /// context.
    Continue(Map<String, Value>),
}

/// Why a streamed request body couldn't be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    TooLarge,
    /// No data for [`BODY_IDLE`].
    Idle,
    Read,
}

/// How long a streamed request body may go without new data.
pub const BODY_IDLE: Duration = Duration::from_secs(30);

/// Why a handler call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The runtime couldn't start or has no healthy workers.
    Unavailable(String),
    /// The runtime reported that this handler doesn't exist.
    MissingHandler(String),
    Timeout,
    /// The worker exited or broke the protocol mid-request.
    WorkerFailed(String),
    /// The handler sent an `error` message.
    Handler(String),
    /// A streamed request body failed: too large, stalled or unreadable.
    Body(BodyError),
    /// The request's JSON body, which the pipeline left unread, turned out
    /// not to be JSON when the host had to read it. Holds the `400`'s
    /// detail.
    BadJson(String),
    /// The request's body isn't JSON, and the worker doesn't take raw
    /// bodies.
    RawBody,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unavailable(m) => write!(f, "runtime unavailable: {m}"),
            CallError::MissingHandler(m) => write!(f, "handler not found: {m}"),
            CallError::Timeout => write!(f, "the handler timed out"),
            CallError::WorkerFailed(m) => write!(f, "worker failed: {m}"),
            CallError::Handler(m) => write!(f, "handler error: {m}"),
            CallError::Body(BodyError::TooLarge) => write!(f, "the request body is too large"),
            CallError::Body(BodyError::Idle) => {
                write!(f, "the request body stalled for {}s", BODY_IDLE.as_secs())
            }
            CallError::Body(BodyError::Read) => write!(f, "couldn't read the request body"),
            CallError::BadJson(detail) => f.write_str(detail),
            CallError::RawBody => write!(
                f,
                "the body isn't JSON, and the worker takes no others (its `ready` has no `raw_body`)"
            ),
        }
    }
}

impl std::error::Error for CallError {}

/// A request for a file from a `static` route.
#[derive(Debug, Clone)]
pub struct StaticRequest {
    /// The route's `dir`, relative to the project root.
    pub dir: String,
    /// The path below the route, from its catch-all parameter.
    pub rest: String,
    pub index: String,
    pub spa_fallback: Option<String>,
    pub cache_control: Option<String>,
    pub method: String,
    pub headers: HeaderMap,
}

/// A request to forward to a `proxy` route's upstream.
pub struct ProxyRequest<B> {
    pub proxy: Arc<Proxy>,
    pub request: http::Request<Body<B>>,
    pub client_ip: IpAddr,
    /// The request's context, as Unsworn recorded it: what a function an
    /// `http` runtime's server calls for this request starts from.
    pub context: Map<String, Value>,
    /// When the request runs out of time, and so do the calls made for it.
    pub deadline: Deadline,
}

/// Why an outgoing request (a JWKS document, an API-key verifier) failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// No complete answer within the timeout.
    Timeout,
    /// Couldn't connect, or the connection failed.
    Connect(String),
    /// The response body couldn't be read, or was over the limit.
    Read(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Timeout => f.write_str("no answer in time"),
            FetchError::Connect(e) => write!(f, "can't connect: {e}"),
            FetchError::Read(e) => write!(f, "can't read the response: {e}"),
        }
    }
}

/// Something the pipeline decided about a request, for a host that records
/// it, such as on the request's span. See [`Host::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step<'a> {
    /// Auth let the caller in, with this scheme: the route's, or an MCP
    /// tool's.
    Authenticated(&'a str),
    /// Every check before `before` handlers passed: access rules, rate
    /// limits, auth, `require`, validation and reading the body.
    Checked,
    /// The cache was looked in, or filled.
    Cache(CacheStep),
    /// A handler's response didn't match its schema.
    ResponseInvalid,
    /// A rule answered the request instead of its action.
    Rejected(Rule),
}

impl Step<'_> {
    /// What spans say of the step, so every host names it the same.
    pub fn describe(&self) -> StepDescription {
        let (event, says, sets) = match *self {
            Step::Authenticated(scheme) => {
                let scheme = Attr::Str(telemetry::cut(scheme).to_string());
                let scheme = ("unsworn.auth.scheme", scheme);
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
        StepDescription { event, says, sets }
    }
}

/// What a [`Step`] adds to its request's span.
#[derive(Debug, Clone, PartialEq)]
pub struct StepDescription {
    /// The event's name.
    pub event: &'static str,
    /// The event's attribute.
    pub says: Option<(&'static str, Attr)>,
    /// The attribute the step sets on the span itself.
    pub sets: Option<(&'static str, Attr)>,
}

/// What the cache did for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStep {
    Hit,
    /// Nothing fresh was there.
    Miss,
    /// The response was stored, after a miss.
    Stored,
}

impl CacheStep {
    pub fn name(self) -> &'static str {
        match self {
            CacheStep::Hit => "hit",
            CacheStep::Miss => "miss",
            CacheStep::Stored => "stored",
        }
    }
}

/// A rule that can answer a request instead of its route's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Origin,
    Ip,
    RateLimit,
    Auth,
    Require,
    /// Path parameters, the query, headers, the body or an MCP tool's
    /// arguments didn't pass their checks.
    Validation,
    /// The body is larger than `max_body`.
    MaxBody,
    /// The body couldn't be read.
    Body,
    /// A `before` handler answered.
    Before,
    Timeout,
    /// The handler's response didn't match its schema, with `enforce`.
    ResponseValidation,
}

impl Rule {
    /// As the config names it.
    pub fn name(self) -> &'static str {
        match self {
            Rule::Origin => "origin",
            Rule::Ip => "ip",
            Rule::RateLimit => "rate_limit",
            Rule::Auth => "auth",
            Rule::Require => "require",
            Rule::Validation => "validation",
            Rule::MaxBody => "max_body",
            Rule::Body => "body",
            Rule::Before => "before",
            Rule::Timeout => "timeout",
            Rule::ResponseValidation => "response_validation",
        }
    }
}

/// The I/O a host does for the pipeline. One value serves one request, so
/// it can carry per-request facts such as the request id.
///
/// Every future a host returns is `Send` on native targets; see
/// [`MaybeSend`].
pub trait Host: MaybeSync {
    /// A request body as it arrives.
    type RequestBody: MaybeSend + 'static;
    /// A body the host produces: a streamed handler reply, a proxied
    /// response, a file.
    type ResponseBody: MaybeSend + 'static;
    /// Where rate limits and cached responses live.
    type Store: RateLimiter + ResponseCache + Clone + 'static;

    fn store(&self) -> Self::Store;

    /// The wall clock, in milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;

    /// A monotonic clock with an arbitrary zero, for deadlines and key
    /// refreshes.
    fn monotonic(&self) -> Duration;

    /// Development mode: error responses carry details.
    fn dev(&self) -> bool;

    /// Reads a request body whole, failing with `413` past `max` bytes and
    /// `400` when it can't be read.
    fn read_body(
        &self,
        body: Self::RequestBody,
        max: u64,
    ) -> impl Future<Output = Result<Bytes, Problem>> + MaybeSend;

    /// Wraps a request body so reading it fails past `max` bytes, for
    /// bodies passed on unread.
    fn limit_body(&self, body: Self::RequestBody, max: u64) -> Self::RequestBody;

    /// Reads a response body whole, if it's at most `limit` bytes.
    /// Otherwise, or when reading fails, gives back a body that yields the
    /// same data (what was read, then the rest, or the error), so the
    /// response can still be sent in full.
    fn collect(
        &self,
        body: Self::ResponseBody,
        limit: usize,
    ) -> impl Future<Output = Result<Bytes, Self::ResponseBody>> + MaybeSend;

    /// Calls a handler, or a `before` handler when `request.phase` says so.
    /// The call must end by `deadline`.
    fn call(
        &self,
        handler: &HandlerRef,
        request: unsworn_proto::Request,
        body: Body<Self::RequestBody>,
        deadline: Deadline,
    ) -> impl Future<Output = Result<Reply<Self::ResponseBody>, CallError>> + MaybeSend;

    /// Forwards a request to a `proxy` route's upstream. A host that can't
    /// proxy answers `501`; `unsworn check --target` reports such routes.
    fn proxy(
        &self,
        request: ProxyRequest<Self::RequestBody>,
    ) -> impl Future<Output = Response<Self::ResponseBody>> + MaybeSend;

    /// Serves a file for a `static` route.
    fn serve_static(
        &self,
        request: StaticRequest,
    ) -> impl Future<Output = Response<Self::ResponseBody>> + MaybeSend;

    /// Sends a request to another service, such as a JWKS endpoint or an
    /// API-key verifier, and reads up to `limit` bytes of the answer.
    fn fetch(
        &self,
        request: http::Request<Bytes>,
        timeout: Duration,
        limit: usize,
    ) -> impl Future<Output = Result<http::Response<Bytes>, FetchError>> + MaybeSend;

    /// Whether `call` takes a request whose `json` is text nobody has read
    /// (`unsworn_proto::Json::unchecked`): because the handler's worker
    /// parses it from the raw body, or because the host checks it first
    /// when a worker needs it inside the message.
    fn takes_unread_json(&self) -> bool {
        false
    }

    /// Runs `future` for at most `timeout`. `None` when it ran out of time.
    fn timeout<F>(
        &self,
        timeout: Duration,
        future: F,
    ) -> impl Future<Output = Option<F::Output>> + MaybeSend
    where
        F: Future + MaybeSend,
        F::Output: MaybeSend;

    /// Sends a copy of a request to a route's shadow handler and compares
    /// its answer with the one the route's own handler gave, without
    /// making the response wait. See [`super::shadow`]. A host that
    /// doesn't run shadows leaves this as it is.
    fn shadow(&self, call: super::shadow::ShadowCall) {
        let _ = call;
    }

    /// Told what the pipeline decided, as it does. A host that records
    /// spans puts it on the request's. Called on every request, so a host
    /// that isn't recording should return at once; the default does
    /// nothing.
    fn step(&self, step: Step<'_>) {
        let _ = step;
    }

    /// Runs `future` without making the response wait for it, such as a
    /// write to a remote store. A host may run it before returning when
    /// that's cheap, as the binary does for its in-memory store, so the
    /// next request sees the write.
    fn background(
        &self,
        future: impl Future<Output = ()> + MaybeSend + 'static,
    ) -> impl Future<Output = ()> + MaybeSend;
}
