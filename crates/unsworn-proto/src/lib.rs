//! The Unsworn handler protocol.
//!
//! One message model ([`Message`]) with two encodings:
//!
//! - [`jsonl`]: Tier 1. One JSON message per line, optionally followed by
//!   raw body bytes. Used over a worker's stdin/stdout.
//! - [`framed`]: Tier 2. Length-prefixed frames carrying the same JSON
//!   message plus raw body bytes. Used over a Unix socket.
//!
//! A message may carry a raw body: its `body_len` field says how many bytes
//! follow it. [`Frame`] pairs a message with those bytes. See
//! `docs/protocol.md` for the full specification.

use std::collections::BTreeMap;

use bytes::{Buf, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The protocol version this crate implements.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default upper bound for a single encoded message, including its body.
pub const DEFAULT_MAX_FRAME: usize = 64 * 1024 * 1024;

/// A protocol message. Serialized with a `type` field naming the variant.
/// Read one with [`Message::from_json`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Ready(Ready),
    Request(Request),
    Response(Response),
    Continue(Continue),
    Chunk(Chunk),
    End(End),
    Cancel(Cancel),
    Error(ErrorMessage),
    Verify(Verify),
    Verified(Verified),
    Log(Log),
    Ack(Ack),
    Call(Call),
    Result(CallResult),
    Fetch(Fetch),
    Fetched(Fetched),
    /// A message type this version doesn't know. Receivers ignore it.
    Unknown,
}

impl Message {
    /// Reads a message from its JSON text.
    ///
    /// The `type` is found first and the message is then read as that type
    /// alone. Reading it as "any message" has serde hold every member in
    /// memory until it has seen the `type`, which for a response with a
    /// large `json` cost more than everything else done with it.
    pub fn from_json(json: &[u8]) -> Result<Message, serde_json::Error> {
        #[derive(Deserialize)]
        struct Type<'a> {
            #[serde(rename = "type", borrow)]
            name: std::borrow::Cow<'a, str>,
        }
        let read;
        let name = match type_in_front(json) {
            Some(name) => name,
            None => {
                read = serde_json::from_slice::<Type>(json)?;
                &*read.name
            }
        };
        Ok(match name {
            "ready" => Message::Ready(serde_json::from_slice(json)?),
            "request" => Message::Request(serde_json::from_slice(json)?),
            "response" => Message::Response(serde_json::from_slice(json)?),
            "continue" => Message::Continue(serde_json::from_slice(json)?),
            "chunk" => Message::Chunk(serde_json::from_slice(json)?),
            "end" => Message::End(serde_json::from_slice(json)?),
            "cancel" => Message::Cancel(serde_json::from_slice(json)?),
            "error" => Message::Error(serde_json::from_slice(json)?),
            "verify" => Message::Verify(serde_json::from_slice(json)?),
            "verified" => Message::Verified(serde_json::from_slice(json)?),
            "log" => Message::Log(serde_json::from_slice(json)?),
            "ack" => Message::Ack(serde_json::from_slice(json)?),
            "call" => Message::Call(serde_json::from_slice(json)?),
            "result" => Message::Result(serde_json::from_slice(json)?),
            "fetch" => Message::Fetch(serde_json::from_slice(json)?),
            "fetched" => Message::Fetched(serde_json::from_slice(json)?),
            _ => {
                serde_json::from_slice::<serde::de::IgnoredAny>(json)?;
                Message::Unknown
            }
        })
    }

    /// The request id this message refers to, if any.
    pub fn id(&self) -> Option<&str> {
        match self {
            Message::Request(m) => Some(&m.id),
            Message::Response(m) => Some(&m.id),
            Message::Continue(m) => Some(&m.id),
            Message::Chunk(m) => Some(&m.id),
            Message::End(m) => Some(&m.id),
            Message::Cancel(m) => Some(&m.id),
            Message::Error(m) => m.id.as_deref(),
            Message::Verify(m) => Some(&m.id),
            Message::Verified(m) => Some(&m.id),
            Message::Log(m) => m.id.as_deref(),
            Message::Ack(m) => Some(&m.id),
            Message::Call(m) => Some(&m.id),
            Message::Result(m) => Some(&m.id),
            Message::Fetch(m) => Some(&m.id),
            Message::Fetched(m) => Some(&m.id),
            Message::Ready(_) | Message::Unknown => None,
        }
    }

    /// Number of raw body bytes that follow this message.
    pub fn body_len(&self) -> Option<u64> {
        match self {
            Message::Request(m) => m.body_len,
            Message::Response(m) => m.body_len,
            Message::Chunk(m) => m.body_len,
            Message::Fetch(m) => m.body_len,
            Message::Fetched(m) => m.body_len,
            _ => None,
        }
    }

    fn set_body_len(&mut self, len: Option<u64>) -> Result<(), EncodeError> {
        let slot = match self {
            Message::Request(m) => &mut m.body_len,
            Message::Response(m) => &mut m.body_len,
            Message::Chunk(m) => &mut m.body_len,
            Message::Fetch(m) => &mut m.body_len,
            Message::Fetched(m) => &mut m.body_len,
            _ if len.is_none() => return Ok(()),
            _ => return Err(EncodeError::BodyNotAllowed),
        };
        *slot = len;
        Ok(())
    }
}

