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
        MatchResult::MethodNotAllowed(m) if m == ["GET", "HEAD"]
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
url = "{env.JWKS}"
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
fn cors_any_origin_with_credentials_is_an_error() {
    let cfg = r#"
[[route]]
path = "/me"
respond = { status = 200 }
cors = { origins = "*", credentials = true }
"#;
    let errs = errors(&[("unsworn.toml", cfg)]).join("\n");
    assert!(
        errs.contains("can't be used with the origin \"*\""),
        "{errs}"
    );
    // A listed origin may send credentials, and `*` alone is fine.
    for cors in [
        r#"{ origins = ["https://app.example.com"], credentials = true }"#,
        r#"{ origins = "*" }"#,
    ] {
        let cfg =
            format!("[[route]]\npath = \"/me\"\nrespond = {{ status = 200 }}\ncors = {cors}\n");
        project(&[("unsworn.toml", &cfg)]);
    }
}

#[test]
fn trusting_every_proxy_warns() {
    for (list, warned) in [(r#"["all"]"#, true), (r#"["10.0.0.0/8"]"#, false)] {
        let cfg = format!(
            "[server]\ntrusted_proxies = {list}\n[[route]]\npath = \"/\"\nrespond = {{ status = 200 }}\n"
        );
        let host = MemHost::with(&[("unsworn.toml", &cfg)]);
        let compiled = compile(&host, "unsworn.toml");
        assert!(compiled.project.is_some());
        let found = compiled.diagnostics.iter().any(|d| {
            d.to_string()
                .contains("takes every client's `X-Forwarded-For`")
        });
        assert_eq!(found, warned, "{list}");
    }
}

#[test]
fn cache_with_auth_warns() {
    let cfg = r#"
[auth.jwt]
type = "jwt"
secret = "0123456789abcdef0123456789abcdef"

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

#[test]
fn cache_keyed_on_a_claim_api_keys_lack_warns() {
    let cfg = r#"
[auth.jwt]
type = "jwt"
secret = "0123456789abcdef0123456789abcdef"

[auth.keys]
type = "api_key"
keys = ["k1"]

[[route]]
path = "/me"
respond = { status = 200 }
auth = ["jwt", "keys"]
cache = { ttl = "1m", key = "auth.sub" }

[[route]]
path = "/jwt-only"
respond = { status = 200 }
auth = "jwt"
cache = { ttl = "1m", key = "auth.sub" }
"#;
    let host = MemHost::with(&[("unsworn.toml", cfg)]);
    let compiled = compile(&host, "unsworn.toml");
    assert!(compiled.project.is_some());
    let warnings: Vec<String> = compiled.diagnostics.iter().map(|d| d.to_string()).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("/me caches by `auth.sub`, which the API keys of `keys` don't have"),
        "{warnings:?}"
    );
}

#[test]
fn warns_about_policies_a_catch_all_app_can_sidestep() {
    let warnings = |routes: &str| -> Vec<String> {
        let cfg = format!(
            r#"
[auth.jwt]
type = "jwt"
secret = "0123456789abcdef0123456789abcdef"
{routes}"#
        );
        let host = MemHost::with(&[("unsworn.toml", &cfg)]);
        let compiled = compile(&host, "unsworn.toml");
        assert!(compiled.project.is_some(), "{:#?}", compiled.diagnostics);
        compiled
            .diagnostics
            .iter()
            .map(|d| format!("{}: {}", d.location, d.message))
            .collect()
    };
    // Auth on part of a public catch-all: the app behind it decides which
    // spellings of `/admin` it serves.
    let w = warnings(
        r#"
[[route]]
path = "/{*rest}"
proxy = "http://127.0.0.1:3000"

[[route]]
path = "/admin/{*rest}"
auth = "jwt"

[[route]]
path = "/internal"
ip = { allow = ["10.0.0.0/8"] }
"#,
    );
    assert_eq!(w.len(), 2, "{w:#?}");
    assert!(w[0].contains(".auth: `auth` on /admin/{*rest} only covers"));
    assert!(w[1].contains(".ip: `ip` on /internal only covers"));

    // The other way round is safe: a path that slips past the entry that
    // opens it only meets the stricter policy.
    let w = warnings(
        r#"
[[route]]
path = "/{*rest}"
proxy = "http://127.0.0.1:3000"
auth = "jwt"

[[route]]
path = "/public/{*rest}"
auth = "none"

[[route]]
path = "/admin/{*rest}"
rate_limit = { requests = 5, per = "1m" }
"#,
    );
    assert!(w.is_empty(), "{w:#?}");

    // So is auth in `[defaults]`, or on a prefix that covers the catch-all.
    let w = warnings(
        r#"
[defaults]
auth = "jwt"

[[route]]
path = "/api/{*rest}"
proxy = "http://127.0.0.1:3000"

[[route]]
path = "/api/admin/{*rest}"
auth = { scheme = "jwt", scopes = ["admin"] }
"#,
    );
    assert!(w.is_empty(), "{w:#?}");
}

#[test]
fn warns_about_rate_limit_keys_the_client_sets() {
    let cfg = r#"
[auth.key]
type = "api_key"
header = "X-Customer-Key"
keys = ["k1"]

[[route]]
path = "/a"
respond = { status = 200 }
rate_limit = { requests = 5, per = "1m", key = "header.x-tenant" }

[[route]]
path = "/b"
respond = { status = 200 }
auth = "key"
rate_limit = { requests = 5, per = "1m", key = "header.x-customer-key" }
"#;
    let host = MemHost::with(&[("unsworn.toml", cfg)]);
    let compiled = compile(&host, "unsworn.toml");
    let warnings: Vec<&str> = compiled
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:#?}");
    assert!(warnings[0].contains("* /a counts its rate limit by the `x-tenant` header"));
}

#[test]
fn binary_protocol_is_an_alias_for_framed() {
    let cfg = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"
protocol = "binary"
"#;
    let host = MemHost::with(&[("unsworn.toml", cfg)]);
    let compiled = compile(&host, "unsworn.toml");
    let warning = compiled
        .diagnostics
        .iter()
        .find(|d| d.level == crate::diag::Level::Warning && d.location == "[runtimes.ts].protocol")
        .expect("a warning for the old name");
    assert!(warning.message.contains("framed"));
    let project = compiled.project.expect("project compiles");
    assert_eq!(project.runtimes["ts"].protocol, Protocol::Framed);
}

#[test]
fn stream_body_rejects_body_validation() {
    let cfg = format!(
        r#"{RUNTIMES}
[[route]]
path = "/upload"
method = "POST"
handler = "src/upload.ts"
stream_body = true
body = {{ type = "object" }}
"#
    );
    let errs = errors(&[("unsworn.toml", &cfg), ("src/upload.ts", "")]);
    assert!(
        errs.iter()
            .any(|e| e.contains("`stream_body` and `body` validation")),
        "{errs:?}"
    );

    let cfg = format!(
        r#"{RUNTIMES}
[policies.uploads]
stream_body = true

[[route]]
path = "/upload"
method = "POST"
handler = "src/upload.ts"
use = ["uploads"]
"#
    );
    let p = project(&[("unsworn.toml", &cfg), ("src/upload.ts", "")]);
    assert!(p.streams_body(&p.routes[0]));
    assert_eq!(
        p.stream_handlers_by_runtime()["ts"],
        vec!["src/upload.ts".to_string()]
    );
}

#[test]
fn store_config_is_checked() {
    let errs = errors(&[("unsworn.toml", "[store]\nbackend = \"redis\"\n")]);
    assert!(errs.iter().any(|e| e.contains("needs a `url`")), "{errs:?}");
    let errs = errors(&[(
        "unsworn.toml",
        "[store]\nbackend = \"redis\"\nurl = \"memcached://cache:11211\"\n",
    )]);
    assert!(errs.iter().any(|e| e.contains("redis://")), "{errs:?}");

    // A REST API (how functions reach Redis) needs its token.
    let errs = errors(&[(
        "unsworn.toml",
        "[store]\nbackend = \"redis\"\nurl = \"https://x.upstash.io\"\n",
    )]);
    assert!(
        errs.iter().any(|e| e.contains("needs a `token`")),
        "{errs:?}"
    );
    let p = project(&[(
        "unsworn.toml",
        "[store]\nbackend = \"redis\"\nurl = \"https://x.upstash.io\"\ntoken = \"t\"\n",
    )]);
    assert!(p.store.is_rest());
    assert_eq!(p.store.token.as_deref(), Some("t"));

    let p = project(&[(
        "unsworn.toml",
        "[store]\nbackend = \"redis\"\nurl = \"rediss://cache:6380\"\non_error = \"closed\"\n",
    )]);
    assert_eq!(p.store.backend, crate::config::StoreBackend::Redis);
    assert_eq!(p.store.on_error, crate::config::OnError::Closed);
    assert_eq!(p.store.prefix, "");
    assert!(!p.store.is_rest());
}

#[test]
fn auth_url_sources() {
    // `url` means a JWKS for `jwt`, and a verifier for `api_key`.
    let cfg = r#"
[auth.users]
type = "jwt"
url = "https://auth.example.com/.well-known/jwks.json"
audience = "api"

[auth.customers]
type = "api_key"
header = "X-Customer-Key"
url = "{env.VERIFY_URL}"
timeout = "500ms"
cache = "30s"
headers = { Authorization = "Bearer {env.AUTH_TOKEN}", "X-Service" = "unsworn" }
"#;
    let mut host = MemHost::with(&[("unsworn.toml", cfg)]);
    host.env.insert(
        "VERIFY_URL".into(),
        "https://auth.example.com/verify-key".into(),
    );
    host.env.insert("AUTH_TOKEN".into(), "t0k".into());
    let compiled = compile(&host, "unsworn.toml");
    assert!(
        compiled.diagnostics.is_empty(),
        "{:?}",
        compiled.diagnostics
    );
    let p = compiled.project.unwrap();
    let Scheme::Jwt(jwt) = p.schemes["users"].as_ref() else {
        panic!("users isn't a jwt scheme")
    };
    assert!(
        matches!(&jwt.keys, JwtKeys::Jwks(u) if u == "https://auth.example.com/.well-known/jwks.json")
    );
    let Scheme::ApiKey(key) = p.schemes["customers"].as_ref() else {
        panic!("customers isn't an api_key scheme")
    };
    assert_eq!(key.header.as_deref(), Some("x-customer-key"));
    let v = key.verifier().unwrap();
    assert_eq!(v.url, "https://auth.example.com/verify-key");
    assert_eq!(v.timeout, Duration::from_millis(500));
    assert_eq!(v.cache, Some(Duration::from_secs(30)));
    assert_eq!(
        v.headers,
        vec![
            ("authorization".to_string(), "Bearer t0k".to_string()),
            ("x-service".to_string(), "unsworn".to_string()),
        ]
    );

    // The same config in YAML, with the documented indented mapping.
    let yaml = r#"
auth:
  customers:
    type: api_key
    url: https://auth.example.com/verify-key
    headers:
      Authorization: Bearer t0k
"#;
    let host = MemHost::with(&[("unsworn.yaml", yaml)]);
    let compiled = compile(&host, "unsworn.yaml");
    assert!(
        compiled.diagnostics.is_empty(),
        "{:?}",
        compiled.diagnostics
    );
    let p = compiled.project.unwrap();
    let Scheme::ApiKey(key) = p.schemes["customers"].as_ref() else {
        panic!("customers isn't an api_key scheme")
    };
    assert_eq!(key.header.as_deref(), Some("x-api-key"));
    assert_eq!(key.verifier().unwrap().timeout, Duration::from_secs(2));
    assert_eq!(key.verifier().unwrap().cache, None);
}

#[test]
fn auth_url_errors() {
    let cases = [
        (
            "type = \"api_key\"\nkeys = \"k\"\nurl = \"https://a.example/v\"",
            "set exactly one of `keys` or `url`",
        ),
        ("type = \"api_key\"", "set exactly one of `keys` or `url`"),
        (
            "type = \"api_key\"\nkeys = \"k\"\nheaders = { A = \"b\" }",
            "`headers` only applies with `url`",
        ),
        (
            "type = \"api_key\"\nkeys = \"k\"\ntimeout = \"1s\"",
            "`timeout` only applies with `url`",
        ),
        (
            "type = \"api_key\"\nkeys = \"k\"\ncache = \"1m\"",
            "`cache` only applies with `url`",
        ),
        (
            "type = \"api_key\"\nurl = \"https://a.example/v\"\ncache = \"0s\"",
            "cache must be longer than 0",
        ),
        (
            "type = \"api_key\"\nurl = \"auth.example.com/verify\"",
            "isn't an http:// or https:// URL",
        ),
        (
            "type = \"api_key\"\nurl = \"https://a.example/v\"\nheaders = { \"Bad Name\" = \"x\" }",
            "not a valid header name",
        ),
        (
            "type = \"jwt\"\nurl = \"https://a.example/jwks\"\nsecret = \"s\"",
            "set exactly one of `url`, `secret` or `public_key`",
        ),
        (
            "type = \"jwt\"\nurl = \"https://a.example/jwks\"\njwks_url = \"https://a.example/jwks\"",
            "set `url` or `jwks_url`, not both",
        ),
    ];
    for (body, want) in cases {
        let cfg = format!("[auth.a]\n{body}\n");
        let errs = errors(&[("unsworn.toml", &cfg)]);
        assert!(errs.iter().any(|e| e.contains(want)), "{body}: {errs:?}");
    }
}

#[test]
fn warns_about_weak_jwt_schemes() {
    let diagnostics = |scheme: &str, env: &[(&str, &str)]| -> Vec<String> {
        let cfg = format!("[auth.users]\ntype = \"jwt\"\n{scheme}\n");
        let mut host = MemHost::with(&[("unsworn.toml", &cfg)]);
        for (k, v) in env {
            host.env.insert(k.to_string(), v.to_string());
        }
        compile(&host, "unsworn.toml")
            .diagnostics
            .iter()
            .map(|d| {
                let level = if d.is_error() { "error" } else { "warning" };
                format!("{level} {}: {}", d.location, d.message)
            })
            .collect()
    };
    let one = |scheme: &str, env: &[(&str, &str)], want: &str| {
        let got = diagnostics(scheme, env);
        assert!(
            got.len() == 1 && got[0].contains(want),
            "{scheme}: {got:#?}"
        );
    };
    // A variable that's set, but to nothing, would let anyone sign tokens.
    one(
        "secret = \"{env.JWT_SECRET}\"",
        &[("JWT_SECRET", "")],
        "error [auth.users].secret: the secret is empty",
    );
    one("secret = \"{env.JWT_SECRET}\"", &[], "JWT_SECRET isn't set");
    one(
        "secret = \"hunter2\"",
        &[],
        "warning [auth.users].secret: the secret is shorter than 32 bytes",
    );
    assert!(diagnostics("secret = \"0123456789abcdef0123456789abcdef\"", &[]).is_empty());

    // A shared key set signs tokens for other people's projects too.
    one(
        "url = \"https://keys.example/jwks\"",
        &[],
        "warning [auth.users]: without `issuer` or `audience`",
    );
    // Keys fetched in the clear can be swapped on the way.
    one(
        "url = \"http://keys.example/jwks\"\naudience = \"api\"",
        &[],
        "warning [auth.users].url: \"http://keys.example/jwks\" isn't encrypted",
    );
    for local in [
        "http://localhost:8081/jwks",
        "http://127.0.0.1/jwks",
        "http://[::1]:9/jwks",
    ] {
        let scheme = format!("url = \"{local}\"\naudience = \"api\"");
        assert!(diagnostics(&scheme, &[]).is_empty(), "{local}");
    }
    let cfg = "[auth.keys]\ntype = \"api_key\"\nurl = \"http://verify.internal/v\"\n";
    let host = MemHost::with(&[("unsworn.toml", cfg)]);
    let compiled = compile(&host, "unsworn.toml");
    assert!(
        compiled.diagnostics[0]
            .message
            .contains("every API key sent to it")
    );
}

#[test]
fn jwks_url_is_an_alias_for_url() {
    let cfg = r#"
[auth.users]
type = "jwt"
jwks_url = "https://auth.example.com/jwks"
issuer = "https://auth.example.com/"
"#;
    let host = MemHost::with(&[("unsworn.toml", cfg)]);
    let compiled = compile(&host, "unsworn.toml");
    assert_eq!(compiled.diagnostics.len(), 1);
    assert!(!compiled.diagnostics[0].is_error());
    assert!(
        compiled.diagnostics[0]
            .message
            .contains("old name for `url`")
    );
    let p = compiled.project.unwrap();
    let Scheme::Jwt(jwt) = p.schemes["users"].as_ref() else {
        panic!("users isn't a jwt scheme")
    };
    assert!(matches!(&jwt.keys, JwtKeys::Jwks(u) if u == "https://auth.example.com/jwks"));
}

const MCP_AUTH: &str = r#"
[auth.oauth]
type = "jwt"
secret = "shh"
audience = "https://api.example.com/mcp"
"#;

#[test]
fn mcp_tools_compile_to_hidden_routes() {
    let cfg = format!(
        r#"{RUNTIMES}{MCP_AUTH}
[defaults]
timeout = "10s"

[[route]]
path = "/{{*rest}}"
rate_limit = {{ requests = 100, per = "1m" }}

[mcp]
name = "acme-tools"
auth = {{ scheme = "oauth", scopes = ["tools:read"] }}
origin = {{ allow = ["https://claude.ai"] }}

[[mcp.tool]]
name = "get_forecast"
description = "Weather forecast for a city"
input = {{ type = "object", properties = {{ city = {{ type = "string", x-mcp-header = "City" }} }}, required = ["city"] }}
handler = "tools/weather.ts#getForecast"
annotations = {{ read_only = true }}

[[mcp.tool]]
name = "create_invoice"
description = "Create a draft invoice"
handler = "src/billing.go"
auth = {{ scheme = "oauth", scopes = ["billing:write"] }}
timeout = "20s"
"#
    );
    let p = project(&[
        ("unsworn.toml", &cfg),
        ("tools/weather.ts", ""),
        ("src/billing.go", ""),
    ]);
    let mcp = p.mcp.as_ref().unwrap();
    assert_eq!(mcp.path, "/mcp");
    assert_eq!(mcp.server.name, "acme-tools");
    assert!(mcp.hosted());

    // The endpoint only takes POST; GET and DELETE get 405.
    let m = found(&p, "POST", "/mcp");
    assert!(matches!(m.route.action, Action::Mcp));
    assert_eq!(m.route.id, mcp.route);
    for method in ["GET", "DELETE"] {
        match p.match_request(None, method, "/mcp") {
            MatchResult::MethodNotAllowed(allowed) => assert_eq!(allowed, vec!["POST"]),
            other => panic!("{method} /mcp: {other:?}"),
        }
    }

    // Tools are sorted by name and never matched by path.
    let names: Vec<&str> = mcp.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["create_invoice", "get_forecast"]);
    assert!(matches!(
        p.match_request(None, "POST", "mcp:get_forecast"),
        MatchResult::NotFound
    ));

    let forecast = mcp.tool("get_forecast").unwrap();
    let route = &p.routes[forecast.route];
    assert_eq!(route.mcp_tool(), Some("get_forecast"));
    assert_eq!(route.handler().unwrap().runtime, "ts");
    assert!(route.validation.body.is_some());
    assert_eq!(forecast.param_headers[0].header, "mcp-param-city");
    assert_eq!(forecast.definition()["annotations"]["readOnlyHint"], true);
    assert!(!p.streams_body(route));
    assert_eq!(
        p.handlers_by_runtime()["go"],
        vec!["src/billing.go".to_string()]
    );

    // Tool policies: [defaults], prefix entries for /mcp, [mcp], the tool.
    let invoice = mcp.tool("create_invoice").unwrap();
    let eff = p.tool_effective(invoice);
    assert_eq!(eff.timeout, Duration::from_secs(20));
    assert!(
        eff.rate_limit.is_some(),
        "the /{{*rest}} prefix entry applies"
    );
    assert_eq!(
        eff.origin.as_deref(),
        Some(&["https://claude.ai".to_string()][..])
    );
    let AuthReq::AnyOf(alts) = &eff.auth else {
        panic!("auth")
    };
    assert_eq!(alts[0].scopes, vec!["billing:write"]);
    // `own` holds only what the tool sets, so calls know what to check again.
    assert!(invoice.own.auth.is_some() && invoice.own.origin.is_none());
    assert!(forecast.own.auth.is_none());
    assert_eq!(p.tool_effective(forecast).timeout, Duration::from_secs(10));

    // Tools aren't in the OpenAPI export.
    let doc = crate::export::openapi(&p);
    assert!(doc["paths"].as_object().unwrap().is_empty());
}

