//! Write [Unsworn](https://github.com/j4crev/unsworn) handlers in Rust.
//!
//! A handler is an async function that takes a request and returns a reply:
//!
//! ```no_run
//! use unsworn_sdk::{Reply, Request, Result, json};
//!
//! async fn get_user(req: Request) -> Result {
//!     // The id param is already validated and converted by Unsworn.
//!     let id: u64 = req.param("id")?;
//!     Ok(Reply::json(json!({ "id": id, "name": "Ada" })))
//! }
//!
//! fn main() {
//!     unsworn_sdk::run(unsworn_sdk::handlers![get_user]);
//! }
//! ```
//!
//! Unsworn builds and runs the worker, from `unsworn.toml`:
//!
//! ```toml
//! [runtimes.rust]
//! extensions = [".rs"]
//! build      = "cargo build --release --bin handlers"
//! command    = "target/release/handlers"
//! protocol   = "framed"
//! ```
//!
//! The reference `src/users.rs#get_user` finds the function registered as
//! `get_user`. See [`Handlers`].

mod body;
mod handlers;
mod logging;
mod worker;

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::sync::Notify;

pub use body::BodyStream;
pub use handlers::Handlers;
pub use logging::LogLayer;
pub use serde_json::{Value as JsonValue, json};
pub use unsworn_proto::Phase;
pub use worker::{run, serve};

/// The SDK version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a handler returns. `Result` alone means `Result<Reply, Error>`.
pub type Result<T = Reply, E = Error> = std::result::Result<T, E>;

