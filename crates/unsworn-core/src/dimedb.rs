//! Pure `[dimedb]` configuration, schema parsing, JSON Schema derivation,
//! and virtual handler synthesis for `unsworn-core` (no I/O, `wasm32`-clean).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::DimeDbMode;
use crate::policy::HandlerRef;

/// Built-in virtual runtime name for dimeDB handlers (`ctx.call` and MCP tools).
pub const RUNTIME: &str = "dimedb";

/// Parsed `dimedb.toml` schema (subset needed for routing, validation, and MCP).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DimeSchema {
    pub app: DimeApp,
    #[serde(default)]
    pub tables: Vec<DimeTable>,
    #[serde(default)]
    pub commands: Vec<DimeVariant>,
    #[serde(default)]
    pub replies: Vec<DimeVariant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DimeApp {
    pub name: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DimeTable {
    pub name: String,
    pub tag: u8,
    pub key: String,
    #[serde(default)]
    pub fields: Vec<DimeField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DimeField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DimeVariant {
    pub name: String,
    pub tag: u8,
    #[serde(default)]
    pub fields: Vec<DimeField>,
}

/// Compiled `[dimedb]` configuration stored on [`crate::Project`].
#[derive(Debug, Clone)]
pub struct DimeDbConfig {
    pub mode: DimeDbMode,
    pub schema_path: String,
    pub schema_toml: String,
    pub schema: DimeSchema,
    pub dir: String,
    pub rules_path: Option<String>,
    pub url: Option<String>,
    pub retry_window: Option<Duration>,
    pub lookup_window: Option<Duration>,
    pub prefix: String,
    pub mcp: bool,
    pub fuel_per_check: u64,
}

impl DimeSchema {
    pub fn from_toml(src: &str) -> Result<Self, String> {
        toml::from_str(src).map_err(|e| format!("invalid dimedb.toml: {e}"))
    }
}

/// Converts `PascalCase` (e.g. `CreateAccount`) to `snake_case` (`create_account`).
pub fn to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Maps a `dimedb.toml` wire type string to a JSON Schema object.
pub fn wire_type_to_json_schema(ty: &str) -> Value {
    let ty = ty.trim();
    match ty {
        "()" => json!({ "type": "null" }),
        "bool" => json!({ "type": "boolean" }),
        "u8" | "u16" | "u32" | "u64" => json!({ "type": "integer", "minimum": 0 }),
        "i8" | "i16" | "i32" | "i64" => json!({ "type": "integer" }),
        "u128" | "i128" | "String" => json!({ "type": "string" }),
        _ if ty.starts_with("Option<") && ty.ends_with('>') => {
            let inner = wire_type_to_json_schema(&ty[7..ty.len() - 1]);
            json!({ "anyOf": [inner, { "type": "null" }] })
        }
        _ if ty.starts_with("Vec<") && ty.ends_with('>') => {
            let inner = wire_type_to_json_schema(&ty[4..ty.len() - 1]);
            json!({ "type": "array", "items": inner })
        }
        _ => json!({}),
    }
}

/// JSON Schema for a mutating dimeDB command (requires `request_id` + command fields).
pub fn command_input_schema(cmd: &DimeVariant) -> Value {
    let mut props = Map::new();
    props.insert(
        "request_id".into(),
        json!({
            "description": "Idempotency key (UUIDv7 string or non-negative integer). Reuse the same request_id on retries to guarantee at-most-once execution.",
            "anyOf": [{ "type": "string" }, { "type": "integer", "minimum": 0 }]
        }),
    );
    let mut required = vec![Value::String("request_id".into())];
    for f in &cmd.fields {
        props.insert(f.name.clone(), wire_type_to_json_schema(&f.ty));
        required.push(Value::String(f.name.clone()));
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false
    })
}

/// JSON Schema for `dimedb.lookup` / `dimedb_lookup`.
pub fn lookup_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "request_id": {
                "description": "The request_id (UUIDv7 string or integer) to look up.",
                "anyOf": [{ "type": "string" }, { "type": "integer", "minimum": 0 }]
            }
        },
        "required": ["request_id"],
        "additionalProperties": false
    })
}

/// JSON Schema for `dimedb.get.<table>` / `dimedb_get_<table>`.
pub fn get_input_schema(table: &DimeTable) -> Value {
    json!({
        "type": "object",
        "properties": {
            "key": wire_type_to_json_schema(&table.key)
        },
        "required": ["key"],
        "additionalProperties": false
    })
}

/// Constructs a virtual [`HandlerRef`] routed to the in-process or remote
/// dimeDB backend.
pub fn handler_ref(op: &str) -> HandlerRef {
    HandlerRef {
        raw: format!("{RUNTIME}:{op}"),
        file: RUNTIME.to_string(),
        name: Some(op.to_string()),
        runtime: RUNTIME.to_string(),
    }
}
