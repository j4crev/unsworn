//! The config file as written: `unsworn.toml` or `unsworn.yaml`.
//!
//! These types mirror the file closely and reject unknown keys, so typos are
//! reported instead of ignored. [`crate::compile`] turns them into a
//! validated, resolved [`crate::Project`].

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::units::{ConfigDuration, ConfigSize};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub server: RawServer,
    pub routes: Option<RawRoutesDir>,
    pub openapi: Option<RawOpenApi>,
    /// Planned for v2. Accepted only to report a clear error.
    pub mcp: Option<Value>,
    #[serde(default)]
    pub runtimes: BTreeMap<String, RawRuntime>,
    #[serde(default)]
    pub auth: BTreeMap<String, RawAuthScheme>,
    pub store: Option<RawStore>,
    #[serde(default)]
    pub defaults: RawPolicies,
    #[serde(default)]
    pub policies: BTreeMap<String, RawPolicies>,
    #[serde(default, rename = "route")]
    pub route: Vec<RawRoute>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawServer {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub max_body: Option<ConfigSize>,
    pub timeout: Option<ConfigDuration>,
    #[serde(default)]
    pub trailing_slash: TrailingSlash,
    pub tls: Option<RawTls>,
    #[serde(default = "default_true")]
    pub compression: bool,
    /// Proxies whose `X-Forwarded-For` header is trusted: CIDRs, or `"all"`.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Optional separate listener for `/_unsworn/metrics`.
    pub admin_listen: Option<String>,
    #[serde(default)]
    pub response_validation: ResponseValidation,
    pub shutdown_timeout: Option<ConfigDuration>,
}