/// A handler error. Any error converts into it with `?`, and
/// [`Error::msg`] makes one from a message. The client gets a `500`; the
/// message is logged, and shown to the client only under `unsworn dev`.
pub struct Error(Box<dyn StdError + Send + Sync + 'static>);

impl Error {
    pub fn msg(message: impl fmt::Display) -> Error {
        Error(message.to_string().into())
    }
}

impl<E: StdError + Send + Sync + 'static> From<E> for Error {
    fn from(e: E) -> Error {
        Error(Box::new(e))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// A request that Unsworn has already matched, authenticated and validated.
#[derive(Debug)]
pub struct Request {
    /// Unsworn's id for this request, unique per worker connection.
    pub id: String,
    pub phase: Phase,
    /// The handler reference, such as `src/users.rs#get_user`.
    pub handler: String,
    /// The matched route pattern, such as `/users/{id}`.
    pub route: String,
    pub method: String,
    pub path: String,
    /// Path parameters, already converted to their declared types.
    pub params: Map<String, Value>,
    /// Query parameters, already converted to their declared types.
    pub query: Map<String, Value>,
    /// The query string as sent, without the `?`.
    pub raw_query: Option<String>,
    /// Request headers, with lowercase names.
    pub headers: BTreeMap<String, String>,
    /// The verified credentials under `"auth"`, plus anything before
    /// handlers added with [`Reply::next`].
    pub ctx: Map<String, Value>,
    /// The JSON body, when the request had one. See [`Request::json`] and
    /// [`Request::body_json`].
    json: JsonBody,
    /// The text body, when the request had one.
    pub text: Option<String>,
    /// The raw body, when the request had one.
    pub body: Option<Bytes>,
    stream: Option<BodyStream>,
    cancel: Cancel,
    calls: Option<worker::Calls>,
}

/// A function answered with an error status, or couldn't be called. See
/// [`Request::call`].
#[derive(Debug, Clone)]
pub struct CallError {
    /// The function's own status, or `404` for no such function, `422`
    /// for input its schema refuses and `504` for a timeout.
    pub status: u16,
    pub message: String,
    /// What the function answered with, if anything.
    pub json: Value,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for CallError {}

impl Request {
    /// Calls a function: a handler in any language, declared in the
    /// config as `[[function]]`. Unsworn checks `input` against the
    /// function's schema, runs it with this request's context and what is
    /// left of its time, and gives back the JSON it answered with.
    ///
    /// Fails with a [`CallError`] when the function answers `400` or
    /// above, or can't be called. `?` turns that into a `500` for this
    /// request; match on it to do something else:
    ///
    /// ```ignore
    /// let vector: Vec<f32> = req.call("embed", json!({ "text": text })).await?;
    /// ```
    pub async fn call<T: DeserializeOwned>(
        &self,
        name: &str,
        input: impl serde::Serialize,
    ) -> std::result::Result<T, CallError> {
        let fail = |status, message: String| CallError {
            status,
            message,
            json: Value::Null,
        };
        let Some(calls) = &self.calls else {
            return Err(fail(
                501,
                "call works in a handler that Unsworn is running".into(),
            ));
        };
        let input = serde_json::to_value(input)
            .map_err(|e| fail(400, format!("can't encode the input for \"{name}\": {e}")))?;
        let json = calls.call(&self.id, name, input).await?;
        serde_json::from_value(json)
            .map_err(|e| fail(502, format!("the answer from \"{name}\" doesn't fit: {e}")))
    }

    /// A request header. `name` is matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// A path parameter, converted to `T`.
    pub fn param<T: DeserializeOwned>(&self, name: &str) -> Result<T> {
        get(&self.params, "path parameter", name)
    }

    /// A query parameter, converted to `T`.
    pub fn query_value<T: DeserializeOwned>(&self, name: &str) -> Result<T> {
        get(&self.query, "query parameter", name)
    }

    /// All path parameters, as a struct.
    pub fn params<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_value(Value::Object(self.params.clone()))?)
    }

    /// All query parameters, as a struct.
    pub fn query<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_value(Value::Object(self.query.clone()))?)
    }

    /// Whether the request has a JSON body.
    pub fn has_json(&self) -> bool {
        !matches!(self.json, JsonBody::None)
    }

    /// The JSON body as a value, or `None` when the request has none. It's
    /// parsed the first time this is called, so a handler that never asks
    /// doesn't pay for it. For a type of your own, [`Request::body_json`]
    /// reads the body once, straight into it.
    ///
    /// Unsworn may not have read the body, so it may turn out not to be
    /// JSON. Return that error (with `?`) and the client gets a `400`.
    pub fn json(&self) -> Result<Option<&Value>> {
        match &self.json {
            JsonBody::None => Ok(None),
            JsonBody::Value(value) => Ok(Some(value)),
            JsonBody::Text { text, parsed } => {
                if let Some(value) = parsed.get() {
                    return Ok(Some(value));
                }
                let value = serde_json::from_slice(text).map_err(body_error)?;
                Ok(Some(parsed.get_or_init(|| value)))
            }
        }
    }

    /// The JSON body, as `T`. An error for a body that isn't JSON at all
    /// gives the client a `400` when the handler returns it. One for JSON
    /// of the wrong shape is an ordinary handler error.
    pub fn body_json<T: DeserializeOwned>(&self) -> Result<T> {
        match &self.json {
            JsonBody::None => Err(Error::msg("the request has no JSON body")),
            JsonBody::Value(value) => Ok(T::deserialize(value)?),
            JsonBody::Text { text, parsed } => match parsed.get() {
                Some(value) => Ok(T::deserialize(value)?),
                None => serde_json::from_slice(text).map_err(body_error),
            },
        }
    }

    /// The body as it arrives, on routes with `stream_body = true`. Returns
    /// `None` on other routes, or if it was already taken.
    pub fn body_stream(&mut self) -> Option<BodyStream> {
        self.stream.take()
    }

    /// Whether Unsworn has cancelled the request: the client went away or
    /// the route's timeout passed.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolves when Unsworn cancels the request. A handler's future is
    /// dropped on cancel anyway; use this for work it spawned.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await
    }

    /// The MCP tool call this request is, or `None` when the handler isn't
    /// running as an MCP tool. The tool's arguments are the JSON body; see
    /// [`Request::body_json`].
    pub fn mcp(&self) -> Option<McpCall> {
        let call = self.ctx.get("mcp")?;
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(String::from);
        Some(McpCall {
            tool: text(call, "tool")?,
            protocol_version: text(call, "protocol_version").unwrap_or_default(),
            client: call.get("client").and_then(|c| {
                Some(McpClient {
                    name: text(c, "name")?,
                    version: text(c, "version").unwrap_or_default(),
                })
            }),
        })
    }
}

/// An MCP tool call. See [`Request::mcp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCall {
    /// The tool's name.
    pub tool: String,
    /// The call's MCP protocol version, such as `2026-07-28`.
    pub protocol_version: String,
    /// What the client says about itself, if anything. Use it for logs, not
    /// decisions.
    pub client: Option<McpClient>,
}

