//! Request and response validation.
//!
//! Path params, query params and headers arrive as strings. Each location
//! is described by a JSON Schema object; values are converted to their
//! declared types (`"42"` becomes `42`) and then validated.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::config::RawParam;

/// One validation failure, e.g. `query.limit: 500 is greater than 100`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldError {
    pub location: String,
    pub message: String,
}

fn compile(schema: &Value) -> Result<Arc<jsonschema::Validator>, String> {
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .map(Arc::new)
        .map_err(|e| format!("invalid JSON Schema: {e}"))
}

fn collect_errors(
    validator: &jsonschema::Validator,
    instance: &Value,
    prefix: &str,
) -> Vec<FieldError> {
    validator
        .iter_errors(instance)
        .map(|e| {
            let path = e.instance_path().to_string();
            let location = if path.is_empty() || path == "/" {
                prefix.to_string()
            } else {
                format!(
                    "{prefix}.{}",
                    path.trim_start_matches('/').replace('/', ".")
                )
            };
            FieldError {
                location,
                message: e.to_string(),
            }
        })
        .collect()
}

/// Converts an inline rule (`{ type = "integer", min = 1 }`) to JSON Schema.
pub fn param_schema(rule: &RawParam) -> Result<Value, String> {
    let mut schema = Map::new();
    let kind = rule.kind.as_deref().unwrap_or("string");
    match kind {
        "string" | "integer" | "number" | "boolean" => {
            schema.insert("type".into(), json!(kind));
        }
        "array" => {
            schema.insert("type".into(), json!("array"));
            let items = rule.items.as_deref().unwrap_or("string");
            if !matches!(items, "string" | "integer" | "number" | "boolean") {
                return Err(format!("unknown items type \"{items}\""));
            }
            schema.insert("items".into(), json!({ "type": items }));
        }
        other => {
            return Err(format!(
                "unknown type \"{other}\" (use string, integer, number, boolean or array)"
            ));
        }
    }
    let numeric = matches!(kind, "integer" | "number");
    if let Some(min) = rule.min {
        let key = if numeric { "minimum" } else { "minItems" };
        if !numeric && kind != "array" {
            return Err("`min` applies to numbers and arrays; use `min_length` for strings".into());
        }
        schema.insert(key.into(), num(min));
    }
    if let Some(max) = rule.max {
        let key = if numeric { "maximum" } else { "maxItems" };
        if !numeric && kind != "array" {
            return Err("`max` applies to numbers and arrays; use `max_length` for strings".into());
        }
        schema.insert(key.into(), num(max));
    }
    if let Some(v) = rule.min_length {
        schema.insert("minLength".into(), json!(v));
    }
    if let Some(v) = rule.max_length {
        schema.insert("maxLength".into(), json!(v));
    }
    if let Some(v) = &rule.one_of {
        schema.insert("enum".into(), json!(v));
    }
    if let Some(v) = &rule.pattern {
        schema.insert("pattern".into(), json!(v));
    }
    if let Some(v) = &rule.format {
        schema.insert("format".into(), json!(v));
    }
    if let Some(v) = &rule.default {
        schema.insert("default".into(), v.clone());
    }
    Ok(Value::Object(schema))
}

fn num(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

/// A declared parameter.
#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: String,
    pub schema: Value,
    pub required: bool,
}

/// Rules for one location: path params, query params or headers.
#[derive(Clone)]
pub struct LocationRules {
    pub prefix: &'static str,
    pub specs: Vec<ParamSpec>,
    validator: Arc<jsonschema::Validator>,
    /// The combined object schema, for OpenAPI output.
    pub schema: Value,
}

impl std::fmt::Debug for LocationRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocationRules")
            .field("prefix", &self.prefix)
            .field("specs", &self.specs)
            .finish()
    }
}

impl LocationRules {
    pub fn new(prefix: &'static str, specs: Vec<ParamSpec>) -> Result<Self, String> {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for spec in &specs {
            properties.insert(spec.name.clone(), spec.schema.clone());
            if spec.required {
                required.push(json!(spec.name));
            }
        }
        let schema = json!({
            "type": "object",
            "properties": properties,
            "required": required,
        });
        let validator = compile(&schema)?;
        Ok(LocationRules {
            prefix,
            specs,
            validator,
            schema,
        })
    }

    /// Converts and validates raw string values. Undeclared names pass
    /// through as strings.
    pub fn apply(&self, raw: &[(String, String)]) -> Result<Map<String, Value>, Vec<FieldError>> {
        let mut out = Map::new();
        for (name, value) in raw {
            if !self.specs.iter().any(|s| &s.name == name) && !out.contains_key(name) {
                out.insert(name.clone(), Value::String(value.clone()));
            }
        }
        for spec in &self.specs {
            let values: Vec<&str> = raw
                .iter()
                .filter(|(n, _)| n == &spec.name)
                .map(|(_, v)| v.as_str())
                .collect();
            if values.is_empty() {
                if let Some(default) = spec.schema.get("default") {
                    out.insert(spec.name.clone(), default.clone());
                }
                continue;
            }
            let types = declared_types(&spec.schema);
            let value = if types.iter().any(|t| t == "array") {
                let item_types = spec
                    .schema
                    .get("items")
                    .map(declared_types)
                    .unwrap_or_default();
                Value::Array(
                    values
                        .iter()
                        .flat_map(|v| v.split(','))
                        .map(|v| coerce(v, &item_types))
                        .collect(),
                )
            } else {
                coerce(values[values.len() - 1], &types)
            };
            out.insert(spec.name.clone(), value);
        }
        let instance = Value::Object(out);
        let errors = collect_errors(&self.validator, &instance, self.prefix);
        if errors.is_empty() {
            let Value::Object(out) = instance else {
                unreachable!()
            };
            Ok(out)
        } else {
            Err(errors)
        }
    }
}