#[test]
fn mcp_defaults_and_line_numbers() {
    let cfg = format!(
        r#"{RUNTIMES}
[mcp]

[[mcp.tool]]
name = "ping_db"
description = "Check the database"
handler = "tools/db.ts"
"#
    );
    let p = project(&[("unsworn.toml", &cfg), ("tools/db.ts", "")]);
    let mcp = p.mcp.as_ref().unwrap();
    assert_eq!(mcp.server.name, "unsworn");
    assert_eq!(mcp.server.version, crate::VERSION);
    assert_eq!(mcp.list_ttl, Duration::from_secs(300));
    assert_eq!(mcp.list_tools, config::ListTools::Permitted);
    let tool = &mcp.tools[0];
    assert_eq!(
        tool.input_schema,
        serde_json::json!({"type": "object", "additionalProperties": false})
    );
    assert_eq!(p.routes[mcp.route].source.to_string(), "unsworn.toml:10");
    assert_eq!(p.routes[tool.route].source.to_string(), "unsworn.toml:12");
}

#[test]
fn proxied_mcp_tools_only_add_policies() {
    let cfg = format!(
        r#"{MCP_AUTH}
[mcp]
proxy = "http://127.0.0.1:4000"
auth = {{ scheme = "oauth", scopes = ["tools:read"] }}

[[mcp.tool]]
name = "create_invoice"
auth = {{ scheme = "oauth", scopes = ["billing:write"] }}
rate_limit = {{ requests = 10, per = "1m", key = "auth.sub" }}
"#
    );
    let p = project(&[("unsworn.toml", &cfg)]);
    let mcp = p.mcp.as_ref().unwrap();
    assert!(!mcp.hosted());
    let route = &p.routes[mcp.tools[0].route];
    assert!(
        matches!(&route.action, Action::Proxy(px) if px.upstreams == ["http://127.0.0.1:4000"])
    );
    assert!(
        route.validation.body.is_none(),
        "without `input`, the server behind the proxy checks arguments"
    );
}

