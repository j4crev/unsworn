use std::collections::BTreeMap;

use super::*;

#[derive(Default)]
pub(crate) struct MemHost {
    pub files: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
}

impl MemHost {
    pub fn with(files: &[(&str, &str)]) -> Self {
        MemHost {
            files: files
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            env: BTreeMap::new(),
        }
    }
}

impl Host for MemHost {
    fn read(&self, path: &str) -> Option<String> {
        self.files.get(path).cloned()
    }
    fn exists(&self, path: &str) -> bool {
        let dir = format!("{}/", path.trim_end_matches('/'));
        self.files.contains_key(path) || self.files.keys().any(|k| k.starts_with(&dir))
    }
    fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        let prefix = format!("{dir}/");
        let out: Vec<String> = self
            .files
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(String::from))
            .collect();
        if out.is_empty() {
            Err("not found".into())
        } else {
            Ok(out)
        }
    }
    fn env(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }
}

const RUNTIMES: &str = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"

[runtimes.go]
extensions = [".go"]
command = "./go-worker"
"#;

fn project(files: &[(&str, &str)]) -> Project {
    let host = MemHost::with(files);
    let compiled = compile(&host, "unsworn.toml");
    for d in &compiled.diagnostics {
        eprintln!("{d}");
    }
    compiled.project.expect("project compiles")
}

fn errors(files: &[(&str, &str)]) -> Vec<String> {
    let host = MemHost::with(files);
    let compiled = compile(&host, "unsworn.toml");
    assert!(compiled.project.is_none(), "expected errors");
    compiled
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.to_string())
        .collect()
}

fn found<'a>(p: &'a Project, method: &str, path: &str) -> Matched<'a> {
    match p.match_request(None, method, path) {
        MatchResult::Found(m) => m,
        other => panic!("{method} {path}: {other:?}"),
    }
}

#[test]
fn config_routes_match_with_params() {
    let cfg = format!(
        r#"{RUNTIMES}
[[route]]
path = "/health"
respond = {{ status = 200, json = {{ ok = true }} }}

[[route]]
path = "/users/{{id}}"
method = "GET"
handler = "src/users.ts#getUser"
params = {{ id = {{ type = "integer", min = 1 }} }}
"#
    );
    let p = project(&[("unsworn.toml", &cfg), ("src/users.ts", "")]);
    let m = found(&p, "GET", "/users/42");
    assert_eq!(m.params, vec![("id".to_string(), "42".to_string())]);
    assert_eq!(m.route.handler().unwrap().name.as_deref(), Some("getUser"));
    assert_eq!(m.route.source.to_string(), "unsworn.toml:14");
    assert!(m.route.validation.params.is_some());

    assert!(matches!(
        p.match_request(None, "POST", "/users/1"),
        MatchResult::MethodNotAllowed(m) if m == vec!["GET".to_string()]
    ));
    // HEAD falls back to GET.
    found(&p, "HEAD", "/users/1");
    assert!(matches!(
        p.match_request(None, "GET", "/nope"),
        MatchResult::NotFound
    ));
    assert_eq!(p.handlers_by_runtime()["ts"], vec!["src/users.ts#getUser"]);
}

#[test]
fn route_files_nested_and_flat_with_folder_policies() {
    let cfg = format!(
        r#"{RUNTIMES}
[routes]
dir = "routes"

[auth.jwt]
type = "jwt"
secret = "s"

[policies.cached]
cache = {{ ttl = "5m" }}
"#
    );
    let p = project(&[
        ("unsworn.toml", &cfg),
        ("routes/users.get.ts", ""),
        ("routes/users/[id]/put.go", ""),
        (
            "routes/admin/_policies.toml",
            "auth = \"jwt\"\nuse = [\"cached\"]\n",
        ),
        ("routes/admin/_before.ts", ""),
        ("routes/admin/stats.get.ts", ""),
        ("routes/lib/db.ts", ""),
    ]);
    assert_eq!(p.routes.len(), 3);
    let put = found(&p, "PUT", "/users/7");
    assert_eq!(put.route.handler().unwrap().runtime, "go");

    let stats = found(&p, "GET", "/admin/stats");
    let eff = p.effective(stats.route, "GET", "/admin/stats");
    assert!(matches!(eff.auth, AuthReq::AnyOf(_)));
    assert!(eff.cache.is_some());
    assert_eq!(eff.before.len(), 1);
    assert_eq!(eff.before[0].file, "routes/admin/_before.ts");

    let users = found(&p, "GET", "/users");
    let eff = p.effective(users.route, "GET", "/users");
    assert_eq!(eff.auth, AuthReq::Public);
}

