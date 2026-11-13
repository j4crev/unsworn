//! OpenAPI 3.0 / 3.1 as a route source.

use serde_json::{Map, Value, json};

use crate::validate::ParamSpec;

const METHODS: &[&str] = &["get", "put", "post", "delete", "options", "head", "patch"];

/// One operation from the spec.
#[derive(Debug, Clone, Default)]
pub struct Operation {
    pub method: String,
    pub path: String,
    pub operation_id: Option<String>,
    pub tags: Vec<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub handler: Option<String>,
    pub params: Vec<ParamSpec>,
    pub query: Vec<ParamSpec>,
    pub headers: Vec<ParamSpec>,
    pub body: Option<(Value, bool)>,
    pub response: Option<Value>,
    /// `None` = no security declared. `Some(vec![])` = explicitly public.
    /// Otherwise any one of the listed (scheme, scopes) requirements.
    pub security: Option<Vec<(String, Vec<String>)>>,
    /// Where it is in the spec, for messages: `/users/{id} get`.
    pub location: String,
}

/// Reads operations from a spec document.
pub fn import(doc: &Value) -> Result<Vec<Operation>, Vec<String>> {
    let mut errors = Vec::new();
    let version = doc.get("openapi").and_then(Value::as_str).unwrap_or("");
    if !version.starts_with("3.") {
        return Err(vec![format!(
            "only OpenAPI 3.0 and 3.1 are supported (found \"{version}\")"
        )]);
    }
    let is_30 = version.starts_with("3.0");
    let global_security = doc.get("security");
    let mut ops = Vec::new();

    let Some(paths) = doc.get("paths").and_then(Value::as_object) else {
        return Ok(ops);
    };
    for (path, item) in paths {
        let item = match resolve(doc, item, 0) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{path}: {e}"));
                continue;
            }
        };
        let shared_params = item.get("parameters").cloned().unwrap_or(json!([]));
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            let location = format!("{path} {method}");
            match operation(
                doc,
                path,
                method,
                op,
                &shared_params,
                global_security,
                is_30,
            ) {
                Ok(o) => ops.push(Operation { location, ..o }),
                Err(e) => errors.extend(e.into_iter().map(|e| format!("{location}: {e}"))),
            }
        }
    }
    if errors.is_empty() {
        Ok(ops)
    } else {
        Err(errors)
    }
}