#[test]
fn mcp_config_errors() {
    let cases: &[(&str, &str)] = &[
        (
            "[[mcp.tool]]\nname = \"get forecast\"\nhandler = \"t.ts\"\n",
            "can only use letters",
        ),
        (
            "[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\n[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\n",
            "already defined",
        ),
        ("[[mcp.tool]]\nname = \"a\"\n", "has no handler"),
        (
            "[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\ninput = { type = \"string\" }\n",
            "\"type\": \"object\"",
        ),
        (
            "[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\ninput = { type = \"object\", properties = { n = { type = \"number\", x-mcp-header = \"N\" } } }\n",
            "x-mcp-header",
        ),
        (
            "[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\nauth = \"none\"\n",
            "can't loosen",
        ),
        (
            "[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\nrate_limit = \"off\"\n",
            "can't switch off",
        ),
        (
            "[policies.web]\ncors = { origins = \"*\" }\n[mcp]\n[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\nuse = [\"web\"]\n",
            "`cors` can't be set on a tool",
        ),
        (
            "[policies.c]\ncache = { ttl = \"1m\" }\n[mcp]\nuse = [\"c\"]\n",
            "`cache` doesn't apply",
        ),
        (
            "[mcp]\nproxy = \"http://127.0.0.1:4000\"\n[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\n",
            "doesn't apply to a tool of a proxied",
        ),
        ("[mcp]\npath = \"/mcp/{id}\"\n", "can't have parameters"),
        (
            "[mcp]\n[[route]]\npath = \"/mcp\"\nmethod = \"POST\"\nproxy = \"http://x:1\"\n",
            "already defined",
        ),
        ("[mcp]\nlist_tools = \"some\"\n", "unknown variant"),
    ];
    for (body, want) in cases {
        let cfg = format!("{RUNTIMES}{MCP_AUTH}\n{body}");
        let errs = errors(&[("unsworn.toml", &cfg), ("t.ts", "")]);
        assert!(errs.iter().any(|e| e.contains(want)), "{body}\n→ {errs:?}");
    }
}