#[test]
fn catch_all_takes_unmatched_methods_and_paths() {
    let cfg = format!(
        r#"{RUNTIMES}
[auth.jwt]
type = "jwt"
secret = "s"

[[route]]
path = "/{{*rest}}"
proxy = "http://127.0.0.1:3000"

[[route]]
path = "/api/{{*rest}}"
auth = "jwt"
rate_limit = {{ requests = 10, per = "1m" }}

[[route]]
path = "/api/users/{{id}}"
method = "GET"
handler = "src/users.ts#getUser"

[[route]]
path = "/api/public/status"
auth = "none"
"#
    );
    let p = project(&[("unsworn.toml", &cfg), ("src/users.ts", "")]);
    let m = found(&p, "GET", "/");
    assert_eq!(m.route.catch_all.as_deref(), Some("rest"));
    assert_eq!(m.params, vec![("rest".to_string(), String::new())]);

    let m = found(&p, "GET", "/api/users/1");
    assert!(m.route.handler().is_some());

    // POST isn't defined on /api/users/{id}, so the app behind the catch-all gets it.
    let m = found(&p, "POST", "/api/users/1");
    assert!(matches!(m.route.action, Action::Proxy(_)));
    assert_eq!(
        m.params,
        vec![("rest".to_string(), "api/users/1".to_string())]
    );
    let eff = p.effective(m.route, "POST", "/api/users/1");
    assert!(
        matches!(eff.auth, AuthReq::AnyOf(_)),
        "prefix entry applies inside the catch-all"
    );
    assert!(eff.rate_limit.is_some());

    let eff = p.effective(found(&p, "GET", "/about").route, "GET", "/about");
    assert_eq!(eff.auth, AuthReq::Public);

    // An exact entry inside the catch-all overrides the prefix entry.
    let m = found(&p, "GET", "/api/public/status");
    let eff = p.effective(m.route, "GET", "/api/public/status");
    assert_eq!(eff.auth, AuthReq::Public);
    assert!(eff.rate_limit.is_some());
}

#[test]
fn exact_entries_overlay_and_must_match() {
    let cfg = format!(
        r#"{RUNTIMES}
[routes]
dir = "routes"

[defaults]
timeout = "10s"

[[route]]
path = "/users/{{id}}"
method = "GET"
params = {{ id = {{ type = "integer" }} }}
timeout = "2s"
"#
    );
    let p = project(&[("unsworn.toml", &cfg), ("routes/users/[id]/get.ts", "")]);
    let m = found(&p, "GET", "/users/1");
    assert!(m.route.validation.params.is_some());
    let eff = p.effective(m.route, "GET", "/users/1");
    assert_eq!(eff.timeout, Duration::from_secs(2));
    assert_eq!(m.route.layers.len(), 2);

    let cfg = format!(
        r#"{RUNTIMES}
[[route]]
path = "/missing"
auth = "none"
"#
    );
    let errs = errors(&[("unsworn.toml", &cfg)]);
    assert!(errs[0].contains("no route matches"), "{errs:?}");
}