/// An MCP client's name and version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpClient {
    pub name: String,
    pub version: String,
}

/// A request's JSON body.
#[derive(Debug)]
pub(crate) enum JsonBody {
    None,
    /// Sent inside the message, so it arrived parsed.
    Value(Value),
    /// Sent as the bytes after the message, which nothing may have read.
    /// `parsed` is filled when a handler first asks for the value.
    Text {
        text: Bytes,
        parsed: std::sync::OnceLock<Value>,
    },
}

impl JsonBody {
    pub(crate) fn text(text: Bytes) -> JsonBody {
        JsonBody::Text {
            text,
            parsed: std::sync::OnceLock::new(),
        }
    }
}

/// A body that was sent as JSON and isn't. The worker answers `400` for a
/// handler error that is one of these.
#[derive(Debug)]
pub(crate) struct BadJson(pub(crate) String);

impl fmt::Display for BadJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the body isn't valid JSON: {}", self.0)
    }
}

impl StdError for BadJson {}

/// A parse error as a handler error: [`BadJson`] unless the text was JSON
/// and only didn't fit the type asked for.
fn body_error(e: serde_json::Error) -> Error {
    match e.classify() {
        serde_json::error::Category::Data => e.into(),
        _ => BadJson(e.to_string()).into(),
    }
}

impl Request {
    #[cfg(test)]
    pub(crate) fn test() -> Request {
        Request {
            id: "r-1".into(),
            phase: Phase::Handle,
            handler: String::new(),
            route: String::new(),
            method: "GET".into(),
            path: "/".into(),
            params: Map::new(),
            query: Map::new(),
            raw_query: None,
            headers: BTreeMap::new(),
            ctx: Map::new(),
            json: JsonBody::None,
            text: None,
            body: None,
            stream: None,
            cancel: Cancel::default(),
            calls: None,
        }
    }
}