/// The `type` of a message that starts with it, as every SDK writes them:
/// `{"type":"response",` or `{"type": "response",`. Saves reading the
/// message once just to find it.
fn type_in_front(json: &[u8]) -> Option<&str> {
    fn spaces(mut text: &[u8]) -> &[u8] {
        while let [b' ', rest @ ..] = text {
            text = rest;
        }
        text
    }
    let rest = spaces(json.strip_prefix(b"{")?);
    let rest = spaces(rest.strip_prefix(b"\"type\"")?);
    let rest = spaces(rest.strip_prefix(b":")?);
    let rest = rest.strip_prefix(b"\"")?;
    let end = rest.iter().position(|b| *b == b'"')?;
    let name = std::str::from_utf8(&rest[..end]).ok()?;
    (!name.contains('\\')).then_some(name)
}

/// Sent by the worker when it is ready. Workers speak first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub verify: bool,
    /// The worker accepts streamed request bodies and acknowledges them.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream_body: bool,
    /// The worker reads a request's JSON body from the raw bytes after the
    /// message (`body_json`) instead of from its `json` member.
    #[serde(default, skip_serializing_if = "is_false")]
    pub raw_json: bool,
    /// The worker's handlers can call functions (see [`Call`]). Nothing
    /// in Unsworn depends on it; the conformance suite reads it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub calls: bool,
    /// The worker's handlers can ask for requests to outbound services
    /// (see [`Fetch`]). Read by the conformance suite.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fetch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<String>,
}

/// Which step of the request a handler runs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Handle,
    Before,
}

/// A request for a handler, sent by Unsworn.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(default)]
    pub phase: Phase,
    pub handler: String,
    pub route: String,
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub query: Map<String, Value>,
    /// The original query string, without the `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_query: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub context: Map<String, Value>,
    #[serde(default, skip_serializing_if = "json_apart")]
    pub json: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
    /// The raw body is the request's JSON body, for a worker whose `ready`
    /// has `raw_json`. Nothing has checked that it is JSON unless the route
    /// needed that.
    #[serde(default, skip_serializing_if = "is_false")]
    pub body_json: bool,
    /// The body follows as `chunk` messages, ending with `end`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// A request's JSON body, or the JSON a response answers with.
///
/// Unsworn checks a request body without keeping what it parsed, and sends
/// the text it received: building a value only to write it out again cost
/// several times what reading the body did. The same goes for a response's
/// `json`, which is kept as the text the handler sent. A decoded request
/// holds a [`Json::Value`], and a decoded response a [`Json::Raw`].
#[derive(Debug, Clone)]
pub enum Json {
    Value(Value),
    /// A JSON document as text, written into the message as it is. Make
    /// one with [`Json::raw`].
    Raw(RawJson),
}

/// The text of a JSON document. See [`Json::raw`] and [`Json::unchecked`].
#[derive(Debug, Clone)]
pub struct RawJson {
    text: Bytes,
    /// Something has read the text and found it to be JSON.
    checked: bool,
}

impl RawJson {
    pub fn text(&self) -> &Bytes {
        &self.text
    }

    /// Whether the text is known to be JSON. See [`Json::unchecked`].
    pub fn is_checked(&self) -> bool {
        self.checked
    }
}

