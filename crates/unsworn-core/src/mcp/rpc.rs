//! Reading MCP JSON-RPC requests and building replies.
//!
//! Hosts pass in the body bytes and the request metadata headers; this
//! module checks them against each other and against MCP 2026-07-28, and
//! never touches the network.

use serde_json::{Map, Value, json};

use super::{McpTool, PROTOCOL_VERSION, ParamHeader, ParamKind, SUPPORTED_VERSIONS, ServerInfo};
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
    fn param(&self, header: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == header)
            .map(|(_, v)| v.as_str())
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
    let value: Value = serde_json::from_slice(body).map_err(|e| {
        reject(
            None,
            RpcError::new(PARSE_ERROR, format!("Parse error: {e}")),
        )
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

    #[test]
    fn base64() {
        assert_eq!(decode_header_value("plain").as_deref(), Some("plain"));
        assert_eq!(
            decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert_eq!(decode_header_value("=?base64?!!?="), None);
    }
}
