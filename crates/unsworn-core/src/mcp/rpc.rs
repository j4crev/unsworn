//! Reading MCP JSON-RPC requests and building replies.
//!
//! Hosts pass in the body bytes and the request metadata headers; this
//! module checks them against each other and against MCP 2026-07-28, and
//! never touches the network.

use serde_json::{Map, Value, json};

use super::{
    McpEndpoint, McpTool, PROTOCOL_VERSION, ParamHeader, ParamKind, SUPPORTED_VERSIONS, ServerInfo,
};
use crate::config::ListTools;
use crate::validate::FieldError;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const HEADER_MISMATCH: i64 = -32020;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT: &str = "io.modelcontextprotocol/clientInfo";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";

/// A JSON-RPC error, with the HTTP status it's sent with.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub status: u16,
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            status: 400,
            code,
            message: message.into(),
            data: None,
        }
    }

    /// A method this server doesn't implement: `404` with `-32601`.
    pub fn method_not_found(method: &str) -> Self {
        RpcError {
            status: 404,
            ..RpcError::new(METHOD_NOT_FOUND, format!("Method not found: {method}"))
        }
    }

    /// Bad `params`, such as an unknown tool name.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        RpcError::new(
            INVALID_PARAMS,
            format!("Invalid params: {}", message.into()),
        )
    }

    fn mismatch(message: impl Into<String>) -> Self {
        RpcError::new(HEADER_MISMATCH, message)
    }

    fn unsupported(requested: Option<&str>) -> Self {
        RpcError {
            data: Some(json!({ "supported": SUPPORTED_VERSIONS, "requested": requested })),
            ..RpcError::new(
                UNSUPPORTED_PROTOCOL_VERSION,
                format!("Unsupported protocol version: this server speaks MCP {PROTOCOL_VERSION}"),
            )
        }
    }

    /// The JSON-RPC error response.
    pub fn to_json(&self, id: Option<&Value>) -> Value {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(d) = &self.data {
            error["data"] = d.clone();
        }
        let mut out = json!({ "jsonrpc": "2.0", "error": error });
        if let Some(id) = id {
            out["id"] = id.clone();
        }
        out
    }
}

/// A request that couldn't be accepted, with its id when it had one.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejected {
    pub id: Option<Value>,
    pub error: RpcError,
}

/// The MCP request metadata headers, as the client sent them.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    /// `MCP-Protocol-Version`.
    pub protocol_version: Option<String>,
    /// `Mcp-Method`.
    pub method: Option<String>,
    /// `Mcp-Name`, still encoded if the client Base64-encoded it.
    pub name: Option<String>,
    /// Every `Mcp-Param-*` header, by lowercase name.
    pub params: Vec<(String, String)>,
    /// MCP headers whose values aren't valid header text.
    pub invalid: Vec<String>,
}

impl Headers {
    /// Reads the MCP headers from a request.
    pub fn from_http(headers: &http::HeaderMap) -> Headers {
        let mut out = Headers::default();
        let mut text = |name: &str, label: &str| match headers.get(name).map(|v| v.to_str()) {
            None => None,
            Some(Ok(v)) => Some(v.to_string()),
            Some(Err(_)) => {
                out.invalid.push(label.to_string());
                None
            }
        };
        let protocol_version = text("mcp-protocol-version", "MCP-Protocol-Version");
        let method = text("mcp-method", "Mcp-Method");
        let name = text("mcp-name", "Mcp-Name");
        out.protocol_version = protocol_version;
        out.method = method;
        out.name = name;
        for (k, v) in headers {
            if k.as_str().starts_with("mcp-param-") {
                match v.to_str() {
                    Ok(v) => out.params.push((k.as_str().to_string(), v.to_string())),
                    Err(_) => out.invalid.push(k.as_str().to_string()),
                }
            }
        }
        out
    }

    fn param(&self, header: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == header)
            .map(|(_, v)| v.as_str())
    }
}

/// The headers policies may use as keys: `Mcp-Param-*` headers only when
/// they were checked against this call's arguments.
pub fn trusted_headers(headers: &http::HeaderMap, tool: Option<&McpTool>) -> http::HeaderMap {
    let mut out = headers.clone();
    let unchecked: Vec<_> = headers
        .keys()
        .filter(|k| {
            k.as_str().starts_with("mcp-param-")
                && !tool.is_some_and(|t| t.param_headers.iter().any(|p| p.header == k.as_str()))
        })
        .cloned()
        .collect();
    for k in unchecked {
        out.remove(k);
    }
    out
}

/// What a `before` handler's rejection says, as text for the model.
/// `body` is the start of the rejection's body.
pub fn rejection_text(status: http::StatusCode, body: &[u8]) -> String {
    let reason = match serde_json::from_slice::<Value>(body) {
        Ok(v) => ["detail", "message", "error"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| v.to_string()),
        Err(_) => String::from_utf8_lossy(body).trim().to_string(),
    };
    let label = format!(
        "{} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    if reason.is_empty() {
        format!("The call was rejected ({}).", label.trim())
    } else {
        format!("The call was rejected ({}): {reason}", label.trim())
    }
}