thread_local! {
    /// Set while [`message_json`] encodes a request whose raw `json` it
    /// writes itself.
    static JSON_APART: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn json_apart(json: &Option<Json>) -> bool {
    match json {
        None => true,
        Some(Json::Raw(_)) => JSON_APART.get(),
        Some(Json::Value(_)) => false,
    }
}

/// Appends a message as JSON. A request's raw `json` is copied in as it
/// is: serializing it like any other member reads the whole document again
/// to make sure it's JSON, which [`Json::raw`] already promises. Text that
/// nothing has checked can't go in that way, so it's an error here: send
/// it as the raw body, or check it and use [`Json::raw`].
fn message_json(message: &Message, out: &mut Vec<u8>) -> Result<(), serde_json::Error> {
    let raw = match message {
        Message::Request(Request {
            json: Some(Json::Raw(raw)),
            ..
        }) => raw,
        _ => return serde_json::to_writer(out, message),
    };
    if !raw.checked {
        return Err(serde::ser::Error::custom(
            "a request's `json` hasn't been checked",
        ));
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            JSON_APART.set(false);
        }
    }
    JSON_APART.set(true);
    let reset = Reset;
    serde_json::to_writer(&mut *out, message)?;
    drop(reset);
    // A request is an object with other members, so this is its last brace.
    out.pop();
    out.extend_from_slice(b",\"json\":");
    out.extend_from_slice(&raw.text);
    out.push(b'}');
    Ok(())
}

impl Json {
    /// `text` must be one valid JSON document: it's written into the
    /// message unchecked.
    pub fn raw(text: Bytes) -> Json {
        Json::Raw(RawJson {
            text,
            checked: true,
        })
    }

    /// Text that is meant to be a JSON document, which nothing has read
    /// yet: a request body on its way to a worker that parses it itself,
    /// or the raw body a worker says is JSON.
    pub fn unchecked(text: Bytes) -> Json {
        Json::Raw(RawJson {
            text,
            checked: false,
        })
    }

    pub fn to_value(&self) -> Value {
        match self {
            Json::Value(v) => v.clone(),
            Json::Raw(raw) => serde_json::from_slice(&raw.text).unwrap_or(Value::Null),
        }
    }

    pub fn into_value(self) -> Value {
        match self {
            Json::Value(v) => v,
            raw => raw.to_value(),
        }
    }

    /// The document as JSON text.
    pub fn to_bytes(&self) -> Bytes {
        match self {
            Json::Value(v) => serde_json::to_vec(v).unwrap_or_default().into(),
            Json::Raw(raw) => raw.text.clone(),
        }
    }
}

/// Reads a `json` member as the text it was written in.
fn raw_json<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Json>, D::Error> {
    let raw = Option::<Box<serde_json::value::RawValue>>::deserialize(d)?;
    Ok(raw.map(|raw| Json::raw(String::from(Box::<str>::from(raw)).into())))
}

impl From<Value> for Json {
    fn from(value: Value) -> Json {
        Json::Value(value)
    }
}

impl PartialEq for Json {
    fn eq(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Value(a), Json::Value(b)) => a == b,
            _ => self.to_value() == other.to_value(),
        }
    }
}

impl Serialize for Json {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Value(v) => v.serialize(serializer),
            // Any serializer but this crate's encoders: checked, and
            // `null` if it turns out not to be JSON.
            Json::Raw(raw) => {
                match serde_json::from_slice::<&serde_json::value::RawValue>(&raw.text) {
                    Ok(raw) => raw.serialize(serializer),
                    Err(_) => serializer.serialize_unit(),
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        Value::deserialize(d).map(Json::Value)
    }
}

/// One or several values for a response header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HeaderValues {
    One(String),
    Many(Vec<String>),
}

impl HeaderValues {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        let slice: &[String] = match self {
            HeaderValues::One(v) => std::slice::from_ref(v),
            HeaderValues::Many(v) => v,
        };
        slice.iter().map(String::as_str)
    }
}

/// A handler's response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, HeaderValues>,
    /// Decoded as the text the handler sent, which goes to the client as
    /// it is.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_json"
    )]
    pub json: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
    /// The raw body is JSON the worker wrote, sent in place of `json`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub body_json: bool,
    /// The body follows as `chunk` messages, ending with `end`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    /// MCP content blocks, for MCP tool calls only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_content: Option<Value>,
}

impl Response {
    pub fn new(id: impl Into<String>, status: u16) -> Self {
        Response {
            id: id.into(),
            status,
            headers: BTreeMap::new(),
            json: None,
            text: None,
            body_len: None,
            body_json: false,
            stream: false,
            mcp_content: None,
        }
    }
}