#[test]
fn mcp_warnings() {
    let cfg = format!("{RUNTIMES}\n[mcp]\n\n[[mcp.tool]]\nname = \"a\"\nhandler = \"t.ts\"\n");
    let host = MemHost::with(&[("unsworn.toml", &cfg), ("t.ts", "")]);
    let compiled = compile(&host, "unsworn.toml");
    assert!(compiled.project.is_some());
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.message.contains("has no description"))
    );

    let empty = compile(
        &MemHost::with(&[("unsworn.toml", "[mcp]\n")]),
        "unsworn.toml",
    );
    assert!(
        empty
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no tools"))
    );
}

#[test]
fn routes_as_tools() {
    let cfg = format!(
        r#"{RUNTIMES}{MCP_AUTH}
[routes]
dir = "routes"

[mcp]
auth = {{ scheme = "oauth", scopes = ["tools:read"] }}
timeout = "45s"
before = ["src/audit.ts"]

[[route]]
path = "/users/{{id}}"
method = "GET"
handler = "src/users.ts#getUser"
params = {{ id = {{ type = "integer", min = 1 }} }}
query = {{ verbose = {{ type = "boolean" }}, limit = {{ type = "integer", default = 20 }} }}
auth = {{ scheme = "oauth", scopes = ["users:read"] }}
rate_limit = {{ requests = 5, per = "1m" }}
before = ["src/tenant.ts"]
mcp = {{ tool = "get_user", description = "Look up a user", annotations = {{ read_only = true }} }}

[[route]]
path = "/users"
method = "POST"
handler = "src/users.ts#createUser"
body = {{ type = "object", required = ["name"], properties = {{ name = {{ type = "string" }}, email = {{ type = "string" }} }} }}
timeout = "5s"
mcp = {{ tool = "create_user", description = "Create a user" }}

[[route]]
path = "/users/{{id}}"
method = "PUT"
handler = "src/users.ts#replaceUser"
body = {{ type = "object", properties = {{ id = {{ type = "integer" }} }} }}
mcp = {{ tool = "replace_user", description = "Replace a user" }}

# A route from a route file, offered as a tool by a policy-only entry.
[[route]]
path = "/health"
method = "GET"
mcp = {{ tool = "health", description = "Check health" }}
"#
    );
    let p = project(&[
        ("unsworn.toml", &cfg),
        ("src/users.ts", ""),
        ("src/audit.ts", ""),
        ("src/tenant.ts", ""),
        ("routes/health.get.ts", ""),
    ]);
    let mcp = p.mcp.as_ref().unwrap();
    let names: Vec<&str> = mcp.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["create_user", "get_user", "health", "replace_user"]);

    // Path and query parameters are top-level arguments, typed as declared.
    let get = mcp.tool("get_user").unwrap();
    let route = &p.routes[get.route];
    assert_eq!(
        (route.path.as_str(), route.mcp_tool()),
        ("/users/{id}", None),
        "the REST route itself"
    );
    assert_eq!(p.route_tool(route.id).unwrap().name, "get_user");
    assert_eq!(
        get.input_schema,
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "default": 20},
                "verbose": {"type": "boolean"}
            },
            "required": ["id"],
            "additionalProperties": false
        })
    );
    assert_eq!(get.definition()["annotations"]["readOnlyHint"], true);
    let b = get.binding.as_ref().unwrap();
    assert_eq!((b.method.as_str(), b.body), ("GET", BodyArgs::None));
    // The route's auth and rate limit are checked on each call, and counted
    // with REST requests (the same route id).
    assert!(get.own.auth.is_some() && get.own.rate_limit.is_some());
    // [mcp]'s before handlers run first, then the route's; [mcp]'s timeout
    // applies when the route sets none.
    let eff = p.tool_effective(get);
    let befores: Vec<&str> = eff.before.iter().map(|h| h.raw.as_str()).collect();
    assert_eq!(befores, ["src/audit.ts", "src/tenant.ts"]);
    assert_eq!(eff.timeout, Duration::from_secs(45));
    assert!(
        get.input_rule
            .as_ref()
            .unwrap()
            .check(&serde_json::json!({"id": 0}), "a")
            .is_err()
    );

    // A body's fields sit next to the parameters when nothing collides.
    let create = mcp.tool("create_user").unwrap();
    assert_eq!(create.binding.as_ref().unwrap().body, BodyArgs::Flat);
    assert_eq!(create.input_schema["required"], serde_json::json!(["name"]));
    assert!(create.input_schema["properties"]["email"].is_object());
    assert!(create.input_schema.get("additionalProperties").is_none());
    assert_eq!(
        p.tool_effective(create).timeout,
        Duration::from_secs(5),
        "the route's own timeout wins"
    );

    // ...and go under `body` when a field shares a parameter's name.
    let replace = mcp.tool("replace_user").unwrap();
    assert_eq!(replace.binding.as_ref().unwrap().body, BodyArgs::Nested);
    assert!(replace.input_schema["properties"]["body"]["properties"]["id"].is_object());

    let health = mcp.tool("health").unwrap();
    assert_eq!(
        p.routes[health.route].source.to_string(),
        "routes/health.get.ts"
    );
    assert_eq!(health.input_schema["properties"], serde_json::json!({}));
}