/// One JSON-RPC request or notification from an MCP client.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// `None` for a notification.
    pub id: Option<Value>,
    pub method: String,
    pub params: Map<String, Value>,
    /// Set on requests; notifications don't carry one.
    pub protocol_version: Option<String>,
    /// What the client says about itself, if it says anything.
    pub client: Option<Value>,
}

impl Message {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// For `tools/call`: the tool name and its arguments (`{}` when absent).
    pub fn tool_call(&self) -> Option<(&str, Value)> {
        if self.method != "tools/call" {
            return None;
        }
        let name = self.params.get("name")?.as_str()?;
        let args = self
            .params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        Some((name, args))
    }
}

/// Reads one message and checks it against its headers: the protocol
/// version, `Mcp-Method` and `Mcp-Name`. `Mcp-Param-*` headers depend on the
/// tool, so [`check_param_headers`] checks them.
pub fn read(body: &[u8], headers: &Headers) -> Result<Message, Rejected> {
    let reject = |id: Option<&Value>, error: RpcError| Rejected {
        id: id.cloned(),
        error,
    };
    // Parsers disagree about which of two members with one name counts.
    // A proxied server gets these bytes as they are, so it could run a
    // different tool, with different arguments, than the one checked here.
    let value = parse_json(body).map_err(|e| match e {
        JsonError::Invalid(e) => reject(
            None,
            RpcError::new(PARSE_ERROR, format!("Parse error: {e}")),
        ),
        JsonError::Duplicate(key) => reject(
            None,
            RpcError::new(
                INVALID_REQUEST,
                format!(
                    "Invalid request: the member \"{key}\" appears more than once in an object"
                ),
            ),
        ),
    })?;
    let Some(obj) = value.as_object() else {
        let what = if value.is_array() {
            "batches aren't supported; send one message per request"
        } else {
            "a message must be a JSON object"
        };
        return Err(reject(
            None,
            RpcError::new(INVALID_REQUEST, format!("Invalid request: {what}")),
        ));
    };
    let id = match obj.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_))) => Some(v),
        Some(_) => {
            return Err(reject(
                None,
                RpcError::new(
                    INVALID_REQUEST,
                    "Invalid request: `id` must be a string or a number",
                ),
            ));
        }
    };
    let invalid = |message: &str| {
        reject(
            id,
            RpcError::new(INVALID_REQUEST, format!("Invalid request: {message}")),
        )
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid("`jsonrpc` must be \"2.0\""));
    }
    if obj.contains_key("result") || obj.contains_key("error") {
        return Err(invalid("clients can't send responses"));
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Err(invalid("`method` must be a string"));
    };
    if id.is_none() && !method.starts_with("notifications/") {
        return Err(invalid("a request needs an `id`"));
    }
    let params = match obj.get("params") {
        None => Map::new(),
        Some(Value::Object(p)) => p.clone(),
        Some(_) => {
            return Err(reject(
                id,
                RpcError::new(INVALID_PARAMS, "Invalid params: `params` must be an object"),
            ));
        }
    };
    if let Some(name) = headers.invalid.first() {
        return Err(reject(
            id,
            RpcError::mismatch(format!(
                "Header mismatch: {name} has characters that aren't allowed"
            )),
        ));
    }

    let meta = params.get("_meta").and_then(Value::as_object);
    let meta_version = meta
        .and_then(|m| m.get(META_VERSION))
        .and_then(Value::as_str);
    let mut protocol_version = None;
    if id.is_some() {
        // Clients for MCP 2025-11-25 and earlier open with `initialize`
        // and no per-request version; tell them which versions work.
        if method == "initialize" || (headers.protocol_version.is_none() && meta_version.is_none())
        {
            return Err(reject(
                id,
                RpcError::unsupported(headers.protocol_version.as_deref().or(meta_version)),
            ));
        }
        let Some(header_version) = headers.protocol_version.as_deref() else {
            return Err(reject(
                id,
                RpcError::mismatch("Header mismatch: the MCP-Protocol-Version header is missing"),
            ));
        };
        let Some(body_version) = meta_version else {
            return Err(reject(
                id,
                RpcError::new(
                    INVALID_PARAMS,
                    format!("Invalid params: `_meta` must include \"{META_VERSION}\""),
                ),
            ));
        };
        if header_version != body_version {
            return Err(reject(
                id,
                RpcError::mismatch(format!(
                    "Header mismatch: MCP-Protocol-Version header value '{header_version}' does not match body value '{body_version}'"
                )),
            ));
        }
        if !SUPPORTED_VERSIONS.contains(&body_version) {
            return Err(reject(id, RpcError::unsupported(Some(body_version))));
        }
        if !meta.is_some_and(|m| m.get(META_CAPABILITIES).is_some_and(Value::is_object)) {
            return Err(reject(
                id,
                RpcError::new(
                    INVALID_PARAMS,
                    format!("Invalid params: `_meta` must include \"{META_CAPABILITIES}\""),
                ),
            ));
        }
        protocol_version = Some(body_version.to_string());
    }

    match headers.method.as_deref() {
        None => {
            return Err(reject(
                id,
                RpcError::mismatch("Header mismatch: the Mcp-Method header is missing"),
            ));
        }
        Some(m) if m != method => {
            return Err(reject(
                id,
                RpcError::mismatch(format!(
                    "Header mismatch: Mcp-Method header value '{m}' does not match body value '{method}'"
                )),
            ));
        }
        Some(_) => {}
    }
    let name_field = match method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    };
    if let Some(field) = name_field {
        let Some(body_name) = params.get(field).and_then(Value::as_str) else {
            return Err(reject(
                id,
                RpcError::new(
                    INVALID_PARAMS,
                    format!("Invalid params: `{field}` must be a string"),
                ),
            ));
        };
        let Some(raw) = headers.name.as_deref() else {
            return Err(reject(
                id,
                RpcError::mismatch("Header mismatch: the Mcp-Name header is missing"),
            ));
        };
        let Some(header_name) = decode_header_value(raw) else {
            return Err(reject(
                id,
                RpcError::mismatch("Header mismatch: the Mcp-Name header isn't valid Base64"),
            ));
        };
        if header_name != body_name {
            return Err(reject(
                id,
                RpcError::mismatch(format!(
                    "Header mismatch: Mcp-Name header value '{header_name}' does not match body value '{body_name}'"
                )),
            ));
        }
    }

    Ok(Message {
        id: id.cloned(),
        method: method.to_string(),
        client: meta.and_then(|m| m.get(META_CLIENT)).cloned(),
        params,
        protocol_version,
    })
}

