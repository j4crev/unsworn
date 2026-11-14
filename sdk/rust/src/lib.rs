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
//! protocol   = "binary"
//! ```
//!
//! The reference `src/users.rs#get_user` finds the function registered as
//! `get_user`. See [`Handlers`].

mod handlers;
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

pub use handlers::Handlers;
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
    /// The JSON body, when the request had one.
    pub json: Option<Value>,
    /// The text body, when the request had one.
    pub text: Option<String>,
    /// The raw body, when the request had one.
    pub body: Option<Bytes>,
    cancel: Cancel,
}

impl Request {
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

    /// The JSON body, as `T`.
    pub fn body_json<T: DeserializeOwned>(&self) -> Result<T> {
        match &self.json {
            Some(v) => Ok(T::deserialize(v)?),
            None => Err(Error::msg("the request has no JSON body")),
        }
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
            json: None,
            text: None,
            body: None,
            cancel: Cancel::default(),
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