/// A `before` handler lets the request continue, adding context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Continue {
    pub id: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub context: Map<String, Value>,
}

/// Part of a streamed response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
}

/// The end of a streamed response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct End {
    pub id: String,
}

/// Unsworn no longer needs the result of a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cancel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A handler failed (with `id`), or the worker has a problem (without).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub message: String,
}

/// Unsworn asks a worker whether it can resolve these handler references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verify {
    pub id: String,
    pub handlers: Vec<String>,
}

/// The worker's answer to [`Verify`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verified {
    pub id: String,
    #[serde(default)]
    pub missing: Vec<MissingHandler>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissingHandler {
    pub handler: String,
    pub reason: String,
}

/// The worker has read `bytes` more of a streamed request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub id: String,
    pub bytes: u64,
}

/// A handler calls a function: another handler, declared in the config as
/// `[[function]]`, in any runtime. Sent by a worker, answered with a
/// [`CallResult`] that has the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// Chosen by the worker, unique among its calls that are in flight.
    pub id: String,
    /// The request the call is made for. The function gets that request's
    /// context and what is left of its time.
    pub request: String,
    /// The function's name.
    pub function: String,
    /// The function's input.
    #[serde(default)]
    pub json: Value,
}

/// How a [`Call`] ended. Sent by Unsworn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallResult {
    pub id: String,
    /// The function's status, or the one for what went wrong: `404` for
    /// no such function, `422` for input its schema refuses, `504` for a
    /// timeout. A call succeeded when this is below 400.
    pub status: u16,
    /// What the function answered with.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub json: Value,
    /// What went wrong, when Unsworn or the function's worker failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A handler asks Unsworn to make an HTTP request to a service the config
/// declares under `[outbound.<name>]`. Unsworn adds the service's own
/// headers, such as its credential, which the handler never sees. Sent by
/// a worker, answered with a [`Fetched`] that has the same `id`. The
/// request's body is the raw bytes after the message.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Fetch {
    /// Chosen by the worker, unique among its fetches that are in flight.
    pub id: String,
    /// The request the fetch is made for.
    pub request: String,
    /// The service's name.
    pub service: String,
    #[serde(default = "get")]
    pub method: String,
    /// The path and query, after the service's URL. Starts with `/`.
    pub path: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
}

fn get() -> String {
    "GET".into()
}

/// A service's answer to a [`Fetch`], with its body as the raw bytes after
/// the message. Or, with `error`, why there isn't one.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Fetched {
    pub id: String,
    /// The service's status. With `error`, the one for what went wrong:
    /// `404` for no such service, `403` for a method or path the config
    /// doesn't allow, `502` for a service that couldn't be reached and
    /// `504` for one that took too long.
    pub status: u16,
    /// The response's headers, with lowercase names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Set when the request wasn't made or got no answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
}

/// A log record from a worker. With `id`, it belongs to that request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Log {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub level: LogLevel,
    pub message: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub fields: Map<String, Value>,
}

/// A log level. Parsing is lenient: case doesn't matter, common aliases
/// such as `warning` and `fatal` are accepted, and anything unknown is
/// `info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Parses a level name, or returns `None` if it isn't one.
    pub fn parse(s: &str) -> Option<LogLevel> {
        Some(match s.to_ascii_lowercase().as_str() {
            "error" | "err" | "fatal" | "critical" | "crit" | "panic" | "emerg" | "alert" => {
                LogLevel::Error
            }
            "warn" | "warning" => LogLevel::Warn,
            "info" | "information" | "notice" => LogLevel::Info,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            _ => return None,
        })
    }
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(LogLevel::parse(&s).unwrap_or_default())
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A message together with the raw body bytes that follow it.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub message: Message,
    pub body: Option<Bytes>,
}

impl Frame {
    pub fn new(message: Message) -> Self {
        Frame {
            message,
            body: None,
        }
    }

    /// A frame with a raw body. Sets the message's `body_len`.
    pub fn with_body(mut message: Message, body: Bytes) -> Result<Self, EncodeError> {
        message.set_body_len(Some(body.len() as u64))?;
        Ok(Frame {
            message,
            body: Some(body),
        })
    }
}

