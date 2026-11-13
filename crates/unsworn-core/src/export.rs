//! OpenAPI output: the routes Unsworn serves, as an OpenAPI 3.1 document.

use serde_json::{Map, Value, json};

use crate::auth::Scheme;
use crate::policy::AuthReq;
use crate::project::{Action, Project, Source};

/// Builds the OpenAPI document. With an OpenAPI source, its spec is the
/// starting point and routes from other sources are added to it.
pub fn openapi(project: &Project) -> Value {
    let mut doc = project.openapi.spec.clone().unwrap_or_else(|| {
        json!({
            "openapi": "3.1.0",
            "info": {
                "title": project.openapi.title.clone().unwrap_or_else(|| "Unsworn API".into()),
                "version": project.openapi.version.clone().unwrap_or_else(|| "0.1.0".into()),
            },
            "paths": {},
        })
    });
    if !doc.get("paths").is_some_and(Value::is_object) {
        doc["paths"] = json!({});
    }

    for route in &project.routes {
        if matches!(route.source, Source::OpenApi { .. }) || route.catch_all.is_some() {
            continue;
        }
        if matches!(route.action, Action::Static { .. } | Action::Proxy(_)) {
            continue;
        }
        let Some(methods) = &route.methods else {
            continue;
        };
        let probe = route.path.clone();
        for method in methods {
            let mut op = Map::new();
            if let Some(info) = &route.op {
                if let Some(id) = &info.operation_id {
                    op.insert("operationId".into(), json!(id));
                }
                if let Some(s) = &info.summary {
                    op.insert("summary".into(), json!(s));
                }
                if !info.tags.is_empty() {
                    op.insert("tags".into(), json!(info.tags));
                }
            }
            let mut params = Vec::new();
            for (rules, location) in [
                (&route.validation.params, "path"),
                (&route.validation.query, "query"),
                (&route.validation.headers, "header"),
            ] {
                if let Some(rules) = rules {
                    for spec in &rules.specs {
                        params.push(json!({
                            "name": spec.name,
                            "in": location,
                            "required": spec.required || location == "path",
                            "schema": spec.schema,
                        }));
                    }
                }
            }
            for name in &route.param_names {
                let declared = params
                    .iter()
                    .any(|p| p["in"] == "path" && p["name"] == json!(name));
                if !declared {
                    params.push(json!({
                        "name": name, "in": "path", "required": true, "schema": {"type": "string"}
                    }));
                }
            }
            if !params.is_empty() {
                op.insert("parameters".into(), Value::Array(params));
            }
            if let Some(body) = &route.validation.body {
                op.insert(
                    "requestBody".into(),
                    json!({
                        "required": body.required,
                        "content": {"application/json": {"schema": body.schema}},
                    }),
                );
            }
            let responses = match (&route.action, &route.validation.response) {
                (_, Some(resp)) => json!({
                    "200": {"description": "OK", "content": {"application/json": {"schema": resp.schema}}}
                }),
                (Action::Respond { status, .. }, None) => json!({
                    status.to_string(): {"description": "Response"}
                }),
                (Action::Redirect { status, .. }, None) => json!({
                    status.to_string(): {"description": "Redirect"}
                }),
                _ => json!({"default": {"description": "Response"}}),
            };
            op.insert("responses".into(), responses);
            let eff = project.effective(route, method, &probe);
            if let AuthReq::AnyOf(alts) = &eff.auth {
                let security: Vec<Value> = alts
                    .iter()
                    .map(|a| json!({ a.scheme.clone(): a.scopes }))
                    .collect();
                op.insert("security".into(), Value::Array(security));
            }
            let path_item = doc["paths"]
                .as_object_mut()
                .expect("paths object")
                .entry(route.path.clone())
                .or_insert_with(|| json!({}));
            path_item[method.to_ascii_lowercase()] = Value::Object(op);
        }
    }

    if !project.schemes.is_empty() {
        let mut schemes = Map::new();
        for (name, scheme) in &project.schemes {
            let value = match scheme.as_ref() {
                Scheme::Jwt(j) if j.header == "authorization" => {
                    json!({"type": "http", "scheme": "bearer", "bearerFormat": "JWT"})
                }
                Scheme::Jwt(j) => json!({"type": "apiKey", "in": "header", "name": j.header}),
                Scheme::ApiKey(k) => match (&k.header, &k.query) {
                    (Some(h), _) => json!({"type": "apiKey", "in": "header", "name": h}),
                    (None, Some(q)) => json!({"type": "apiKey", "in": "query", "name": q}),
                    (None, None) => continue,
                },
            };
            schemes.insert(name.clone(), value);
        }
        let components = doc
            .as_object_mut()
            .expect("document object")
            .entry("components")
            .or_insert_with(|| json!({}));
        let existing = components
            .as_object_mut()
            .map(|c| c.entry("securitySchemes").or_insert_with(|| json!({})));
        if let Some(Value::Object(existing)) = existing {
            for (k, v) in schemes {
                existing.entry(k).or_insert(v);
            }
        }
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::compile;
    use crate::project::tests::MemHost;

    #[test]
    fn exports_config_routes() {
        let cfg = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun"

[auth.jwt]
type = "jwt"
secret = "s"

[openapi]
title = "Acme"

[[route]]
path = "/users/{id}"
method = ["GET", "DELETE"]
auth = { scheme = "jwt", scopes = ["users:read"] }
handler = "src/users.ts#user"
params = { id = { type = "integer" } }
query = { verbose = { type = "boolean" } }
body = { type = "object" }

[[route]]
path = "/{*rest}"
proxy = "http://app"
"#;
        let host = MemHost::with(&[("unsworn.toml", cfg), ("src/users.ts", "")]);
        let p = compile(&host, "unsworn.toml").project.unwrap();
        let doc = openapi(&p);
        assert_eq!(doc["info"]["title"], "Acme");
        let get = &doc["paths"]["/users/{id}"]["get"];
        assert_eq!(get["parameters"][0]["schema"]["type"], "integer");
        assert_eq!(get["parameters"][1]["in"], "query");
        assert_eq!(get["security"][0]["jwt"][0], "users:read");
        assert!(doc["paths"]["/users/{id}"]["delete"].is_object());
        assert!(doc["paths"].get("/{*rest}").is_none());
        assert_eq!(
            doc["components"]["securitySchemes"]["jwt"]["scheme"],
            "bearer"
        );
    }
}