impl Default for RawServer {
    fn default() -> Self {
        RawServer {
            listen: default_listen(),
            max_body: None,
            timeout: None,
            trailing_slash: TrailingSlash::default(),
            tls: None,
            compression: true,
            trusted_proxies: Vec::new(),
            admin_listen: None,
            response_validation: ResponseValidation::default(),
            shutdown_timeout: None,
        }
    }
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrailingSlash {
    #[default]
    Redirect,
    Strict,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseValidation {
    Off,
    #[default]
    Log,
    Enforce,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTls {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRoutesDir {
    pub dir: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOpenApi {
    pub spec: Option<String>,
    pub handler: Option<String>,
    /// Planned for v3.
    pub mcp_tools: Option<Value>,
    pub title: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Jsonl,
    Binary,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRuntime {
    pub extensions: Vec<String>,
    pub command: String,
    pub build: Option<String>,
    #[serde(default)]
    pub protocol: Protocol,
    pub workers: Option<u32>,
    #[serde(default)]
    pub eager: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub startup_timeout: Option<ConfigDuration>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RawAuthScheme {
    Jwt(RawJwt),
    ApiKey(RawApiKey),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawJwt {
    pub jwks_url: Option<String>,
    pub secret: Option<String>,
    pub public_key: Option<String>,
    pub issuer: Option<String>,
    pub audience: Option<StringOrList>,
    pub algorithms: Option<Vec<String>>,
    pub header: Option<String>,
    pub leeway: Option<ConfigDuration>,
    pub protected_resource: Option<RawProtectedResource>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProtectedResource {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    pub resource_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawApiKey {
    pub header: Option<String>,
    pub query: Option<String>,
    pub keys: StringOrList,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStore {
    #[serde(default = "default_store")]
    pub backend: String,
    pub url: Option<String>,
}

fn default_store() -> String {
    "memory".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s.clone()],
            StringOrList::Many(v) => v.clone(),
        }
    }
}

/// A setting that can also be switched off with `"off"`.
#[derive(Debug, Clone, PartialEq)]
pub enum OrOff<T> {
    Off,
    On(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OrOff<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper<T> {
            Str(String),
            Val(T),
        }
        match Helper::<T>::deserialize(d)? {
            Helper::Str(s) if s == "off" => Ok(OrOff::Off),
            Helper::Str(s) => Err(serde::de::Error::custom(format!(
                "expected a table or \"off\", found \"{s}\""
            ))),
            Helper::Val(v) => Ok(OrOff::On(v)),
        }
    }
}

/// `auth` as written: `"jwt"`, `"none"`, `["jwt", "service"]`,
/// `{ scheme = "jwt", scopes = [...] }`, or a list of those.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum RawAuth {
    Name(String),
    One(RawAuthAlt),
    Many(Vec<RawAuthItem>),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum RawAuthItem {
    Name(String),
    Alt(RawAuthAlt),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAuthAlt {
    pub scheme: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCors {
    pub origins: StringOrList,
    #[serde(default)]
    pub credentials: bool,
    pub methods: Option<Vec<String>>,
    pub headers: Option<Vec<String>>,
    #[serde(default)]
    pub expose: Vec<String>,
    pub max_age: Option<ConfigDuration>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOrigin {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRateLimit {
    pub requests: u32,
    pub per: ConfigDuration,
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCache {
    pub ttl: ConfigDuration,
    #[serde(default)]
    pub vary: Vec<String>,
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHeaders {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Policy keys, as used in `[defaults]`, `[policies.*]` and
/// `_policies.toml` files. Route entries repeat these fields.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPolicies {
    pub auth: Option<RawAuth>,
    pub cors: Option<OrOff<RawCors>>,
    pub origin: Option<OrOff<RawOrigin>>,
    pub rate_limit: Option<OrOff<RawRateLimit>>,
    pub cache: Option<OrOff<RawCache>>,
    pub timeout: Option<ConfigDuration>,
    pub max_body: Option<ConfigSize>,
    pub headers: Option<OrOff<RawHeaders>>,
    pub before: Option<OrOff<Vec<String>>>,
    #[serde(rename = "use")]
    pub use_sets: Option<Vec<String>>,
}

/// One inline parameter rule: `{ type = "integer", min = 1 }`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawParam {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub required: Option<bool>,
    pub default: Option<Value>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub min_length: Option<u64>,
    pub max_length: Option<u64>,
    #[serde(rename = "enum")]
    pub one_of: Option<Vec<Value>>,
    pub pattern: Option<String>,
    pub format: Option<String>,
    pub items: Option<String>,
}

/// A JSON Schema reference: a file path, or an inline schema table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum SchemaRef {
    File(String),
    Inline(Map<String, Value>),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRespond {
    #[serde(default = "default_status")]
    pub status: u16,
    pub json: Option<Value>,
    pub text: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_status() -> u16 {
    200
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRedirect {
    pub to: String,
    #[serde(default = "default_redirect")]
    pub status: u16,
}

fn default_redirect() -> u16 {
    308
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStatic {
    pub dir: String,
    pub spa_fallback: Option<String>,
    pub index: Option<String>,
    pub cache_control: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum RawProxy {
    Url(String),
    Full(RawProxyFull),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProxyFull {
    pub upstreams: Vec<String>,
    pub strip_prefix: Option<String>,
    pub sticky: Option<String>,
    #[serde(default)]
    pub preserve_host: bool,
}

/// A `[[route]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRoute {
    pub path: String,
    pub method: Option<StringOrList>,
    pub host: Option<String>,

    // Actions: at most one.
    pub respond: Option<RawRespond>,
    pub redirect: Option<RawRedirect>,
    #[serde(rename = "static")]
    pub static_files: Option<RawStatic>,
    pub proxy: Option<RawProxy>,
    pub handler: Option<String>,
    pub runtime: Option<String>,

    // Validation.
    pub params: Option<BTreeMap<String, RawParam>>,
    pub query: Option<BTreeMap<String, RawParam>>,
    pub body: Option<SchemaRef>,
    pub response: Option<SchemaRef>,

    // Policies (same keys as RawPolicies).
    pub auth: Option<RawAuth>,
    pub cors: Option<OrOff<RawCors>>,
    pub origin: Option<OrOff<RawOrigin>>,
    pub rate_limit: Option<OrOff<RawRateLimit>>,
    pub cache: Option<OrOff<RawCache>>,
    pub timeout: Option<ConfigDuration>,
    pub max_body: Option<ConfigSize>,
    /// Either request-header validation (a table of param rules) or the
    /// `headers` policy (`{ set = ..., remove = ... }`). See
    /// [`RawRoute::split_headers`].
    pub headers: Option<Value>,
    pub before: Option<OrOff<Vec<String>>>,
    #[serde(rename = "use")]
    pub use_sets: Option<Vec<String>>,

    /// Planned for v3.
    pub mcp: Option<Value>,
}

/// The two meanings of `headers` on a route.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SplitHeaders {
    pub validation: Option<BTreeMap<String, RawParam>>,
    pub policy: Option<OrOff<RawHeaders>>,
}

impl RawRoute {
    /// `headers = { set = {...}, remove = [...] }` is the response-headers
    /// policy. Any other table is request-header validation.
    pub fn split_headers(&self) -> Result<SplitHeaders, String> {
        let Some(value) = &self.headers else {
            return Ok(SplitHeaders::default());
        };
        if value.as_str() == Some("off") {
            return Ok(SplitHeaders {
                validation: None,
                policy: Some(OrOff::Off),
            });
        }
        let Some(table) = value.as_object() else {
            return Err("`headers` must be a table".into());
        };
        let is_policy = !table.is_empty() && table.keys().all(|k| k == "set" || k == "remove");
        if is_policy {
            let policy: RawHeaders =
                serde_json::from_value(value.clone()).map_err(|e| format!("headers: {e}"))?;
            return Ok(SplitHeaders {
                validation: None,
                policy: Some(OrOff::On(policy)),
            });
        }
        let validation: BTreeMap<String, RawParam> =
            serde_json::from_value(value.clone()).map_err(|e| format!("headers: {e}"))?;
        Ok(SplitHeaders {
            validation: Some(validation),
            policy: None,
        })
    }

    pub fn policies(&self) -> Result<RawPolicies, String> {
        Ok(RawPolicies {
            auth: self.auth.clone(),
            cors: self.cors.clone(),
            origin: self.origin.clone(),
            rate_limit: self.rate_limit.clone(),
            cache: self.cache.clone(),
            timeout: self.timeout,
            max_body: self.max_body,
            headers: self.split_headers()?.policy,
            before: self.before.clone(),
            use_sets: self.use_sets.clone(),
        })
    }

    pub fn action_count(&self) -> usize {
        [
            self.respond.is_some(),
            self.redirect.is_some(),
            self.static_files.is_some(),
            self.proxy.is_some(),
            self.handler.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }
}

/// The format of a config file, picked from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Yaml,
    Json,
}

impl Format {
    pub fn from_path(path: &str) -> Format {
        if path.ends_with(".yaml") || path.ends_with(".yml") {
            Format::Yaml
        } else if path.ends_with(".json") {
            Format::Json
        } else {
            Format::Toml
        }
    }
}

/// Deserializes any config-like document (config, `_policies.toml`, schemas).
pub fn parse_document<T: serde::de::DeserializeOwned>(
    text: &str,
    format: Format,
) -> Result<T, String> {
    match format {
        Format::Toml => toml::from_str(text).map_err(|e| e.to_string().trim_end().to_string()),
        Format::Yaml => serde_norway::from_str(text).map_err(|e| e.to_string()),
        Format::Json => serde_json::from_str(text).map_err(|e| e.to_string()),
    }
}

/// Line numbers of each `[[route]]` header in a TOML file, in order.
pub fn route_lines(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let t = line.trim_start();
            t.starts_with("[[route]]") || t.starts_with("[[ route ]]")
        })
        .map(|(i, _)| i + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let text = r#"
[server]
listen = "127.0.0.1:9000"
max_body = "2MB"

[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"
protocol = "binary"

[auth.jwt]
type = "jwt"
secret = "shh"
audience = "api"

[defaults]
timeout = "10s"
cache = { ttl = "30s" }

[policies.public]
auth = "none"

[[route]]
path = "/users/{id}"
method = "GET"
handler = "src/users.ts#getUser"
params = { id = { type = "integer", min = 1 } }
headers = { set = { "x-a" = "b" } }
auth = { scheme = "jwt", scopes = ["users:read"] }
cache = "off"

[[route]]
path = "/legacy/{*rest}"
proxy = "http://old:8080"
"#;
        let cfg: RawConfig = parse_document(text, Format::Toml).unwrap();
        assert_eq!(cfg.server.listen, "127.0.0.1:9000");
        assert_eq!(cfg.route.len(), 2);
        let r = &cfg.route[0];
        assert_eq!(r.cache, Some(OrOff::Off));
        assert!(r.split_headers().unwrap().policy.is_some());
        assert!(matches!(r.auth, Some(RawAuth::One(_))));
        assert_eq!(route_lines(text), vec![23, 32]);
    }

    #[test]
    fn rejects_unknown_keys() {
        let err =
            parse_document::<RawConfig>("[server]\nlisten_on = \"x\"\n", Format::Toml).unwrap_err();
        assert!(err.contains("listen_on"), "{err}");
    }

    #[test]
    fn headers_validation_vs_policy() {
        let route: RawRoute = parse_document(
            "path = \"/\"\nheaders = { \"x-tenant\" = { required = true } }\n",
            Format::Toml,
        )
        .unwrap();
        let split = route.split_headers().unwrap();
        assert!(split.validation.unwrap().contains_key("x-tenant"));
        assert!(split.policy.is_none());
    }

    #[test]
    fn yaml_config() {
        let cfg: RawConfig = parse_document(
            "server:\n  listen: 0.0.0.0:1\nroute:\n  - path: /health\n    respond: { status: 200, json: { ok: true } }\n",
            Format::Yaml,
        )
        .unwrap();
        assert_eq!(cfg.route[0].respond.as_ref().unwrap().status, 200);
    }
}