fn get<T: DeserializeOwned>(map: &Map<String, Value>, what: &str, name: &str) -> Result<T> {
    let value = map
        .get(name)
        .ok_or_else(|| Error::msg(format!("no {what} named \"{name}\"")))?;
    T::deserialize(value).map_err(|e| Error::msg(format!("{what} \"{name}\": {e}")))
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Cancel(Arc<(AtomicBool, Notify)>);

impl Cancel {
    pub(crate) fn cancel(&self) {
        self.0.0.store(true, Ordering::SeqCst);
        self.0.1.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.0.0.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.0.1.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) enum Body {
    Empty,
    Json(std::result::Result<Value, String>),
    Text(String),
    Bytes(Bytes),
    Stream(BoxStream<'static, Result<Bytes>>),
    Next(Value),
}

/// What a handler returns: build one with [`Reply::json`], [`Reply::text`],
/// [`Reply::bytes`], [`Reply::stream`], [`Reply::status`] or
/// [`Reply::next`]. The default is a `204` with no body.
pub struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Body,
    /// MCP content blocks, for handlers that run as MCP tools.
    pub(crate) mcp_content: Option<Value>,
}

impl Default for Reply {
    fn default() -> Self {
        Reply::status(204)
    }
}

impl Reply {
    fn with_body(status: u16, body: Body) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body,
            mcp_content: None,
        }
    }

    /// `200` with `value` encoded as JSON.
    pub fn json(value: impl Serialize) -> Reply {
        Reply::with_body(
            200,
            Body::Json(serde_json::to_value(value).map_err(|e| e.to_string())),
        )
    }

    /// `200` with a text body.
    pub fn text(text: impl Into<String>) -> Reply {
        Reply::with_body(200, Body::Text(text.into()))
    }

    /// `200` with a raw body.
    pub fn bytes(body: impl Into<Bytes>) -> Reply {
        Reply::with_body(200, Body::Bytes(body.into()))
    }

    /// A status with no body.
    pub fn status(code: u16) -> Reply {
        Reply::with_body(code, Body::Empty)
    }

    /// `200` with a streamed body: each item is sent to the client as soon
    /// as it's ready. An `Err` item ends the stream with an error. Use it for
    /// server-sent events, large downloads and generated output.
    pub fn stream<S, B>(stream: S) -> Reply
    where
        S: Stream<Item = Result<B>> + Send + 'static,
        B: Into<Bytes>,
    {
        Reply::with_body(
            200,
            Body::Stream(stream.map(|item| item.map(Into::into)).boxed()),
        )
    }

    /// `200` with MCP content blocks, for a handler that runs as an MCP tool.
    /// `blocks` is a JSON array of MCP content blocks, such as
    /// `json!([{ "type": "image", "data": b64, "mimeType": "image/png" }])`.
    /// Add structured content with `Reply::json(v).with_content(blocks)`
    /// instead.
    pub fn content(blocks: Value) -> Reply {
        Reply::status(200).with_content(blocks)
    }

    /// An error the model sees and can react to, such as "city not found".
    /// Unsworn turns it into an MCP tool result with `isError` set. As an
    /// HTTP reply, it's a `400` with `{"error": message}`; change that with
    /// [`Reply::with_status`].
    pub fn tool_error(message: impl Into<String>) -> Reply {
        Reply::json(serde_json::json!({ "error": message.into() })).with_status(400)
    }

    /// Adds MCP content blocks, for a handler that runs as an MCP tool. See
    /// [`Reply::content`].
    pub fn with_content(mut self, blocks: Value) -> Reply {
        self.mcp_content = Some(blocks);
        self
    }

    /// From a before handler: let the request continue, adding the fields of
    /// `context` (a JSON object) to its context.
    pub fn next(context: Value) -> Reply {
        Reply::with_body(0, Body::Next(context))
    }

    /// Sets the status.
    pub fn with_status(mut self, code: u16) -> Reply {
        self.status = code;
        self
    }

    /// Adds a response header. Adding the same name again sends it several
    /// times, as `set-cookie` needs.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Reply {
        self.headers
            .push((name.into().to_ascii_lowercase(), value.into()));
        self
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn mcp_call_and_replies() {
        let mut req = Request::test();
        assert!(req.mcp().is_none());
        req.ctx.insert(
            "mcp".into(),
            json!({"tool": "get_forecast", "protocol_version": "2026-07-28", "client": {"name": "c", "version": "1"}}),
        );
        let call = req.mcp().unwrap();
        assert_eq!(call.tool, "get_forecast");
        assert_eq!(call.client.unwrap().name, "c");

        let e = Reply::tool_error("city not found");
        assert_eq!(e.status, 400);
        assert!(matches!(&e.body, Body::Json(Ok(v)) if v["error"] == "city not found"));
        let c = Reply::content(json!([{"type": "text", "text": "hi"}]));
        assert_eq!((c.status, c.mcp_content.is_some()), (200, true));
    }

    #[test]
    fn the_json_body_is_read_on_request() {
        #[derive(serde::Deserialize)]
        struct Args {
            n: u32,
        }
        let with = |text: &'static str| Request {
            json: JsonBody::text(Bytes::from_static(text.as_bytes())),
            ..Request::test()
        };
        let req = with(r#"{"n": 2}"#);
        assert!(req.has_json());
        assert_eq!(req.body_json::<Args>().unwrap().n, 2);
        assert_eq!(req.json().unwrap(), Some(&json!({"n": 2})));
        // The value is kept, and `body_json` reads from it afterwards.
        assert!(std::ptr::eq(
            req.json().unwrap().unwrap(),
            req.json().unwrap().unwrap()
        ));
        assert_eq!(req.body_json::<Args>().unwrap().n, 2);

        // Not JSON at all is the client's mistake.
        let bad = |e: Error| e.0.downcast_ref::<BadJson>().is_some();
        assert!(with(r#"{"n": "#).json().is_err_and(bad));
        assert!(with(r#"{"n": "#).body_json::<Args>().is_err_and(bad));
        // JSON of another shape is the handler's to answer for.
        assert!(
            with(r#"{"n": "two"}"#)
                .body_json::<Args>()
                .is_err_and(|e| !bad(e))
        );

        let none = Request::test();
        assert!(!none.has_json());
        assert_eq!(none.json().unwrap(), None);
        assert!(none.body_json::<Args>().is_err());
        let inside = Request {
            json: JsonBody::Value(json!({"n": 3})),
            ..Request::test()
        };
        assert_eq!(inside.body_json::<Args>().unwrap().n, 3);
    }
}