/// Why a document isn't acceptable JSON.
#[derive(Debug)]
pub enum JsonError {
    /// It isn't valid JSON.
    Invalid(serde_json::Error),
    /// An object in it has two members with this name.
    Duplicate(String),
}

/// Parses a JSON document, refusing one in which an object has two members
/// with the same name. One pass does both: a body used to be parsed once
/// for its value and again to look for repeats, which doubled the cost of
/// every JSON request.
pub fn parse_json(json: &[u8]) -> Result<Value, JsonError> {
    use serde::de::{Deserializer as _, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};
    use std::cell::RefCell;

    /// Builds a value, noting the first repeated member name in `found`.
    struct Build<'a>(&'a RefCell<Option<String>>);

    impl<'de> serde::de::DeserializeSeed<'de> for Build<'_> {
        type Value = Value;
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
            d.deserialize_any(self)
        }
    }

    impl<'de> Visitor<'de> for Build<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON value")
        }
        fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
            Ok(Value::Bool(v))
        }
        fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
            Ok(Value::Number(v.into()))
        }
        fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
            Ok(Value::Number(v.into()))
        }
        fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
            Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
        }
        fn visit_str<E>(self, v: &str) -> Result<Value, E> {
            Ok(Value::String(v.to_owned()))
        }
        fn visit_string<E>(self, v: String) -> Result<Value, E> {
            Ok(Value::String(v))
        }
        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element_seed(Build(self.0))? {
                items.push(item);
            }
            Ok(Value::Array(items))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut members = Map::new();
            while let Some(key) = map.next_key::<String>()? {
                if members.contains_key(&key) {
                    *self.0.borrow_mut() = Some(key);
                    return Err(A::Error::custom("duplicate member"));
                }
                let value = map.next_value_seed(Build(self.0))?;
                members.insert(key, value);
            }
            Ok(Value::Object(members))
        }
    }

    let found = RefCell::new(None);
    let mut de = serde_json::Deserializer::from_slice(json);
    let parsed = de.deserialize_any(Build(&found)).and_then(|v| {
        de.end()?;
        Ok(v)
    });
    match (parsed, found.into_inner()) {
        (Ok(value), _) => Ok(value),
        // A repeat in a document that's also malformed further on is
        // reported as malformed, as it always was.
        (Err(_), Some(key)) => match serde_json::from_slice::<IgnoredAny>(json) {
            Ok(_) => Err(JsonError::Duplicate(key)),
            Err(e) => Err(JsonError::Invalid(e)),
        },
        (Err(e), None) => Err(JsonError::Invalid(e)),
    }
}