impl From<Message> for Frame {
    fn from(message: Message) -> Self {
        Frame::new(message)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("this message type can't carry a raw body")]
    BodyNotAllowed,
    #[error("body_len is {declared} but the body has {actual} bytes")]
    BodyLenMismatch { declared: u64, actual: u64 },
    #[error("message is too large ({0} bytes)")]
    TooLarge(usize),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// A line or frame that isn't a valid message. It has been consumed, so
    /// decoding can continue with the next one.
    #[error("invalid message: {error}: {snippet}")]
    Invalid { snippet: String, error: String },
    /// A message larger than the decoder's limit. The stream can't recover.
    #[error("message is too large ({0} bytes)")]
    TooLarge(usize),
}

fn check_body(frame: &Frame) -> Result<(), EncodeError> {
    let declared = frame.message.body_len().unwrap_or(0);
    let actual = frame.body.as_ref().map_or(0, |b| b.len() as u64);
    if declared != actual {
        return Err(EncodeError::BodyLenMismatch { declared, actual });
    }
    Ok(())
}

fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.chars().count() > 200 {
        let cut: String = text.chars().take(200).collect();
        format!("{cut}…")
    } else {
        text.to_string()
    }
}

/// Tier 1: JSON lines.
///
/// Each message is one line of JSON ending in `\n`. If it has `body_len: N`,
/// exactly `N` raw bytes follow the newline.
pub mod jsonl {
    use super::*;

    /// Appends the encoded frame to `out`.
    pub fn encode(frame: &Frame, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        encode_head(frame, out)?;
        if let Some(body) = &frame.body {
            out.extend_from_slice(body);
        }
        Ok(())
    }

    /// Appends everything but the raw body, for a sender that writes a
    /// large body after it instead of copying it in.
    pub fn encode_head(frame: &Frame, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        check_body(frame)?;
        let start = out.len();
        message_json(&frame.message, out)?;
        // A line feed ends the message. One can only have come from raw
        // JSON text, where it is whitespace between tokens, so a space says
        // the same.
        for byte in &mut out[start..] {
            if *byte == b'\n' {
                *byte = b' ';
            }
        }
        out.push(b'\n');
        Ok(())
    }

    pub fn encode_to_vec(frame: &Frame) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::new();
        encode(frame, &mut out)?;
        Ok(out)
    }

    /// Incremental decoder. Feed bytes into a [`BytesMut`] and call
    /// [`Decoder::decode`] until it returns `Ok(None)`.
    #[derive(Debug)]
    pub struct Decoder {
        max_frame: usize,
        /// A parsed message waiting for its raw body bytes.
        pending: Option<(Message, usize)>,
    }

    impl Default for Decoder {
        fn default() -> Self {
            Decoder::new(DEFAULT_MAX_FRAME)
        }
    }

    impl Decoder {
        pub fn new(max_frame: usize) -> Self {
            Decoder {
                max_frame,
                pending: None,
            }
        }

        pub fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, DecodeError> {
            loop {
                if let Some((_, len)) = &self.pending {
                    let len = *len;
                    if buf.len() < len {
                        return Ok(None);
                    }
                    let body = buf.split_to(len).freeze();
                    let (message, _) = self.pending.take().expect("pending message");
                    return Ok(Some(Frame {
                        message,
                        body: Some(body),
                    }));
                }

                let Some(newline) = buf.iter().position(|b| *b == b'\n') else {
                    if buf.len() > self.max_frame {
                        return Err(DecodeError::TooLarge(buf.len()));
                    }
                    return Ok(None);
                };
                let line = buf.split_to(newline + 1);
                let line = &line[..newline];
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let message = Message::from_json(line).map_err(|e| DecodeError::Invalid {
                    snippet: snippet(line),
                    error: e.to_string(),
                })?;
                match message.body_len() {
                    Some(len) if len > 0 => {
                        let len = len as usize;
                        if len > self.max_frame {
                            return Err(DecodeError::TooLarge(len));
                        }
                        self.pending = Some((message, len));
                    }
                    _ => {
                        return Ok(Some(Frame {
                            message,
                            body: None,
                        }));
                    }
                }
            }
        }
    }
}

/// Tier 2: length-prefixed frames.
///
/// ```text
/// ┌────────────────┬───────────────┬──────────────┬─────────────────┐
/// │ frame_len: u32 │ json_len: u32 │ JSON message │ raw body bytes  │
/// └────────────────┴───────────────┴──────────────┴─────────────────┘
/// ```
///
/// Both lengths are big-endian. `frame_len` counts everything after itself
/// (`4 + json_len + body length`). The message's `body_len` equals the
/// number of raw body bytes.
pub mod framed {
    use super::*;