#[test]
fn route_tool_errors_and_warnings() {
    let tool = |extra: &str| {
        format!("handler = \"src/a.ts\"\nmcp = {{ tool = \"t\", description = \"T\" }}\n{extra}")
    };
    let cases: Vec<(String, &str)> = vec![
        (format!("[[route]]\npath = \"/a\"\nmethod = \"GET\"\n{}", tool("")), "needs an [mcp] table"),
        (
            format!("[mcp]\nproxy = \"http://x:1\"\n[[route]]\npath = \"/a\"\nmethod = \"GET\"\n{}", tool("")),
            "proxied MCP server",
        ),
        (
            "[mcp]\n[[route]]\npath = \"/a\"\nrespond = { status = 200 }\nmcp = { tool = \"t\" }\n".into(),
            "no handler",
        ),
        (format!("[mcp]\n[[route]]\npath = \"/a\"\n{}", tool("")), "several methods"),
        (
            format!("[mcp]\n[[route]]\npath = \"/a\"\nmethod = \"GET\"\n{}", tool("stream_body = true")),
            "stream_body",
        ),
        (
            format!("[mcp]\n[[route]]\npath = \"/a\"\nmethod = \"GET\"\n{}", tool("headers = { \"x-tenant\" = { required = true } }")),
            "x-tenant",
        ),
        (
            format!(
                "[auth.key]\ntype = \"api_key\"\nkeys = \"k\"\n[mcp]\nauth = \"oauth\"\n[[route]]\npath = \"/a\"\nmethod = \"GET\"\n{}",
                tool("auth = \"key\"")
            ),
            "shares no scheme",
        ),
        (
            format!(
                "[mcp]\n[[mcp.tool]]\nname = \"t\"\nhandler = \"src/a.ts\"\n[[route]]\npath = \"/a\"\nmethod = \"GET\"\n{}",
                tool("")
            ),
            "already defined",
        ),
        (
            "[mcp]\n[[route]]\npath = \"/a\"\nmethod = \"GET\"\nhandler = \"src/a.ts\"\nmcp = { tool = \"bad name\" }\n".into(),
            "can only use letters",
        ),
        (
            "[mcp]\n[[route]]\npath = \"/a/{*rest}\"\nauth = \"none\"\nmcp = { tool = \"t\" }\n".into(),
            "prefix entry can't be an MCP tool",
        ),
        (
            format!("[mcp]\n[[route]]\npath = \"/a\"\nmethod = [\"GET\", \"POST\"]\n{}", tool("").replace("}\n", ", method = \"PUT\" }\n")),
            "doesn't accept PUT",
        ),
    ];
    for (body, want) in cases {
        let cfg = format!("{RUNTIMES}{MCP_AUTH}\n{body}");
        let errs = errors(&[("unsworn.toml", &cfg), ("src/a.ts", "")]);
        assert!(errs.iter().any(|e| e.contains(want)), "{body}\n→ {errs:?}");
    }

    // A POST route without a body schema still works, with a warning.
    let cfg = format!(
        "{RUNTIMES}\n[mcp]\n[[route]]\npath = \"/a\"\nmethod = \"POST\"\nhandler = \"src/a.ts\"\nmcp = {{ tool = \"t\" }}\n"
    );
    let compiled = compile(
        &MemHost::with(&[("unsworn.toml", &cfg), ("src/a.ts", "")]),
        "unsworn.toml",
    );
    assert!(compiled.project.is_some());
    let warnings: Vec<String> = compiled
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect();
    assert!(
        warnings.iter().any(|m| m.contains("no `body` schema")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|m| m.contains("no description")),
        "{warnings:?}"
    );
}