/// Checks what [`parse_json`] checks, without building the value: for a
/// body that's passed on as the text it arrived as. About a quarter of the
/// cost of parsing it.
pub fn check_json(json: &[u8]) -> Result<(), JsonError> {
    use serde::de::{Deserializer as _, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::collections::HashSet;

    /// An object's member name, borrowed from the document unless it has
    /// escapes.
    struct Name;

    impl<'de> serde::de::DeserializeSeed<'de> for Name {
        type Value = Cow<'de, str>;
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Cow<'de, str>, D::Error> {
            d.deserialize_str(self)
        }
    }

    impl<'de> Visitor<'de> for Name {
        type Value = Cow<'de, str>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a member name")
        }
        fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Cow<'de, str>, E> {
            Ok(Cow::Borrowed(v))
        }
        fn visit_str<E>(self, v: &str) -> Result<Cow<'de, str>, E> {
            Ok(Cow::Owned(v.to_owned()))
        }
    }

    /// Objects with more members than this look names up in a set.
    const SCAN: usize = 16;

    #[derive(Default)]
    struct Seen<'de> {
        /// The member names of every object being read, innermost last,
        /// in one list so an object doesn't cost an allocation.
        names: Vec<Cow<'de, str>>,
        repeated: Option<String>,
    }

    /// Reads a value and drops it, noting the first repeated member name.
    struct Check<'a, 'de>(&'a RefCell<Seen<'de>>);

    impl<'de> serde::de::DeserializeSeed<'de> for Check<'_, 'de> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(self)
        }
    }

    impl<'de> Visitor<'de> for Check<'_, 'de> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON value")
        }
        fn visit_bool<E>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element_seed(Check(self.0))?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            // This object's names are `names[start..]`, until there are
            // enough of them to be worth a set.
            let start = self.0.borrow().names.len();
            let mut set: HashSet<Cow<'de, str>> = HashSet::new();
            while let Some(name) = map.next_key_seed(Name)? {
                let mut seen = self.0.borrow_mut();
                let repeated = if set.is_empty() {
                    seen.names[start..].contains(&name)
                } else {
                    set.contains(&name)
                };
                if repeated {
                    seen.repeated = Some(name.into_owned());
                    return Err(A::Error::custom("duplicate member"));
                }
                if !set.is_empty() {
                    set.insert(name);
                } else if seen.names.len() - start < SCAN {
                    seen.names.push(name);
                } else {
                    set.extend(seen.names.drain(start..));
                    set.insert(name);
                }
                drop(seen);
                map.next_value_seed(Check(self.0))?;
            }
            self.0.borrow_mut().names.truncate(start);
            Ok(())
        }
    }

    let seen = RefCell::new(Seen::default());
    let mut de = serde_json::Deserializer::from_slice(json);
    let checked = de.deserialize_any(Check(&seen)).and_then(|()| de.end());
    let found = seen.into_inner().repeated;
    match (checked, found) {
        (Ok(()), _) => Ok(()),
        (Err(_), Some(key)) => match serde_json::from_slice::<IgnoredAny>(json) {
            Ok(_) => Err(JsonError::Duplicate(key)),
            Err(e) => Err(JsonError::Invalid(e)),
        },
        (Err(e), None) => Err(JsonError::Invalid(e)),
    }
}

/// The first member name that appears twice in one object, anywhere in a
/// JSON document. `None` for a document without one, or one that isn't
/// JSON.
pub fn duplicate_key(json: &[u8]) -> Option<String> {
    match parse_json(json) {
        Err(JsonError::Duplicate(key)) => Some(key),
        _ => None,
    }
}