#[test]
fn openapi_source() {
    let cfg = format!(
        r#"{RUNTIMES}
[auth.jwt]
type = "jwt"
secret = "s"

[openapi]
spec = "openapi.yaml"
handler = "src/{{tag}}.ts#{{operationId}}"

[[route]]
path = "/health"
method = "GET"
respond = {{ status = 200 }}
"#
    );
    let spec = r#"
openapi: 3.1.0
info: { title: Test, version: "1" }
paths:
  /users/{id}:
    get:
      operationId: getUser
      tags: [users]
      security: [{ jwt: [] }]
      parameters:
        - { name: id, in: path, required: true, schema: { type: integer } }
  /health:
    get:
      responses: {}
"#;
    let p = project(&[
        ("unsworn.toml", &cfg),
        ("openapi.yaml", spec),
        ("src/users.ts", ""),
    ]);
    let m = found(&p, "GET", "/users/3");
    assert_eq!(m.route.handler().unwrap().raw, "src/users.ts#getUser");
    let eff = p.effective(m.route, "GET", "/users/3");
    assert!(matches!(eff.auth, AuthReq::AnyOf(_)));
    // The config route serves /health, so the operation needs no handler.
    let h = found(&p, "GET", "/health");
    assert!(matches!(h.route.action, Action::Respond { .. }));
    assert!(h.route.op.is_some());
}

#[test]
fn reports_useful_errors() {
    let cfg = format!(
        r#"{RUNTIMES}
[[route]]
path = "/a"
handler = "src/a.rb"

[[route]]
path = "/b"
handler = "src/missing.ts"

[[route]]
path = "/c"
respond = {{ status = 200 }}
proxy = "http://x"

[[route]]
path = "/d"
auth = "nope"
respond = {{ status = 200 }}

[[route]]
path = "/e"
use = ["nothing"]
respond = {{ status = 200 }}
"#
    );
    let errs = errors(&[("unsworn.toml", &cfg)]).join("\n");
    assert!(errs.contains("no runtime handles \".rb\""), "{errs}");
    assert!(
        errs.contains("handler file \"src/missing.ts\" not found"),
        "{errs}"
    );
    assert!(errs.contains("only one action"), "{errs}");
    assert!(errs.contains("unknown auth scheme \"nope\""), "{errs}");
    assert!(errs.contains("unknown policy set \"nothing\""), "{errs}");
}

#[test]
fn duplicate_actions_are_errors() {
    let cfg = format!(
        r#"{RUNTIMES}
[routes]
dir = "routes"

[[route]]
path = "/users"
method = "GET"
respond = {{ status = 200 }}
"#
    );
    let errs = errors(&[("unsworn.toml", &cfg), ("routes/users.get.ts", "")]);
    assert!(errs[0].contains("already defined"), "{errs:?}");
}

#[test]
fn env_substitution_and_protected_resource() {
    let cfg = r#"
[auth.oauth]
type = "jwt"
jwks_url = "{env.JWKS}"
audience = "https://api.example.com/mcp"

[auth.oauth.protected_resource]
resource = "https://api.example.com/mcp"
authorization_servers = ["https://auth.example.com"]

[[route]]
path = "/mcp"
proxy = "http://127.0.0.1:4000"
auth = { scheme = "oauth", scopes = ["tools:read"] }
"#;
    let mut host = MemHost::with(&[("unsworn.toml", cfg)]);
    let compiled = compile(&host, "unsworn.toml");
    assert!(compiled.diagnostics[0].message.contains("JWKS"));
    host.env
        .insert("JWKS".into(), "https://auth.example.com/jwks".into());
    let p = compile(&host, "unsworn.toml").project.unwrap();
    let docs = p.protected_resource_docs();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[1].0, "/.well-known/oauth-protected-resource/mcp");
}

#[test]
fn cache_with_auth_warns() {
    let cfg = r#"
[auth.jwt]
type = "jwt"
secret = "s"

[[route]]
path = "/me"
respond = { status = 200 }
auth = "jwt"
cache = { ttl = "1m" }
"#;
    let host = MemHost::with(&[("unsworn.toml", cfg)]);
    let compiled = compile(&host, "unsworn.toml");
    assert!(compiled.project.is_some());
    assert!(compiled.diagnostics[0].message.contains("won't be cached"));
}