const TOOLS_SPEC: &str = r#"
openapi: 3.1.0
info: { title: Test, version: "1" }
paths:
  /users/{id}:
    get:
      operationId: getUser
      tags: [users]
      summary: Get a user
      description: Look up one user by id.
      parameters:
        - { name: id, in: path, required: true, schema: { type: integer } }
  /users:
    get:
      operationId: listUsers
      tags: [users]
      summary: List users
    post:
      tags: [users]
      x-unsworn-handler: src/users.ts#create
  /admin/stats:
    get:
      operationId: stats
      tags: [admin]
      summary: Usage stats
  /health:
    get:
      operationId: health
      tags: [users]
"#;

fn openapi_tools(mcp_tools: &str, extra: &str) -> Compiled {
    let cfg = format!(
        r#"{RUNTIMES}
[mcp]

[openapi]
spec = "openapi.yaml"
handler = "src/{{tag}}.ts#{{operationId}}"
mcp_tools = {mcp_tools}

[[route]]
path = "/health"
method = "GET"
respond = {{ status = 200 }}
{extra}"#
    );
    let host = MemHost::with(&[
        ("unsworn.toml", &cfg),
        ("openapi.yaml", TOOLS_SPEC),
        ("src/users.ts", ""),
        ("src/admin.ts", ""),
    ]);
    compile(&host, "unsworn.toml")
}