    /// Appends the encoded frame to `out`.
    pub fn encode(frame: &Frame, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        encode_head(frame, out)?;
        if let Some(body) = &frame.body {
            out.extend_from_slice(body);
        }
        Ok(())
    }

    /// Appends everything but the raw body, for a sender that writes a
    /// large body after it instead of copying it in.
    pub fn encode_head(frame: &Frame, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        check_body(frame)?;
        // The lengths go in front, so they're filled in once the message
        // behind them has been written.
        let start = out.len();
        out.extend_from_slice(&[0; 8]);
        message_json(&frame.message, out)?;
        let json_len = out.len() - start - 8;
        let body_len = frame.body.as_ref().map_or(0, Bytes::len);
        let frame_len = 4 + json_len + body_len;
        let Ok(frame_len) = u32::try_from(frame_len) else {
            out.truncate(start);
            return Err(EncodeError::TooLarge(frame_len));
        };
        out[start..start + 4].copy_from_slice(&frame_len.to_be_bytes());
        out[start + 4..start + 8].copy_from_slice(&(json_len as u32).to_be_bytes());
        Ok(())
    }

    pub fn encode_to_vec(frame: &Frame) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::new();
        encode(frame, &mut out)?;
        Ok(out)
    }

    #[derive(Debug)]
    pub struct Decoder {
        max_frame: usize,
    }

    impl Default for Decoder {
        fn default() -> Self {
            Decoder::new(DEFAULT_MAX_FRAME)
        }
    }

    impl Decoder {
        pub fn new(max_frame: usize) -> Self {
            Decoder { max_frame }
        }

        pub fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, DecodeError> {
            if buf.len() < 4 {
                return Ok(None);
            }
            let frame_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            if frame_len > self.max_frame {
                return Err(DecodeError::TooLarge(frame_len));
            }
            if buf.len() < 4 + frame_len {
                return Ok(None);
            }
            buf.advance(4);
            let mut frame = buf.split_to(frame_len);
            if frame.len() < 4 {
                return Err(DecodeError::Invalid {
                    snippet: String::new(),
                    error: "frame shorter than its header".into(),
                });
            }
            let json_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
            frame.advance(4);
            if json_len > frame.len() {
                return Err(DecodeError::Invalid {
                    snippet: String::new(),
                    error: format!("json_len {json_len} exceeds frame size {}", frame.len()),
                });
            }
            let json = frame.split_to(json_len);
            let message = Message::from_json(&json).map_err(|e| DecodeError::Invalid {
                snippet: snippet(&json),
                error: e.to_string(),
            })?;
            let body = frame.freeze();
            let declared = message.body_len().unwrap_or(0) as usize;
            if declared != body.len() {
                return Err(DecodeError::Invalid {
                    snippet: snippet(&json),
                    error: format!(
                        "body_len is {declared} but the frame carries {} body bytes",
                        body.len()
                    ),
                });
            }
            Ok(Some(Frame {
                message,
                body: (!body.is_empty()).then_some(body),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> Message {
        let mut params = Map::new();
        params.insert("id".into(), json!(42));
        Message::Request(Request {
            id: "r-1".into(),
            handler: "src/users.ts#getUser".into(),
            route: "/users/{id}".into(),
            method: "GET".into(),
            path: "/users/42".into(),
            params,
            ..Default::default()
        })
    }

    fn response_with_body() -> Frame {
        let mut response = Response::new("r-1", 200);
        response.headers.insert(
            "set-cookie".into(),
            HeaderValues::Many(vec!["a=1".into(), "b=2".into()]),
        );
        Frame::with_body(
            Message::Response(response),
            Bytes::from_static(b"\x00binary\nbody"),
        )
        .unwrap()
    }

    #[test]
    fn message_json_shape() {
        let value = serde_json::to_value(request()).unwrap();
        assert_eq!(value["type"], "request");
        assert_eq!(value["phase"], "handle");
        assert_eq!(value["params"]["id"], 42);
        assert!(value.get("query").is_none(), "empty maps are omitted");

        let ready =
            Message::from_json(br#"{"type":"ready","protocol":1,"future_field":true}"#).unwrap();
        assert_eq!(
            ready,
            Message::Ready(Ready {
                protocol: 1,
                max_in_flight: None,
                verify: false,
                stream_body: false,
                calls: false,
                raw_json: false,
                fetch: false,
                sdk: None
            })
        );
    }

    #[test]
    fn raw_json_is_sent_as_it_is() {
        let text = "{\"b\": [1,\n 2.50, 1e2],\n\"a\":\"x\\ny\"}";
        let message = Message::Request(Request {
            id: "r-1".into(),
            json: Some(Json::raw(Bytes::from(text))),
            ..Default::default()
        });
        for framed in [false, true] {
            let frame = Frame::new(message.clone());
            let mut bytes = Vec::new();
            if framed {
                framed::encode(&frame, &mut bytes).unwrap();
            } else {
                jsonl::encode(&frame, &mut bytes).unwrap();
                assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
            }
            let sent = String::from_utf8_lossy(&bytes).into_owned();
            // Member order and number spelling are the sender's. Only a
            // line of JSON has to do without the line feeds.
            let want = if framed {
                format!("\"json\":{text}")
            } else {
                r#""json":{"b": [1,  2.50, 1e2], "a":"x\ny"}"#.to_string()
            };
            assert!(sent.contains(&want), "{sent}");
            let mut buf = BytesMut::from(&bytes[..]);
            let decoded = if framed {
                framed::Decoder::default().decode(&mut buf)
            } else {
                jsonl::Decoder::default().decode(&mut buf)
            };
            let Message::Request(request) = decoded.unwrap().unwrap().message else {
                panic!("expected a request")
            };
            assert_eq!(
                request.json,
                Some(Json::Value(json!({"b": [1, 2.5, 100.0], "a": "x\ny"})))
            );
        }
        // Serialized as a value, text that isn't JSON becomes `null`.
        let broken = Request {
            json: Some(Json::raw(Bytes::from_static(b"{\"a\":"))),
            ..Default::default()
        };
        assert_eq!(serde_json::to_value(&broken).unwrap()["json"], Value::Null);
        // And a request serialized any other way still carries its body.
        let Message::Request(request) = message else {
            unreachable!()
        };
        let plain = serde_json::to_string(&request).unwrap();
        assert!(plain.contains(&format!("\"json\":{text}")), "{plain}");
        // Text nothing has checked can't be written into a message.
        let unchecked = Message::Request(Request {
            id: "r-2".into(),
            json: Some(Json::unchecked(Bytes::from_static(b"{\"a\":"))),
            ..Default::default()
        });
        assert!(framed::encode(&Frame::new(unchecked.clone()), &mut Vec::new()).is_err());
        assert!(jsonl::encode(&Frame::new(unchecked), &mut Vec::new()).is_err());
    }

    #[test]
    fn unknown_message_types_are_tolerated() {
        let msg = Message::from_json(br#"{"type":"telemetry","level":"info"}"#).unwrap();
        assert_eq!(msg, Message::Unknown);
        assert!(Message::from_json(br#"{"type":"telemetry","level":}"#).is_err());
        assert!(Message::from_json(br#"{"level":"info"}"#).is_err());
    }

    #[test]
    fn the_type_is_found_wherever_it_is() {
        for text in [
            r#"{"type":"response","id":"r-1","status":200}"#,
            r#"{"id":"r-1","status":200,"type":"response"}"#,
            r#" { "type" : "response", "id": "r-1", "status": 200 }"#,
            r#"{"type": "response", "id": "r-1", "status": 200}"#,
            "{\n\t\"type\": \"response\", \"id\": \"r-1\", \"status\": 200}",
            r#"{"type":"\u0072esponse","id":"r-1","status":200}"#,
        ] {
            let msg = Message::from_json(text.as_bytes()).unwrap();
            assert_eq!(msg, Message::Response(Response::new("r-1", 200)), "{text}");
        }
    }

    #[test]
    fn a_response_keeps_its_json_as_sent() {
        let text =
            r#"{"type":"response","id":"r-1","status":200,"json":{"b": 1.0,  "a":[1e2,"\u00e9"]}}"#;
        let Message::Response(response) = Message::from_json(text.as_bytes()).unwrap() else {
            panic!("expected a response")
        };
        let json = response.json.as_ref().expect("json");
        assert_eq!(&json.to_bytes()[..], br#"{"b": 1.0,  "a":[1e2,"\u00e9"]}"#);
        assert_eq!(json.to_value(), json!({"b": 1.0, "a": [100.0, "é"]}));
        // Written out again, it's the same text.
        let again = serde_json::to_string(&Message::Response(response)).unwrap();
        assert!(
            again.ends_with(r#""json":{"b": 1.0,  "a":[1e2,"\u00e9"]}}"#),
            "{again}"
        );

        // `null` is no JSON at all, and bad JSON is refused.
        let none =
            Message::from_json(br#"{"type":"response","id":"r-1","status":200,"json":null}"#);
        assert_eq!(none.unwrap(), Message::Response(Response::new("r-1", 200)));
        assert!(
            Message::from_json(br#"{"type":"response","id":"r-1","status":200,"json":{"a":}}"#)
                .is_err()
        );

        // A response held as a value, as the edge build has it.
        let value = json!({"id": "r-1", "status": 200, "json": {"a": [1, 2]}});
        let response: Response = serde_json::from_value(value).unwrap();
        assert_eq!(response.json.unwrap().to_value(), json!({"a": [1, 2]}));
    }

    #[test]
    fn jsonl_roundtrip_with_partial_input() {
        let mut bytes = Vec::new();
        jsonl::encode(&request().into(), &mut bytes).unwrap();
        jsonl::encode(&response_with_body(), &mut bytes).unwrap();
        jsonl::encode(&Message::End(End { id: "r-1".into() }).into(), &mut bytes).unwrap();

        // Feed one byte at a time to exercise every partial state.
        let mut decoder = jsonl::Decoder::default();
        let mut buf = BytesMut::new();
        let mut frames = Vec::new();
        for b in bytes {
            buf.extend_from_slice(&[b]);
            while let Some(frame) = decoder.decode(&mut buf).unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].message, request());
        assert_eq!(frames[1], response_with_body());
        assert!(buf.is_empty());
    }

    #[test]
    fn jsonl_invalid_line_is_skipped() {
        let mut buf = BytesMut::from(&b"debug: hello\n\n{\"type\":\"end\",\"id\":\"x\"}\n"[..]);
        let mut decoder = jsonl::Decoder::default();
        match decoder.decode(&mut buf) {
            Err(DecodeError::Invalid { snippet, .. }) => assert_eq!(snippet, "debug: hello"),
            other => panic!("expected invalid line, got {other:?}"),
        }
        let frame = decoder.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.message, Message::End(End { id: "x".into() }));
    }

    #[test]
    fn jsonl_line_too_large() {
        let mut decoder = jsonl::Decoder::new(16);
        let mut buf = BytesMut::from(&b"{\"type\":\"end\",\"id\":\"0123456789\"}"[..]);
        assert!(matches!(
            decoder.decode(&mut buf),
            Err(DecodeError::TooLarge(_))
        ));
    }

    #[test]
    fn framed_roundtrip() {
        let mut bytes = Vec::new();
        framed::encode(&response_with_body(), &mut bytes).unwrap();
        framed::encode(&request().into(), &mut bytes).unwrap();

        let mut decoder = framed::Decoder::default();
        let mut buf = BytesMut::new();
        let mut frames = Vec::new();
        for chunk in bytes.chunks(3) {
            buf.extend_from_slice(chunk);
            while let Some(frame) = decoder.decode(&mut buf).unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames, vec![response_with_body(), request().into()]);
    }

    #[test]
    fn body_len_must_match() {
        let mut response = Response::new("r", 200);
        response.body_len = Some(10);
        let frame = Frame::new(Message::Response(response));
        assert!(matches!(
            jsonl::encode_to_vec(&frame),
            Err(EncodeError::BodyLenMismatch { .. })
        ));
        assert!(
            Frame::with_body(
                Message::End(End { id: "r".into() }),
                Bytes::from_static(b"x")
            )
            .is_err()
        );
    }

    #[test]
    fn log_levels_parse_leniently() {
        let log =
            Message::from_json(br#"{"type":"log","level":"WARNING","message":"hi","id":"r-1"}"#)
                .unwrap();
        let Message::Log(log) = log else {
            panic!("expected a log message")
        };
        assert_eq!(log.level, LogLevel::Warn);
        assert_eq!(log.id.as_deref(), Some("r-1"));
        let log = Message::from_json(br#"{"type":"log","level":"loud","message":"hi"}"#).unwrap();
        assert!(matches!(
            log,
            Message::Log(Log {
                level: LogLevel::Info,
                ..
            })
        ));
        assert_eq!(LogLevel::parse("fatal"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("verbose"), None);
    }
}