/// Checks the `Mcp-Param-*` headers a tool declares against its arguments.
/// A header must be present exactly when its argument is (and isn't
/// `null`), with the same value.
pub fn check_param_headers(
    tool: &McpTool,
    args: &Value,
    headers: &Headers,
) -> Result<(), RpcError> {
    for p in &tool.param_headers {
        let value = p
            .path
            .iter()
            .try_fold(args, |v, key| v.get(key))
            .filter(|v| !v.is_null());
        let header = headers.param(&p.header);
        match (value, header) {
            (None, None) => {}
            (Some(_), None) => {
                return Err(RpcError::mismatch(format!(
                    "Header mismatch: the Mcp-Param-{} header is missing",
                    p.name
                )));
            }
            (None, Some(_)) => {
                return Err(RpcError::mismatch(format!(
                    "Header mismatch: the Mcp-Param-{} header is set, but `{}` isn't in the arguments",
                    p.name,
                    p.path.join(".")
                )));
            }
            (Some(v), Some(raw)) => {
                let Some(decoded) = decode_header_value(raw) else {
                    return Err(RpcError::mismatch(format!(
                        "Header mismatch: the Mcp-Param-{} header isn't valid Base64",
                        p.name
                    )));
                };
                if !param_matches(p, v, &decoded) {
                    return Err(RpcError::mismatch(format!(
                        "Header mismatch: Mcp-Param-{} header value '{decoded}' does not match body value {v}",
                        p.name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn param_matches(p: &ParamHeader, value: &Value, header: &str) -> bool {
    match (p.kind, value) {
        (ParamKind::String, Value::String(s)) => s == header,
        (ParamKind::Integer, Value::Number(n)) => {
            matches!((n.as_f64(), header.trim().parse::<f64>()), (Some(a), Ok(b)) if a == b)
        }
        (ParamKind::Boolean, Value::Bool(b)) => header == if *b { "true" } else { "false" },
        // A value of the wrong type fails input validation anyway; only a
        // string equal to the header passes here.
        (_, other) => other == header,
    }
}

/// Decodes the `=?base64?...?=` form MCP uses for header values that
/// aren't plain ASCII. Other values are returned as they are.
pub fn decode_header_value(raw: &str) -> Option<String> {
    match raw
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(encoded) => String::from_utf8(base64_decode(encoded)?).ok(),
        None => Some(raw.to_string()),
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// A successful JSON-RPC response. Adds `resultType` and, when given, the
/// server's identity in `_meta`.
pub fn result(id: &Value, mut result: Map<String, Value>, server: Option<&ServerInfo>) -> Value {
    result.insert("resultType".into(), json!("complete"));
    if let Some(s) = server {
        let meta = result.entry("_meta").or_insert_with(|| json!({}));
        if let Some(m) = meta.as_object_mut() {
            m.insert(META_SERVER.into(), s.to_json());
        }
    }
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A tool result that reports an error to the model, so it can react.
pub fn tool_error(text: impl Into<String>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert(
        "content".into(),
        json!([{ "type": "text", "text": text.into() }]),
    );
    out.insert("isError".into(), json!(true));
    out
}

/// The `server/discover` result.
pub fn discover(mcp: &McpEndpoint) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("supportedVersions".into(), json!(SUPPORTED_VERSIONS));
    // No `listChanged`: change notifications aren't supported yet.
    out.insert("capabilities".into(), json!({ "tools": {} }));
    if let Some(i) = &mcp.instructions {
        out.insert("instructions".into(), json!(i));
    }
    out.insert("ttlMs".into(), json!(mcp.list_ttl.as_millis() as u64));
    out.insert("cacheScope".into(), json!("public"));
    out
}

/// Whether `tools/list` depends on the caller: only when it's filtered and
/// some tool needs scopes of its own.
pub fn list_varies_by_caller(mcp: &McpEndpoint) -> bool {
    mcp.list_tools == ListTools::Permitted && mcp.tools.iter().any(|t| t.own.auth.is_some())
}

/// The `tools/list` result, with the tools `visible` lets through, in name
/// order.
pub fn list_tools(mcp: &McpEndpoint, visible: impl Fn(&McpTool) -> bool) -> Map<String, Value> {
    let tools: Vec<Value> = mcp
        .tools
        .iter()
        .filter(|t| visible(t))
        .map(McpTool::definition)
        .collect();
    let scope = if list_varies_by_caller(mcp) {
        "private"
    } else {
        "public"
    };
    let mut out = Map::new();
    out.insert("tools".into(), Value::Array(tools));
    out.insert("ttlMs".into(), json!(mcp.list_ttl.as_millis() as u64));
    out.insert("cacheScope".into(), json!(scope));
    out
}

/// What a tool's handler replied, for [`call_result`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Reply<'a> {
    pub status: u16,
    pub json: Option<&'a Value>,
    pub text: Option<&'a str>,
    pub mcp_content: Option<&'a Value>,
    /// The handler sent raw bytes or a stream, which MCP results can't carry.
    pub other_body: bool,
}

/// Turns a handler's reply into a tool result.
///
/// - `json` becomes `structuredContent`, plus a text copy in `content`, as
///   MCP recommends.
/// - `text` becomes one text block.
/// - `mcp_content` is used as `content` as it is.
/// - A status outside 2xx becomes a tool error with the handler's message.
pub fn call_result(reply: Reply<'_>) -> Map<String, Value> {
    if !(200..300).contains(&reply.status) {
        let message = reply
            .json
            .and_then(|j| {
                ["error", "message", "detail"]
                    .iter()
                    .find_map(|k| j.get(*k).and_then(Value::as_str))
            })
            .map(String::from)
            .or_else(|| reply.json.map(Value::to_string))
            .or_else(|| reply.text.map(|t| t.trim().to_string()))
            .filter(|m| !m.is_empty());
        return tool_error(match message {
            Some(m) => m,
            None => format!("The tool failed (status {}).", reply.status),
        });
    }
    let content = match (reply.mcp_content, reply.text, reply.json) {
        (Some(c), _, _) => {
            let valid = c.as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .all(|b| b.get("type").is_some_and(Value::is_string))
            });
            if !valid {
                return tool_error(
                    "The tool returned invalid MCP content: `mcp_content` must be a list of content blocks, each with a `type`.",
                );
            }
            c.clone()
        }
        (None, Some(t), _) => json!([{ "type": "text", "text": t }]),
        (None, None, Some(j)) => json!([{ "type": "text", "text": j.to_string() }]),
        (None, None, None) if reply.other_body => {
            return tool_error(
                "The tool returned a raw or streamed body, which MCP results can't carry. Return `json`, `text` or `mcp_content`.",
            );
        }
        (None, None, None) => json!([]),
    };
    let mut out = Map::new();
    out.insert("content".into(), content);
    if let Some(j) = reply.json {
        out.insert("structuredContent".into(), j.clone());
    }
    out
}

/// The text of a tool error for arguments that don't match the input schema.
pub fn invalid_arguments(errors: &[FieldError]) -> String {
    let lines: Vec<String> = errors
        .iter()
        .map(|e| format!("- {}: {}", e.location, e.message))
        .collect();
    format!("Invalid arguments:\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Layer;

    fn headers(method: &str, name: Option<&str>) -> Headers {
        Headers {
            protocol_version: Some(PROTOCOL_VERSION.into()),
            method: Some(method.into()),
            name: name.map(String::from),
            ..Default::default()
        }
    }

    fn call(name: &str, args: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args,
                "_meta": {
                    META_VERSION: PROTOCOL_VERSION,
                    META_CAPABILITIES: {},
                    META_CLIENT: {"name": "test", "version": "1"}
                }
            }
        }))
        .unwrap()
    }

    fn code(body: &[u8], h: &Headers) -> i64 {
        read(body, h).unwrap_err().error.code
    }

    #[test]
    fn reads_a_tool_call() {
        let m = read(
            &call("get_forecast", json!({"city": "Lisbon"})),
            &headers("tools/call", Some("get_forecast")),
        )
        .unwrap();
        assert_eq!(m.id, Some(json!(7)));
        assert_eq!(m.protocol_version.as_deref(), Some(PROTOCOL_VERSION));
        assert_eq!(m.client.as_ref().unwrap()["name"], "test");
        let (name, args) = m.tool_call().unwrap();
        assert_eq!((name, args), ("get_forecast", json!({"city": "Lisbon"})));
    }

    #[test]
    fn rejects_malformed_messages() {
        let h = headers("tools/list", None);
        assert_eq!(code(b"{", &h), PARSE_ERROR);
        assert_eq!(code(b"[]", &h), INVALID_REQUEST);
        assert_eq!(
            code(br#"{"jsonrpc":"2.0","id":null,"method":"x"}"#, &h),
            INVALID_REQUEST
        );
        assert_eq!(
            code(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#, &h),
            INVALID_REQUEST
        );
        assert_eq!(
            code(br#"{"jsonrpc":"2.0","id":1,"method":"x","params":[]}"#, &h),
            INVALID_PARAMS
        );
    }

    #[test]
    fn rejects_repeated_members() {
        // Which `name` counts depends on the parser, so a server behind
        // the proxy could run a tool other than the one checked.
        let twice = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read","name":"delete","arguments":{}}}"#;
        let h = headers("tools/call", Some("delete"));
        let rejected = read(twice, &h).unwrap_err().error;
        assert_eq!(rejected.code, INVALID_REQUEST);
        assert!(
            rejected.message.contains("\"name\""),
            "{}",
            rejected.message
        );

        assert_eq!(duplicate_key(br#"{"a":1,"b":{"a":2},"c":[{"a":3}]}"#), None);
        for (doc, key) in [
            (&br#"{"method":"a","method":"b"}"#[..], "method"),
            (br#"{"params":{"arguments":{"id":1,"id":2}}}"#, "id"),
            (br#"[{"x":[{"k":null,"k":null}]}]"#, "k"),
            // An escape spells the same name.
            (br#"{"name":1,"n\u0061me":2}"#, "name"),
        ] {
            assert_eq!(duplicate_key(doc).as_deref(), Some(key));
        }
        assert_eq!(duplicate_key(b"{"), None);

        // One pass gives the value `serde_json` would, for every kind of value.
        let doc = br#"{"s":"a\u00e9\n","i":-7,"u":18446744073709551615,"f":1.5e3,"t":true,"n":null,"a":[1,[2,{"k":[]}],{}],"o":{"x":{"y":"z"}}}"#;
        assert_eq!(
            parse_json(doc).unwrap(),
            serde_json::from_slice::<Value>(doc).unwrap()
        );
        // Anything after the document is an error, as it is for `serde_json`.
        assert!(matches!(parse_json(b"{} x"), Err(JsonError::Invalid(_))));
        assert!(matches!(parse_json(b""), Err(JsonError::Invalid(_))));
        // A repeat in a document that's malformed further on is malformed.
        assert!(matches!(
            parse_json(br#"{"a":1,"a":2,"#),
            Err(JsonError::Invalid(_))
        ));
        assert!(matches!(
            parse_json(br#"{"a":1,"a":2} x"#),
            Err(JsonError::Invalid(_))
        ));
        // A well-formed call is unaffected.
        let ok = call("get_forecast", json!({"city": "Lisbon", "days": [1, 2]}));
        assert_eq!(duplicate_key(&ok), None);
    }

    #[test]
    fn checking_agrees_with_parsing() {
        // `check_json` reads a body that's passed on unparsed, so it has to
        // accept and refuse exactly what `parse_json` does.
        let wide_ok: String = (0..40).map(|i| format!("\"k{i}\":{i},")).collect();
        let wide_twice = format!("{{{wide_ok}\"k7\":0}}");
        let wide_escaped = format!("{{{wide_ok}\"k\\u0037\":0}}");
        let wide_ok = format!("{{{wide_ok}\"last\":0}}");
        let docs: Vec<&[u8]> = vec![
            br#"{"s":"a\u00e9\n","i":-7,"u":18446744073709551615,"f":1.5e3,"t":true,"n":null,"a":[1,[2,{"k":[]}],{}],"o":{"x":{"y":"z"}}}"#,
            br#"{"a":1,"b":{"a":2},"c":[{"a":3}]}"#,
            b"[]",
            b"null",
            b" 7 ",
            br#""text""#,
            wide_ok.as_bytes(),
            // Repeats: at the top, nested, spelled with an escape, and in
            // an object with more members than are scanned one by one.
            br#"{"method":"a","method":"b"}"#,
            br#"[{"x":[{"k":null,"k":null}]}]"#,
            br#"{"name":1,"n\u0061me":2}"#,
            wide_twice.as_bytes(),
            wide_escaped.as_bytes(),
            // Malformed, with and without a repeat before the fault.
            b"",
            b"{",
            b"{} x",
            br#"{"a":1,"a":2,"#,
            br#"{"a":1,"a":2} x"#,
            br#"{"a":1e999}"#,
            br#"{"a":"\ud800"}"#,
            b"{\"a\":\"\xff\"}",
            b"{\"\xff\":1}",
            b"{\"a\":\"line\nbreak\"}",
            br#"[1,]"#,
            br#"{"a":01}"#,
        ];
        let mut refused = 0;
        for doc in docs {
            let text = String::from_utf8_lossy(doc);
            match (parse_json(doc), check_json(doc)) {
                (Ok(_), Ok(())) => {}
                (Err(JsonError::Duplicate(a)), Err(JsonError::Duplicate(b))) => {
                    assert_eq!(a, b, "{text}");
                    refused += 1;
                }
                (Err(JsonError::Invalid(_)), Err(JsonError::Invalid(_))) => refused += 1,
                (parsed, checked) => panic!("{text}: parsed {parsed:?}, checked {checked:?}"),
            }
        }
        assert_eq!(refused, 17);
        // Nesting deeper than the parser allows is refused by both.
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(parse_json(deep.as_bytes()).is_err());
        assert!(check_json(deep.as_bytes()).is_err());
    }

    #[test]
    fn checks_versions_and_headers() {
        let body = call("a", json!({}));
        // Old clients: `initialize`, or no version anywhere.
        let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#;
        let err = read(
            init,
            &Headers {
                method: Some("initialize".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.error.code, UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(
            err.error.data.unwrap()["supported"],
            json!([PROTOCOL_VERSION])
        );
        assert_eq!(err.id, Some(json!(1)));

        let mut h = headers("tools/call", Some("a"));
        h.protocol_version = Some("2025-11-25".into());
        assert_eq!(code(&body, &h), HEADER_MISMATCH, "header and _meta differ");

        let old = String::from_utf8(body.clone())
            .unwrap()
            .replace(PROTOCOL_VERSION, "2030-01-01");
        h.protocol_version = Some("2030-01-01".into());
        assert_eq!(code(old.as_bytes(), &h), UNSUPPORTED_PROTOCOL_VERSION);

        let mut h = headers("tools/call", Some("a"));
        h.protocol_version = None;
        assert_eq!(code(&body, &h), HEADER_MISMATCH);
        assert_eq!(
            code(&body, &headers("tools/list", Some("a"))),
            HEADER_MISMATCH
        );
        assert_eq!(
            code(&body, &headers("tools/call", Some("b"))),
            HEADER_MISMATCH
        );
        assert_eq!(code(&body, &headers("tools/call", None)), HEADER_MISMATCH);
        // `Mcp-Name` may be Base64-encoded.
        assert!(read(&body, &headers("tools/call", Some("=?base64?YQ==?="))).is_ok());

        let no_caps = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
        assert_eq!(code(no_caps, &headers("tools/list", None)), INVALID_PARAMS);

        let mut h = headers("tools/call", Some("a"));
        h.invalid.push("Mcp-Name".into());
        assert_eq!(code(&body, &h), HEADER_MISMATCH);
    }

    #[test]
    fn notifications_skip_version_checks() {
        let note = br#"{"jsonrpc":"2.0","method":"notifications/x"}"#;
        let m = read(
            note,
            &Headers {
                method: Some("notifications/x".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(m.is_notification());
        let no_id = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"a"}}"#;
        let h = Headers {
            method: Some("tools/call".into()),
            name: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(code(no_id, &h), INVALID_REQUEST);
    }

    fn tool(params: Vec<ParamHeader>) -> McpTool {
        McpTool {
            name: "t".into(),
            title: None,
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            param_headers: params,
            route: 0,
            own: Layer::default(),
            input_rule: None,
            binding: None,
        }
    }

    #[test]
    fn param_headers_must_match_arguments() {
        let schema = json!({"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "n": {"type": "integer", "x-mcp-header": "N"},
            "on": {"type": "boolean", "x-mcp-header": "On"}
        }});
        let t = tool(super::super::param_headers(&schema).unwrap());
        let with = |pairs: &[(&str, &str)]| Headers {
            params: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        };
        let ok = with(&[
            ("mcp-param-region", "eu"),
            ("mcp-param-n", "42"),
            ("mcp-param-on", "true"),
        ]);
        assert!(
            check_param_headers(&t, &json!({"region": "eu", "n": 42.0, "on": true}), &ok).is_ok()
        );
        assert!(check_param_headers(&t, &json!({}), &with(&[])).is_ok());
        assert!(check_param_headers(&t, &json!({"region": null}), &with(&[])).is_ok());
        let encoded = with(&[("mcp-param-region", "=?base64?ZXUtd2VzdA==?=")]);
        assert!(check_param_headers(&t, &json!({"region": "eu-west"}), &encoded).is_ok());

        let bad = [
            (json!({"region": "eu"}), with(&[])),
            (json!({}), with(&[("mcp-param-region", "eu")])),
            (json!({"region": "eu"}), with(&[("mcp-param-region", "us")])),
            (json!({"n": 42}), with(&[("mcp-param-n", "41")])),
            (json!({"on": false}), with(&[("mcp-param-on", "true")])),
        ];
        for (args, h) in bad {
            assert_eq!(
                check_param_headers(&t, &args, &h).unwrap_err().code,
                HEADER_MISMATCH,
                "{args}"
            );
        }
    }

    #[test]
    fn builds_results_and_errors() {
        let server = ServerInfo {
            name: "acme".into(),
            version: "1.0".into(),
            title: None,
        };
        let r = result(&json!(3), tool_error("nope"), Some(&server));
        assert_eq!(r["id"], 3);
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(r["result"]["content"][0]["text"], "nope");
        assert_eq!(r["result"]["_meta"][META_SERVER]["name"], "acme");
        assert!(
            result(&json!(3), Map::new(), None)["result"]
                .get("_meta")
                .is_none()
        );

        let e = RpcError::unsupported(None).to_json(Some(&json!("a")));
        assert_eq!(e["id"], "a");
        assert_eq!(e["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
        assert!(
            RpcError::new(PARSE_ERROR, "x")
                .to_json(None)
                .get("id")
                .is_none()
        );
    }

    fn endpoint(tools: Vec<McpTool>, list_tools: ListTools) -> McpEndpoint {
        McpEndpoint {
            path: "/mcp".into(),
            route: 0,
            server: ServerInfo {
                name: "acme".into(),
                version: "1".into(),
                title: None,
            },
            instructions: Some("Be nice.".into()),
            list_tools,
            list_ttl: std::time::Duration::from_secs(60),
            proxy: None,
            tools,
        }
    }

    #[test]
    fn discover_and_list() {
        let mut secret = tool(Vec::new());
        secret.name = "secret".into();
        secret.own.auth = Some(crate::policy::AuthReq::Public);
        let open = tool(Vec::new());
        let mcp = endpoint(vec![secret, open], ListTools::Permitted);

        let d = discover(&mcp);
        assert_eq!(d["supportedVersions"], json!([PROTOCOL_VERSION]));
        assert_eq!(d["capabilities"], json!({"tools": {}}));
        assert_eq!(d["instructions"], "Be nice.");
        assert_eq!(d["ttlMs"], 60_000);

        let all = list_tools(&mcp, |_| true);
        assert_eq!(all["tools"].as_array().unwrap().len(), 2);
        assert_eq!(all["cacheScope"], "private", "filtered by scopes");
        let some = list_tools(&mcp, |t| t.own.auth.is_none());
        assert_eq!(some["tools"][0]["name"], "t");

        let everyone = endpoint(mcp.tools.clone(), ListTools::All);
        assert_eq!(list_tools(&everyone, |_| true)["cacheScope"], "public");
    }

    #[test]
    fn maps_handler_replies() {
        let ok = |r: Reply| call_result(Reply { status: 200, ..r });
        let j = json!({"temp": 21});
        let r = ok(Reply {
            json: Some(&j),
            ..Default::default()
        });
        assert_eq!(r["structuredContent"], j);
        assert_eq!(r["content"][0]["text"], r#"{"temp":21}"#);
        assert!(r.get("isError").is_none());

        let r = ok(Reply {
            text: Some("hi"),
            ..Default::default()
        });
        assert_eq!(r["content"], json!([{"type": "text", "text": "hi"}]));
        assert!(r.get("structuredContent").is_none());

        let blocks = json!([{"type": "image", "data": "AAAA", "mimeType": "image/png"}]);
        let r = ok(Reply {
            mcp_content: Some(&blocks),
            json: Some(&j),
            ..Default::default()
        });
        assert_eq!(r["content"], blocks);
        assert_eq!(
            r["structuredContent"], j,
            "json still gives structured content"
        );

        let bad = json!([{"data": "x"}]);
        assert_eq!(
            ok(Reply {
                mcp_content: Some(&bad),
                ..Default::default()
            })["isError"],
            true
        );
        assert_eq!(
            ok(Reply {
                other_body: true,
                ..Default::default()
            })["isError"],
            true
        );
        assert_eq!(ok(Reply::default())["content"], json!([]));

        let e = json!({"error": "city not found"});
        let r = call_result(Reply {
            status: 404,
            json: Some(&e),
            ..Default::default()
        });
        assert_eq!(r["isError"], true);
        assert_eq!(r["content"][0]["text"], "city not found");
        let r = call_result(Reply {
            status: 500,
            ..Default::default()
        });
        assert_eq!(r["content"][0]["text"], "The tool failed (status 500).");
        let r = call_result(Reply {
            status: 400,
            text: Some("bad date\n"),
            ..Default::default()
        });
        assert_eq!(r["content"][0]["text"], "bad date");
    }

    #[test]
    fn base64() {
        assert_eq!(decode_header_value("plain").as_deref(), Some("plain"));
        assert_eq!(
            decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert_eq!(decode_header_value("=?base64?!!?="), None);
    }

    #[test]
    fn rejections_and_trusted_headers() {
        use http::StatusCode;
        assert_eq!(
            rejection_text(StatusCode::FORBIDDEN, br#"{"detail":"no access"}"#),
            "The call was rejected (403 Forbidden): no access"
        );
        assert_eq!(
            rejection_text(StatusCode::FORBIDDEN, b""),
            "The call was rejected (403 Forbidden)."
        );
        let mut headers = http::HeaderMap::new();
        headers.insert("mcp-param-region", "eu".parse().unwrap());
        headers.insert("x-other", "1".parse().unwrap());
        let trusted = trusted_headers(&headers, None);
        assert!(!trusted.contains_key("mcp-param-region"));
        assert!(trusted.contains_key("x-other"));
        let parsed = Headers::from_http(&headers);
        assert_eq!(
            parsed.params,
            vec![("mcp-param-region".into(), "eu".into())]
        );
    }
}