#[test]
fn openapi_operations_as_tools() {
    // An explicit `mcp` on a policy-only entry renames a bulk pick.
    let compiled = openapi_tools(
        r#"{ tags = ["users"] }"#,
        "[[route]]\npath = \"/users\"\nmethod = \"GET\"\nmcp = { tool = \"list_all_users\", description = \"Every user\" }\n",
    );
    let messages: Vec<String> = compiled.diagnostics.iter().map(|d| d.to_string()).collect();
    let p = compiled.project.unwrap_or_else(|| panic!("{messages:?}"));
    let mcp = p.mcp.as_ref().unwrap();
    let names: Vec<&str> = mcp.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["getUser", "list_all_users"], "only the `users` tag");

    let get = mcp.tool("getUser").unwrap();
    assert_eq!(get.description.as_deref(), Some("Look up one user by id."));
    assert_eq!(get.input_schema["properties"]["id"]["type"], "integer");
    assert_eq!(
        p.routes[get.route].handler().unwrap().raw,
        "src/users.ts#getUser"
    );

    // Operations that can't be tools are skipped with a warning.
    assert!(
        messages.iter().any(|m| m.contains("no operationId")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("/health") && m.contains("isn't offered as an MCP tool")),
        "{messages:?}"
    );
    assert!(compiled.diagnostics.iter().all(|d| !d.is_error()));

    let all = openapi_tools(r#""all""#, "").project.unwrap();
    let names: Vec<&str> = all
        .mcp
        .as_ref()
        .unwrap()
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, ["getUser", "listUsers", "stats"]);
    let stats = all.mcp.as_ref().unwrap().tool("stats").unwrap();
    assert_eq!(
        stats.description.as_deref(),
        Some("Usage stats"),
        "the summary when there's no description"
    );
}

