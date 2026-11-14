//! MCP endpoints: the compiled `[mcp]` table and its tools.
//!
//! Unsworn speaks MCP protocol version [`PROTOCOL_VERSION`] only. That
//! version is stateless: no sessions and no `initialize` handshake. Every
//! request carries its protocol version, and each request is handled on its
//! own. See `docs/mcp.md`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::config::ListTools;
use crate::policy::Layer;
use crate::project::Proxy;

/// The MCP protocol version Unsworn implements.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Every protocol version Unsworn accepts, newest first.
pub const SUPPORTED_VERSIONS: &[&str] = &[PROTOCOL_VERSION];

/// The compiled `[mcp]` table.
#[derive(Debug, Clone)]
pub struct McpEndpoint {
    pub path: String,
    /// The endpoint's route, in [`crate::Project::routes`].
    pub route: usize,
    pub server: ServerInfo,
    pub instructions: Option<String>,
    pub list_tools: ListTools,
    /// How long clients may cache `tools/list` and `server/discover`.
    pub list_ttl: Duration,
    /// Set when Unsworn forwards to an existing MCP server.
    pub proxy: Option<Arc<Proxy>>,
    /// Sorted by name, so `tools/list` is always in the same order.
    pub tools: Vec<McpTool>,
}

impl McpEndpoint {
    pub fn tool(&self, name: &str) -> Option<&McpTool> {
        self.tools
            .binary_search_by(|t| t.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.tools[i])
    }

    /// Whether Unsworn runs the tools itself, rather than forwarding.
    pub fn hosted(&self) -> bool {
        self.proxy.is_none()
    }
}

/// The server identity reported in each result's `_meta`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

impl ServerInfo {
    /// The MCP `Implementation` object.
    pub fn to_json(&self) -> Value {
        let mut out = json!({ "name": self.name, "version": self.version });
        if let Some(t) = &self.title {
            out["title"] = json!(t);
        }
        out
    }
}

/// One compiled `[[mcp.tool]]`.
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The `inputSchema` shown to clients and used to validate arguments.
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// MCP-shaped annotations (`readOnlyHint`, ...), when any are set.
    pub annotations: Option<Value>,
    /// Arguments that clients copy into `Mcp-Param-*` headers.
    pub param_headers: Vec<ParamHeader>,
    /// The tool's route, in [`crate::Project::routes`]. It carries the
    /// handler (or the proxy), validation and merged policies.
    pub route: usize,
    /// The policies the tool sets itself. A call checks these on top of the
    /// ones already checked for the whole endpoint.
    pub own: Layer,
}

impl McpTool {
    /// The tool as `tools/list` describes it.
    pub fn definition(&self) -> Value {
        let mut out = Map::new();
        out.insert("name".into(), json!(self.name));
        if let Some(t) = &self.title {
            out.insert("title".into(), json!(t));
        }
        if let Some(d) = &self.description {
            out.insert("description".into(), json!(d));
        }
        out.insert("inputSchema".into(), self.input_schema.clone());
        if let Some(o) = &self.output_schema {
            out.insert("outputSchema".into(), o.clone());
        }
        if let Some(a) = &self.annotations {
            out.insert("annotations".into(), a.clone());
        }
        Value::Object(out)
    }
}

/// An argument that clients mirror into an `Mcp-Param-{name}` header,
/// declared with `x-mcp-header` in the input schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamHeader {
    /// The `x-mcp-header` value as written, such as `Region`.
    pub name: String,
    /// The full header name in lowercase, such as `mcp-param-region`.
    pub header: String,
    /// The chain of `properties` keys from the schema root to the argument.
    pub path: Vec<String>,
    pub kind: ParamKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    String,
    Integer,
    Boolean,
}

/// Checks a tool name against the MCP naming rules: 1 to 128 ASCII letters,
/// digits, `_`, `-` and `.`.
pub fn check_tool_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err("a tool name must be 1 to 128 characters long".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
    {
        return Err(format!(
            "a tool name can only use letters, digits, `_`, `-` and `.`, not {c:?}"
        ));
    }
    Ok(())
}

/// Checks that an input schema describes an arguments object.
pub fn check_input_schema(schema: &Value) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return Err("the input schema must be a JSON Schema object".into());
    };
    match obj.get("type") {
        Some(Value::String(t)) if t == "object" => Ok(()),
        _ => Err(
            "the input schema must have `\"type\": \"object\"`, since arguments are an object"
                .into(),
        ),
    }
}