fn operation(
    doc: &Value,
    path: &str,
    method: &str,
    op: &Value,
    shared_params: &Value,
    global_security: Option<&Value>,
    is_30: bool,
) -> Result<Operation, Vec<String>> {
    let mut errors = Vec::new();
    let op = resolve(doc, op, 0).map_err(|e| vec![e])?;
    let mut out = Operation {
        method: method.to_ascii_uppercase(),
        path: path.to_string(),
        operation_id: op
            .get("operationId")
            .and_then(Value::as_str)
            .map(String::from),
        tags: op
            .get("tags")
            .and_then(Value::as_array)
            .map(|t| {
                t.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        summary: op.get("summary").and_then(Value::as_str).map(String::from),
        description: op
            .get("description")
            .and_then(Value::as_str)
            .map(String::from),
        ..Default::default()
    };

    if let Some(obj) = op.as_object() {
        for key in obj.keys() {
            if key.starts_with("x-unsworn-") && key != "x-unsworn-handler" {
                errors.push(format!(
                    "`{key}` isn't supported: `x-unsworn-handler` is the only Unsworn field allowed in the spec; put policies in unsworn.toml"
                ));
            }
        }
    }
    out.handler = match op.get("x-unsworn-handler") {
        None => None,
        Some(Value::String(h)) => Some(h.clone()),
        Some(_) => {
            errors.push("`x-unsworn-handler` must be a string".into());
            None
        }
    };

    // Operation-level parameters override path-level ones with the same name and location.
    let mut params: Vec<Value> = Vec::new();
    let op_params = op.get("parameters").cloned().unwrap_or(json!([]));
    for p in shared_params
        .as_array()
        .into_iter()
        .flatten()
        .chain(op_params.as_array().into_iter().flatten())
    {
        let p = match resolve(doc, p, 0) {
            Ok(p) => p,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let key = (p.get("name").cloned(), p.get("in").cloned());
        params.retain(|q| (q.get("name").cloned(), q.get("in").cloned()) != key);
        params.push(p);
    }
    for p in params {
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let location = p.get("in").and_then(Value::as_str).unwrap_or_default();
        let mut schema = p
            .get("schema")
            .cloned()
            .unwrap_or(json!({"type": "string"}));
        if is_30 {
            convert_30(&mut schema);
        }
        let required = p
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(location == "path");
        let spec = ParamSpec {
            name: if location == "header" {
                name.to_ascii_lowercase()
            } else {
                name
            },
            schema,
            required,
        };
        match location {
            "path" => out.params.push(spec),
            "query" => out.query.push(spec),
            "header" => out.headers.push(spec),
            "cookie" => {}
            other => errors.push(format!("unknown parameter location \"{other}\"")),
        }
    }

    if let Some(body) = op.get("requestBody") {
        let body = resolve(doc, body, 0).map_err(|e| vec![e])?;
        let required = body
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(schema) = json_schema_of(&body) {
            let mut schema = schema;
            if is_30 {
                convert_30(&mut schema);
            }
            out.body = Some((schema, required));
        }
    }
    if let Some(responses) = op.get("responses").and_then(Value::as_object) {
        for code in ["200", "201", "2XX", "default"] {
            if let Some(resp) = responses.get(code) {
                let resp = resolve(doc, resp, 0).map_err(|e| vec![e])?;
                if let Some(mut schema) = json_schema_of(&resp) {
                    if is_30 {
                        convert_30(&mut schema);
                    }
                    out.response = Some(schema);
                }
                break;
            }
        }
    }

    let security = op.get("security").or(global_security);
    if let Some(sec) = security {
        match parse_security(sec) {
            Ok(s) => out.security = Some(s),
            Err(e) => errors.push(e),
        }
    }

    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn json_schema_of(obj: &Value) -> Option<Value> {
    let content = obj.get("content")?.as_object()?;
    content
        .iter()
        .find(|(ct, _)| ct.starts_with("application/json") || ct.ends_with("+json"))
        .and_then(|(_, media)| media.get("schema").cloned())
}

fn parse_security(sec: &Value) -> Result<Vec<(String, Vec<String>)>, String> {
    let list = sec.as_array().ok_or("`security` must be a list")?;
    let mut out = Vec::new();
    for req in list {
        let obj = req
            .as_object()
            .ok_or("each security requirement must be an object")?;
        if obj.is_empty() {
            // `{}` means "no auth" is one of the alternatives.
            return Ok(Vec::new());
        }
        if obj.len() > 1 {
            return Err(
                "security requirements that combine several schemes aren't supported yet".into(),
            );
        }
        let (name, scopes) = obj.iter().next().expect("one entry");
        let scopes = scopes
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        out.push((name.clone(), scopes));
    }
    Ok(out)
}

/// Replaces local `$ref`s (`#/components/...`) with their targets.
pub fn resolve(doc: &Value, value: &Value, depth: usize) -> Result<Value, String> {
    if depth > 32 {
        return Err("`$ref`s nest too deeply (recursive schemas aren't supported yet)".into());
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                let pointer = r
                    .strip_prefix('#')
                    .ok_or_else(|| format!("only local `$ref`s are supported (found \"{r}\")"))?;
                let target = doc
                    .pointer(pointer)
                    .ok_or_else(|| format!("`$ref` \"{r}\" doesn't point to anything"))?;
                return resolve(doc, target, depth + 1);
            }
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), resolve(doc, v, depth + 1)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(|v| resolve(doc, v, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// OpenAPI 3.0 schemas use `nullable: true` instead of `type: [.., "null"]`.
fn convert_30(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            if map.remove("nullable") == Some(Value::Bool(true))
                && let Some(Value::String(t)) = map.get("type").cloned()
            {
                map.insert("type".into(), json!([t, "null"]));
            }
            for v in map.values_mut() {
                convert_30(v);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(convert_30),
        _ => {}
    }
}

/// Fills `{operationId}` and `{tag}` in a handler template.
pub fn handler_from_template(template: &str, op: &Operation) -> Result<String, String> {
    let mut out = template.to_string();
    if out.contains("{operationId}") {
        let id = op
            .operation_id
            .as_deref()
            .ok_or("the handler template uses {operationId}, but the operation has none")?;
        out = out.replace("{operationId}", id);
    }
    if out.contains("{tag}") {
        let tag = op
            .tags
            .first()
            .ok_or("the handler template uses {tag}, but the operation has no tags")?;
        out = out.replace("{tag}", tag);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Value {
        json!({
            "openapi": "3.0.3",
            "security": [{"jwt": []}],
            "components": {
                "schemas": {
                    "User": {"type": "object", "properties": {"name": {"type": "string", "nullable": true}}}
                },
                "parameters": {
                    "Id": {"name": "id", "in": "path", "required": true, "schema": {"type": "integer", "minimum": 1}}
                }
            },
            "paths": {
                "/users/{id}": {
                    "parameters": [{"$ref": "#/components/parameters/Id"}],
                    "get": {
                        "operationId": "getUser",
                        "tags": ["users"],
                        "x-unsworn-handler": "src/users.ts#getUser",
                        "responses": {"200": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}}}
                    },
                    "put": {
                        "operationId": "updateUser",
                        "tags": ["users"],
                        "security": [{"jwt": ["users:write"]}],
                        "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}},
                        "responses": {}
                    }
                },
                "/health": {"get": {"security": [], "responses": {}}}
            }
        })
    }

    #[test]
    fn imports_operations() {
        let ops = import(&spec()).unwrap();
        assert_eq!(ops.len(), 3);
        let get = ops
            .iter()
            .find(|o| o.operation_id.as_deref() == Some("getUser"))
            .unwrap();
        assert_eq!(get.handler.as_deref(), Some("src/users.ts#getUser"));
        assert_eq!(get.params[0].name, "id");
        assert_eq!(get.security, Some(vec![("jwt".into(), vec![])]));
        let schema = get.response.as_ref().unwrap();
        assert_eq!(
            schema["properties"]["name"]["type"],
            json!(["string", "null"])
        );

        let put = ops.iter().find(|o| o.method == "PUT").unwrap();
        assert!(put.body.as_ref().unwrap().1);
        assert_eq!(
            put.security,
            Some(vec![("jwt".into(), vec!["users:write".into()])])
        );

        let health = ops.iter().find(|o| o.path == "/health").unwrap();
        assert_eq!(health.security, Some(vec![]));
        assert_eq!(
            handler_from_template("src/{tag}.ts#{operationId}", put).unwrap(),
            "src/users.ts#updateUser"
        );
    }

    #[test]
    fn rejects_other_unsworn_fields() {
        let mut doc = spec();
        doc["paths"]["/health"]["get"]["x-unsworn-cache"] = json!({"ttl": "5s"});
        let errors = import(&doc).unwrap_err();
        assert!(errors[0].contains("x-unsworn-cache"), "{errors:?}");
    }
}