fn declared_types(schema: &Value) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn coerce(raw: &str, types: &[String]) -> Value {
    for t in types {
        match t.as_str() {
            "integer" => {
                if let Ok(i) = raw.parse::<i64>() {
                    return json!(i);
                }
            }
            "number" => {
                if let Ok(n) = raw.parse::<f64>()
                    && n.is_finite()
                {
                    return json!(n);
                }
            }
            "boolean" => match raw {
                "true" | "1" => return json!(true),
                "false" | "0" => return json!(false),
                _ => {}
            },
            "null" if raw.is_empty() || raw == "null" => return Value::Null,
            _ => {}
        }
    }
    Value::String(raw.to_string())
}

/// A JSON Schema for a request or response body.
#[derive(Clone)]
pub struct BodyRule {
    pub schema: Value,
    pub required: bool,
    validator: Arc<jsonschema::Validator>,
}

impl std::fmt::Debug for BodyRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyRule")
            .field("required", &self.required)
            .finish()
    }
}

impl BodyRule {
    pub fn new(schema: Value, required: bool) -> Result<Self, String> {
        let validator = compile(&schema)?;
        Ok(BodyRule {
            schema,
            required,
            validator,
        })
    }

    pub fn check(&self, value: &Value, prefix: &str) -> Result<(), Vec<FieldError>> {
        let errors = collect_errors(&self.validator, value, prefix);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Everything a route validates.
#[derive(Debug, Clone, Default)]
pub struct Validation {
    pub params: Option<LocationRules>,
    pub query: Option<LocationRules>,
    pub headers: Option<LocationRules>,
    pub body: Option<BodyRule>,
    pub response: Option<BodyRule>,
}

impl Validation {
    pub fn is_empty(&self) -> bool {
        self.params.is_none()
            && self.query.is_none()
            && self.headers.is_none()
            && self.body.is_none()
            && self.response.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(kind: &str) -> RawParam {
        RawParam {
            kind: Some(kind.into()),
            ..Default::default()
        }
    }

    #[test]
    fn converts_and_validates() {
        let id = RawParam {
            min: Some(1.0),
            ..rule("integer")
        };
        let rules = LocationRules::new(
            "params",
            vec![ParamSpec {
                name: "id".into(),
                schema: param_schema(&id).unwrap(),
                required: true,
            }],
        )
        .unwrap();
        let ok = rules.apply(&[("id".into(), "42".into())]).unwrap();
        assert_eq!(ok["id"], json!(42));

        let errors = rules.apply(&[("id".into(), "0".into())]).unwrap_err();
        assert_eq!(errors[0].location, "params.id");
        let errors = rules.apply(&[("id".into(), "abc".into())]).unwrap_err();
        assert!(errors[0].message.contains("integer"), "{:?}", errors);
        let errors = rules.apply(&[]).unwrap_err();
        assert_eq!(errors[0].location, "params");
    }

    #[test]
    fn query_defaults_arrays_and_passthrough() {
        let limit = RawParam {
            default: Some(json!(20)),
            max: Some(100.0),
            ..rule("integer")
        };
        let tags = RawParam {
            items: Some("string".into()),
            ..rule("array")
        };
        let rules = LocationRules::new(
            "query",
            vec![
                ParamSpec {
                    name: "limit".into(),
                    schema: param_schema(&limit).unwrap(),
                    required: false,
                },
                ParamSpec {
                    name: "tag".into(),
                    schema: param_schema(&tags).unwrap(),
                    required: false,
                },
            ],
        )
        .unwrap();
        let out = rules
            .apply(&[
                ("tag".into(), "a".into()),
                ("tag".into(), "b,c".into()),
                ("other".into(), "x".into()),
            ])
            .unwrap();
        assert_eq!(out["limit"], json!(20));
        assert_eq!(out["tag"], json!(["a", "b", "c"]));
        assert_eq!(out["other"], json!("x"));
        assert!(rules.apply(&[("limit".into(), "500".into())]).is_err());
    }

    #[test]
    fn body_rules() {
        let rule = BodyRule::new(
            json!({"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}),
            true,
        )
        .unwrap();
        assert!(rule.check(&json!({"name": "Ada"}), "body").is_ok());
        let errors = rule.check(&json!({"name": 3}), "body").unwrap_err();
        assert_eq!(errors[0].location, "body.name");
    }

    #[test]
    fn inline_rule_errors() {
        assert!(param_schema(&rule("date")).is_err());
        assert!(
            param_schema(&RawParam {
                min: Some(1.0),
                ..rule("string")
            })
            .is_err()
        );
    }
}