/// The MCP annotations object for a tool, or `None` if nothing is set.
pub fn annotations(raw: &crate::config::RawToolAnnotations) -> Option<Value> {
    let mut out = Map::new();
    for (key, value) in [
        ("readOnlyHint", raw.read_only),
        ("destructiveHint", raw.destructive),
        ("idempotentHint", raw.idempotent),
        ("openWorldHint", raw.open_world),
    ] {
        if let Some(v) = value {
            out.insert(key.into(), json!(v));
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// Finds every `x-mcp-header` in an input schema and checks it against the
/// MCP rules. Clients drop a tool whose annotations break them, so these are
/// reported as config errors.
pub fn param_headers(schema: &Value) -> Result<Vec<ParamHeader>, Vec<String>> {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    walk(schema, &mut Vec::new(), true, &mut found, &mut errors);
    let mut seen: Vec<String> = Vec::new();
    for h in &found {
        if seen.contains(&h.header) {
            errors.push(format!(
                "x-mcp-header \"{}\" is used more than once (names are case-insensitive)",
                h.name
            ));
        }
        seen.push(h.header.clone());
    }
    if errors.is_empty() {
        Ok(found)
    } else {
        Err(errors)
    }
}

/// Keywords whose values are data, not subschemas.
const DATA_KEYWORDS: &[&str] = &["enum", "const", "default", "examples", "required"];

fn walk(
    node: &Value,
    path: &mut Vec<String>,
    reachable: bool,
    found: &mut Vec<ParamHeader>,
    errors: &mut Vec<String>,
) {
    let Some(obj) = node.as_object() else {
        if let Some(items) = node.as_array() {
            for item in items {
                walk(item, path, false, found, errors);
            }
        }
        return;
    };
    if let Some(value) = obj.get("x-mcp-header") {
        let at = if path.is_empty() {
            "the schema root".to_string()
        } else {
            format!("`{}`", path.join("."))
        };
        if !reachable || path.is_empty() {
            errors.push(format!(
                "x-mcp-header at {at} must be on a property reached only through `properties`, not through items, $ref, $defs or anyOf/oneOf/allOf/not/if"
            ));
        } else {
            match header_param(value, obj, path) {
                Ok(h) => found.push(h),
                Err(e) => errors.push(format!("x-mcp-header at {at}: {e}")),
            }
        }
    }
    for (key, value) in obj {
        if key == "x-mcp-header" || DATA_KEYWORDS.contains(&key.as_str()) {
            continue;
        }
        if key == "properties"
            && let Some(props) = value.as_object()
        {
            for (name, sub) in props {
                path.push(name.clone());
                walk(sub, path, reachable, found, errors);
                path.pop();
            }
            continue;
        }
        walk(value, path, false, found, errors);
    }
}

fn header_param(
    value: &Value,
    schema: &Map<String, Value>,
    path: &[String],
) -> Result<ParamHeader, String> {
    let Some(name) = value.as_str() else {
        return Err("the value must be a string".into());
    };
    if name.is_empty() {
        return Err("the name can't be empty".into());
    }
    if let Some(c) = name.chars().find(|c| !is_tchar(*c)) {
        return Err(format!("{c:?} isn't allowed in a header name"));
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let non_null: Vec<&str> = types.into_iter().filter(|t| *t != "null").collect();
    let kind = match non_null.as_slice() {
        ["string"] => ParamKind::String,
        ["integer"] => ParamKind::Integer,
        ["boolean"] => ParamKind::Boolean,
        _ => {
            return Err(
                "the property must have one `type` of \"string\", \"integer\" or \"boolean\""
                    .into(),
            );
        }
    };
    Ok(ParamHeader {
        name: name.to_string(),
        header: format!("mcp-param-{}", name.to_ascii_lowercase()),
        path: path.to_vec(),
        kind,
    })
}

/// RFC 9110 `tchar`.
fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names() {
        assert!(check_tool_name("get_forecast").is_ok());
        assert!(check_tool_name("admin.tools.list-v2").is_ok());
        assert!(check_tool_name("").is_err());
        assert!(check_tool_name("get forecast").is_err());
        assert!(check_tool_name(&"a".repeat(129)).is_err());
    }

    #[test]
    fn input_schema_must_be_an_object() {
        assert!(check_input_schema(&json!({"type": "object"})).is_ok());
        assert!(check_input_schema(&json!({"type": "string"})).is_err());
        assert!(check_input_schema(&json!({})).is_err());
    }

    #[test]
    fn finds_param_headers() {
        let schema = json!({
            "type": "object",
            "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"},
                "tenant": {
                    "type": "object",
                    "properties": {"id": {"type": ["integer", "null"], "x-mcp-header": "Tenant-Id"}}
                },
                "query": {"type": "string", "default": {"x-mcp-header": "ignored"}}
            }
        });
        let found = param_headers(&schema).unwrap();
        assert_eq!(found.len(), 2);
        let region = found.iter().find(|h| h.name == "Region").unwrap();
        assert_eq!(region.header, "mcp-param-region");
        assert_eq!(region.path, vec!["region"]);
        let tenant = found.iter().find(|h| h.name == "Tenant-Id").unwrap();
        assert_eq!(tenant.path, vec!["tenant", "id"]);
        assert_eq!(tenant.kind, ParamKind::Integer);
    }

    #[test]
    fn rejects_bad_param_headers() {
        let cases = [
            // Through `items`.
            json!({"type": "object", "properties": {"xs": {"type": "array", "items": {"type": "string", "x-mcp-header": "X"}}}}),
            // Through `anyOf`.
            json!({"type": "object", "anyOf": [{"properties": {"a": {"type": "string", "x-mcp-header": "A"}}}]}),
            // A number.
            json!({"type": "object", "properties": {"n": {"type": "number", "x-mcp-header": "N"}}}),
            // No type.
            json!({"type": "object", "properties": {"n": {"x-mcp-header": "N"}}}),
            // Not a token.
            json!({"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": "A B"}}}),
            // Duplicate, ignoring case.
            json!({"type": "object", "properties": {
                "a": {"type": "string", "x-mcp-header": "Name"},
                "b": {"type": "string", "x-mcp-header": "name"}
            }}),
            // On the root.
            json!({"type": "object", "x-mcp-header": "Root"}),
        ];
        for schema in cases {
            assert!(param_headers(&schema).is_err(), "{schema}");
        }
    }

    #[test]
    fn definition_and_annotations() {
        let raw = crate::config::RawToolAnnotations {
            read_only: Some(true),
            ..Default::default()
        };
        let tool = McpTool {
            name: "get_forecast".into(),
            title: None,
            description: Some("Forecast".into()),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: annotations(&raw),
            param_headers: Vec::new(),
            route: 0,
            own: Layer::default(),
        };
        assert_eq!(
            tool.definition(),
            json!({
                "name": "get_forecast",
                "description": "Forecast",
                "inputSchema": {"type": "object"},
                "annotations": {"readOnlyHint": true}
            })
        );
        assert!(annotations(&Default::default()).is_none());
    }
}