#[test]
fn openapi_tool_errors() {
    let errs = |compiled: Compiled| -> Vec<String> {
        assert!(compiled.project.is_none(), "expected errors");
        compiled
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.to_string())
            .collect()
    };
    let e = errs(openapi_tools(r#""some""#, ""));
    assert!(
        e.iter()
            .any(|m| m.contains("\"some\" isn't a valid choice")),
        "{e:?}"
    );

    let bad_name = TOOLS_SPEC.replace("operationId: stats", "operationId: usage stats");
    let cfg = format!(
        "{RUNTIMES}\n[mcp]\n[openapi]\nspec = \"openapi.yaml\"\nhandler = \"src/{{tag}}.ts#x\"\nmcp_tools = \"all\"\n"
    );
    let host = MemHost::with(&[
        ("unsworn.toml", &cfg),
        ("openapi.yaml", &bad_name),
        ("src/users.ts", ""),
        ("src/admin.ts", ""),
    ]);
    let e = errs(compile(&host, "unsworn.toml"));
    assert!(
        e.iter().any(|m| m.contains("can only use letters")),
        "an invalid name stays an error: {e:?}"
    );

    for (mcp, want) in [
        ("", "needs an [mcp] table"),
        ("[mcp]\nproxy = \"http://x:1\"\n", "proxied MCP server"),
    ] {
        let cfg = format!(
            "{RUNTIMES}\n{mcp}[openapi]\nspec = \"openapi.yaml\"\nhandler = \"src/{{tag}}.ts#{{operationId}}\"\nmcp_tools = \"all\"\n"
        );
        let host = MemHost::with(&[
            ("unsworn.toml", &cfg),
            ("openapi.yaml", TOOLS_SPEC),
            ("src/users.ts", ""),
            ("src/admin.ts", ""),
        ]);
        let e = errs(compile(&host, "unsworn.toml"));
        assert!(e.iter().any(|m| m.contains(want)), "{e:?}");
        assert_eq!(
            e.iter().filter(|m| m.contains("mcp")).count(),
            1,
            "reported once: {e:?}"
        );
    }
}

#[test]
fn an_outbound_service_allows_only_its_methods_and_paths() {
    let cfg = r#"
[outbound.billing]
url = "https://billing.example/api/"
headers = { Authorization = "Bearer k" }
methods = ["get", "POST"]
paths = ["/charges", "/charges/{id}", "/public/{*rest}"]
"#;
    let p = project(&[("unsworn.toml", cfg)]);
    let s = &p.outbound["billing"];
    assert_eq!(s.url, "https://billing.example/api");
    assert_eq!(
        s.headers,
        [("authorization".to_string(), "Bearer k".to_string())]
    );
    assert!(s.allows("GET", "/charges").is_ok());
    assert!(s.allows("POST", "/charges/42?expand=x").is_ok());
    assert!(s.allows("GET", "/public/a/b/c").is_ok());
    assert!(
        s.allows("DELETE", "/charges")
            .unwrap_err()
            .contains("doesn't allow DELETE")
    );
    assert!(
        s.allows("GET", "/refunds")
            .unwrap_err()
            .contains("doesn't allow the path")
    );
    assert!(s.allows("GET", "/charges/42/refund").is_err());
    // A path can't step out of an allowed one, or leave the service.
    for path in [
        "/public/../refunds",
        "/public/%2e%2e/refunds",
        "/public/%2E./refunds",
        "/public/./x",
        "/public/a\\b",
        "/public/x#y",
        "charges",
        "",
    ] {
        assert!(s.allows("GET", path).is_err(), "{path}");
    }
    // Dots inside a name are fine.
    assert!(s.allows("GET", "/public/v1.2/file..txt").is_ok());

    // No lists: anything, but still nothing with a `..` in it.
    let p = project(&[(
        "unsworn.toml",
        "[outbound.open]\nurl = \"http://127.0.0.1:9\"\n",
    )]);
    let s = &p.outbound["open"];
    assert!(s.allows("PATCH", "/anything/at/all").is_ok());
    assert!(s.allows("GET", "/a/../b").is_err());
}

#[test]
fn outbound_services_are_checked() {
    let e = errors(&[(
        "unsworn.toml",
        "[outbound.a]\nurl = \"ftp://x\"\n\n[outbound.b]\nurl = \"https://x/y?z=1\"\nheaders = { \"bad name\" = \"v\" }\npaths = [\"nope\"]\n",
    )]);
    assert!(
        e.iter()
            .any(|m| m.contains("[outbound.a].url") && m.contains("isn't an http")),
        "{e:#?}"
    );
    assert!(e.iter().any(|m| m.contains("can't have a query")), "{e:#?}");
    assert!(
        e.iter().any(|m| m.contains("not a valid header name")),
        "{e:#?}"
    );
    assert!(
        e.iter().any(|m| m.contains("must start with `/`")),
        "{e:#?}"
    );
}

#[test]
fn an_outbound_services_header_variables_are_kept_from_workers() {
    let mut host = MemHost::with(&[(
        "unsworn.toml",
        "[outbound.s]\nurl = \"{env.S_URL}\"\nheaders = { Authorization = \"Bearer {env.S_KEY}\" }\n",
    )]);
    host.env.insert("S_URL".into(), "https://s.example".into());
    host.env.insert("S_KEY".into(), "k".into());
    let p = compile(&host, "unsworn.toml").project.unwrap();
    assert_eq!(p.secret_env, ["S_KEY"]);
    assert_eq!(p.outbound["s"].url, "https://s.example");
}

#[test]
fn a_shadow_can_be_set_on_the_route_or_on_an_entry_for_it() {
    let cfg = format!(
        r#"{RUNTIMES}
[routes]
dir = "routes"

[[route]]
path = "/search"
method = "GET"
handler = "src/search.ts#search"
shadow = {{ handler = "src/search.go#Search", sample = 0.2 }}

# For a route that comes from a file.
[[route]]
path = "/users/{{id}}"
method = "GET"
shadow = "src/users.go#Get"
"#
    );
    let p = project(&[
        ("unsworn.toml", &cfg),
        ("src/search.ts", ""),
        ("src/search.go", ""),
        ("src/users.go", ""),
        ("routes/users.[id].get.ts", ""),
    ]);
    let search = found(&p, "GET", "/search").route.shadow.clone().unwrap();
    assert_eq!(
        (search.handler.raw.as_str(), search.handler.runtime.as_str()),
        ("src/search.go#Search", "go")
    );
    assert_eq!(search.every, 5);
    // One request in five is taken.
    let taken = (0..20).filter(|_| search.take()).count();
    assert_eq!(taken, 4);
    let users = found(&p, "GET", "/users/7").route.shadow.clone().unwrap();
    assert_eq!(users.handler.raw, "src/users.go#Get");
    assert_eq!(users.every, 1);
    // Workers are asked whether the shadow handlers exist, like any other.
    assert!(p.handlers_by_runtime()["go"].contains(&"src/users.go#Get".to_string()));
}
