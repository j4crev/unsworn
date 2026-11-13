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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// A message type this version doesn't know. Receivers ignore it.
    #[serde(other)]
    Unknown,
}

impl Message {
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
            Message::Ready(_) | Message::Unknown => None,
        }
    }

    /// Number of raw body bytes that follow this message.
    pub fn body_len(&self) -> Option<u64> {
        match self {
            Message::Request(m) => m.body_len,
            Message::Response(m) => m.body_len,
            Message::Chunk(m) => m.body_len,
            _ => None,
        }
    }

    fn set_body_len(&mut self, len: Option<u64>) -> Result<(), EncodeError> {
        let slot = match self {
            Message::Request(m) => &mut m.body_len,
            Message::Response(m) => &mut m.body_len,
            Message::Chunk(m) => &mut m.body_len,
            _ if len.is_none() => return Ok(()),
            _ => return Err(EncodeError::BodyNotAllowed),
        };
        *slot = len;
        Ok(())
    }
}

/// Sent by the worker when it is ready. Workers speak first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub verify: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
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
        check_body(frame)?;
        serde_json::to_writer(&mut *out, &frame.message)?;
        out.push(b'\n');
        if let Some(body) = &frame.body {
            out.extend_from_slice(body);
        }
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
                let message: Message =
                    serde_json::from_slice(line).map_err(|e| DecodeError::Invalid {
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
        check_body(frame)?;
        let json = serde_json::to_vec(&frame.message)?;
        let body_len = frame.body.as_ref().map_or(0, Bytes::len);
        let frame_len = 4 + json.len() + body_len;
        let frame_len = u32::try_from(frame_len).map_err(|_| EncodeError::TooLarge(frame_len))?;
        out.extend_from_slice(&frame_len.to_be_bytes());
        out.extend_from_slice(&(json.len() as u32).to_be_bytes());
        out.extend_from_slice(&json);
        if let Some(body) = &frame.body {
            out.extend_from_slice(body);
        }
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
            let message: Message =
                serde_json::from_slice(&json).map_err(|e| DecodeError::Invalid {
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

        let ready: Message =
            serde_json::from_str(r#"{"type":"ready","protocol":1,"future_field":true}"#).unwrap();
        assert_eq!(
            ready,
            Message::Ready(Ready {
                protocol: 1,
                max_in_flight: None,
                verify: false,
                sdk: None
            })
        );
    }

    #[test]
    fn unknown_message_types_are_tolerated() {
        let msg: Message = serde_json::from_str(r#"{"type":"log","level":"info"}"#).unwrap();
        assert_eq!(msg, Message::Unknown);
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
}
