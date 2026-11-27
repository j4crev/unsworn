//! The config file as written: `unsworn.toml` or `unsworn.yaml`.
//!
//! These types mirror the file closely and reject unknown keys, so typos are
//! reported instead of ignored. [`crate::compile`] turns them into a
//! validated, resolved [`crate::Project`].

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::units::{ConfigDuration, ConfigSize};

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub server: RawServer,
    pub routes: Option<RawRoutesDir>,
    pub openapi: Option<RawOpenApi>,
    /// A remote MCP server: tools as handlers, or policies for a proxied one.
    pub mcp: Option<RawMcp>,
    #[serde(default)]
    pub runtimes: BTreeMap<String, RawRuntime>,
    #[serde(default)]
    pub auth: BTreeMap<String, RawAuthScheme>,
    pub store: Option<RawStore>,
    /// Files of typed request models to write for handlers, by language.
    #[serde(rename = "gen")]
    pub types: Option<RawGen>,
    #[serde(default)]
    pub defaults: RawPolicies,
    #[serde(default)]
    pub policies: BTreeMap<String, RawPolicies>,
    #[serde(default, rename = "route")]
    pub route: Vec<RawRoute>,
    /// Handlers that other handlers call by name. Nothing outside reaches
    /// them.
    #[serde(default, rename = "function")]
    pub function: Vec<RawFunction>,
    /// Services Unsworn calls for handlers, adding each one's credential.
    #[serde(default)]
    pub outbound: BTreeMap<String, RawOutbound>,
    /// OpenTelemetry traces, sent to one of the `outbound` services.
    pub telemetry: Option<RawTelemetry>,
}

/// `[telemetry]`: where Unsworn sends spans, and of how many requests. It
/// has no URL of its own: spans only go to a declared outbound service.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawTelemetry {
    /// The name of the `[outbound.<name>]` that spans are sent to over
    /// OTLP/HTTP. It must allow `POST /v1/traces`.
    pub outbound: String,
    /// The service name on the spans. Default: `OTEL_SERVICE_NAME`, then
    /// "unsworn".
    pub service_name: Option<String>,
    /// The share of requests that are recorded, from 0 to 1. One from a
    /// proxy in `[server].trusted_proxies` that has a `traceparent` follows
    /// its sampled flag instead. Default: 0.
    pub sample: Option<f64>,
    /// Whether the server's metrics are sent too, with `POST /v1/metrics`,
    /// which the service must then allow. Default: false.
    pub metrics: Option<bool>,
    /// Whether the server's logs are sent too, worker output included, with
    /// `POST /v1/logs`, which the service must then allow. Default: false.
    pub logs: Option<bool>,
}

/// `[outbound.<name>]`: a service handlers reach with `ctx.fetch(name, ...)`.
/// Unsworn makes the request and adds `headers`, so a handler never holds
/// the service's credential.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawOutbound {
    /// Where the service is, such as "https://api.stripe.com". A handler's
    /// path goes after it. May use `{env.NAME}`.
    pub url: String,
    /// Headers added to every request, replacing any a handler sets by the
    /// same name: `{ Authorization = "Bearer {env.STRIPE_KEY}" }`. The
    /// variables read here aren't passed to workers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Methods a handler may use. Default: any.
    pub methods: Option<Vec<String>>,
    /// Paths a handler may ask for, as route patterns such as
    /// "/v1/charges/{id}" or "/v1/customers/{*rest}". Default: any.
    pub paths: Option<Vec<String>>,
    /// The longest a request may take. Default: `[server].timeout`.
    pub timeout: Option<ConfigDuration>,
    /// The largest response body. Default: "10MB".
    pub max_response: Option<ConfigSize>,
}

/// A `[[function]]`: a handler other handlers call with `ctx.call`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawFunction {
    /// What callers call it by: letters, digits, `_`, `-` and `.`.
    pub name: String,
    pub handler: String,
    /// The runtime, when the handler file's extension doesn't say.
    pub runtime: Option<String>,
    /// A JSON Schema for the input: a file path or an inline table. The
    /// input is checked against it before the function runs.
    pub input: Option<SchemaRef>,
    /// A JSON Schema for what it answers with.
    pub output: Option<SchemaRef>,
    /// The longest a call may take. Default: `[server].timeout`. A call
    /// also ends when the request it's made for runs out of time.
    pub timeout: Option<ConfigDuration>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawServer {
    /// The address to serve on. Default: "0.0.0.0:8080", or
    /// "127.0.0.1:8080" under `unsworn dev`, whose detailed errors are for
    /// this machine only.
    pub listen: Option<String>,
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
    /// The most connections held open at once. More wait to be accepted.
    pub max_connections: Option<u32>,
    /// How long a kept-alive connection may wait for its next request.
    pub idle_timeout: Option<ConfigDuration>,
}

impl Default for RawServer {
    fn default() -> Self {
        RawServer {
            listen: None,
            max_body: None,
            timeout: None,
            trailing_slash: TrailingSlash::default(),
            tls: None,
            compression: true,
            trusted_proxies: Vec::new(),
            admin_listen: None,
            response_validation: ResponseValidation::default(),
            shutdown_timeout: None,
            max_connections: None,
            idle_timeout: None,
        }
    }
}

/// Where `unsworn serve` listens when the config doesn't say.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
/// Where `unsworn dev` listens when the config doesn't say.
pub const DEFAULT_DEV_LISTEN: &str = "127.0.0.1:8080";

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrailingSlash {
    #[default]
    Redirect,
    Strict,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseValidation {
    Off,
    #[default]
    Log,
    Enforce,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawTls {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawRoutesDir {
    pub dir: String,
}

/// `[gen]`: where `unsworn gen` and `unsworn dev` write each language's
/// types, relative to the project directory.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawGen {
    /// TypeScript, such as "src/unsworn.gen.ts".
    pub ts: Option<String>,
    /// Python, such as "unsworn_gen.py".
    pub python: Option<String>,
    /// Go: a path, or `{ file = "...", package = "..." }` when the file
    /// isn't in package `main`.
    pub go: Option<RawGenGo>,
    /// Rust, such as "src/unsworn_gen.rs".
    pub rust: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawGenGo {
    File(String),
    Full(RawGenGoFile),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawGenGoFile {
    pub file: String,
    /// The file's Go package. Default: "main".
    pub package: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawOpenApi {
    pub spec: Option<String>,
    pub handler: Option<String>,
    /// Which operations are also MCP tools: `"all"`, or `{ tags = [...] }`.
    pub mcp_tools: Option<RawOpenApiTools>,
    pub title: Option<String>,
    pub version: Option<String>,
}

/// `mcp_tools` in `[openapi]`.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawOpenApiTools {
    /// `"all"`: every operation with an `operationId`.
    All(String),
    /// `{ tags = ["public"] }`: operations with any of these tags.
    Tags(RawToolTags),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawToolTags {
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Tier 1: JSON lines over stdin/stdout.
    #[default]
    Jsonl,
    /// Tier 2: length-prefixed frames over a Unix socket.
    Framed,
    /// An HTTP server of the runtime's own: any language's, with no SDK.
    /// Unsworn starts it and forwards requests to it once they've passed
    /// their route's rules. Routes reach it with `proxy = { runtime = ... }`.
    Http,
    /// Deprecated: the old name for `framed`.
    #[schemars(extend("deprecated" = true))]
    Binary,
}

/// `workers` in a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawWorkers {
    /// Exactly this many, always.
    Fixed(u32),
    /// Between these, by load.
    Range(RawWorkerRange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawWorkerRange {
    /// How many to start, and keep when there's little to do. Default: 1.
    pub min: Option<u32>,
    /// The most to run. Default: the number of CPUs available.
    pub max: Option<u32>,
    /// How long a worker the others could do without is kept before it's
    /// retired. Default: "30s".
    pub idle: Option<ConfigDuration>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawRuntime {
    /// The file extensions of the handlers it runs. An `http` runtime
    /// has none: routes name it.
    #[serde(default)]
    pub extensions: Vec<String>,
    pub command: String,
    pub build: Option<String>,
    #[serde(default)]
    pub protocol: Protocol,
    /// How many worker processes: a number for exactly that many, or
    /// `{ min, max }` to start `min` and add more under load. Default:
    /// one, growing to the number of CPUs available.
    pub workers: Option<RawWorkers>,
    #[serde(default)]
    pub eager: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub startup_timeout: Option<ConfigDuration>,
    /// Where an `http` runtime's server listens. Default: `"socket"`.
    pub listen: Option<Listen>,
    /// The `[[function]]`s and `[outbound.<name>]` services an `http`
    /// runtime's server may call through Unsworn, by name, or `["*"]` for
    /// all of them. Default: none, so it isn't given the endpoint and its
    /// requests carry no `Unsworn-Call` token.
    #[serde(default)]
    pub calls: Vec<String>,
}

/// Where an `http` runtime's server listens for Unsworn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Listen {
    /// A Unix socket only Unsworn's user can open, at the path in
    /// `UNSWORN_SOCKET`. Nothing else can reach the server.
    #[default]
    Socket,
    /// A port on this machine that Unsworn picks, in `PORT`, for a server
    /// that can't listen on a socket. Anything on the machine can reach it.
    Port,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RawAuthScheme {
    Jwt(RawJwt),
    ApiKey(RawApiKey),
    Webhook(RawWebhook),
}

/// A scheme for requests a service signs with a shared secret.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawWebhook {
    /// The secret the service signs with.
    pub secret: String,
    /// A service whose way of signing Unsworn knows: `stripe`, `github`,
    /// `slack`, `standard` (Standard Webhooks) or `svix`.
    pub provider: Option<String>,
    /// Or, for any other: the header that holds an HMAC-SHA256 of the
    /// body.
    pub header: Option<String>,
    /// How that header writes it: `hex` (the default) or `base64`.
    pub encoding: Option<String>,
    /// Text in front of it, such as `sha256=`.
    pub prefix: Option<String>,
    /// How far a signed time may be from now, for the providers that sign
    /// one. Default: 5 minutes.
    pub tolerance: Option<ConfigDuration>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawJwt {
    /// A JWKS document with the verification keys.
    pub url: Option<String>,
    /// Deprecated: the old name for `url`.
    #[schemars(extend("deprecated" = true))]
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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawProtectedResource {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    pub resource_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawApiKey {
    /// The request header that carries the key.
    pub header: Option<String>,
    /// The query parameter that carries the key.
    pub query: Option<String>,
    /// The valid keys: a list, or a comma-separated string.
    pub keys: Option<StringOrList>,
    /// An OAuth 2.0 token introspection endpoint (RFC 7662) that verifies
    /// keys, instead of `keys`.
    pub url: Option<String>,
    /// Headers sent to `url`, such as its own credentials.
    pub headers: Option<BTreeMap<String, String>>,
    /// How long to wait for `url` before answering `503`. Default: 2s.
    pub timeout: Option<ConfigDuration>,
    /// How long to remember the verifier's answer for a key, in `[store]`.
    /// Default: not cached.
    pub cache: Option<ConfigDuration>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawStore {
    #[serde(default)]
    pub backend: StoreBackend,
    /// For the `redis` backend: `redis://` or `rediss://` (TLS), or the
    /// `https://` URL of a Redis REST API (Upstash's protocol), which is
    /// how serverless functions reach Redis.
    pub url: Option<String>,
    /// The REST API's token, with an `https://` `url`.
    pub token: Option<String>,
    /// Prepended to every key, so separate projects can share one Redis.
    pub prefix: Option<String>,
    /// What rate limits do when the store can't be reached.
    #[serde(default)]
    pub on_error: OnError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackend {
    /// In this process only. Fine for a single instance.
    #[default]
    Memory,
    /// Shared by every instance that uses the same Redis (or Valkey, Dragonfly...).
    Redis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Let requests through without counting them.
    #[default]
    Open,
    /// Answer `503`.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
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

impl<T: JsonSchema> JsonSchema for OrOff<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("{}OrOff", T::schema_name()).into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let inner = generator.subschema_for::<T>();
        schemars::json_schema!({
            "anyOf": [{ "const": "off", "description": "Switch off a setting inherited from a broader layer." }, inner]
        })
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OrOff<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawAuth {
    Name(String),
    One(RawAuthAlt),
    Many(Vec<RawAuthItem>),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawAuthItem {
    Name(String),
    Alt(RawAuthAlt),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawAuthAlt {
    pub scheme: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawOrigin {
    pub allow: Vec<String>,
}

/// `ip = { allow = ["10.0.0.0/8"], deny = ["10.0.0.5"] }`
#[derive(Debug, Clone, PartialEq, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawIp {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawRateLimit {
    pub requests: u32,
    pub per: ConfigDuration,
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawCache {
    pub ttl: ConfigDuration,
    #[serde(default)]
    pub vary: Vec<String>,
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawHeaders {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Policy keys, as used in `[defaults]` and `[policies.*]`. Route entries
/// repeat these fields.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawPolicies {
    pub auth: Option<RawAuth>,
    pub cors: Option<OrOff<RawCors>>,
    pub origin: Option<OrOff<RawOrigin>>,
    pub ip: Option<OrOff<RawIp>>,
    pub rate_limit: Option<OrOff<RawRateLimit>>,
    pub cache: Option<OrOff<RawCache>>,
    pub timeout: Option<ConfigDuration>,
    pub max_body: Option<ConfigSize>,
    /// Pass the request body to the handler as it arrives, instead of
    /// reading it first.
    pub stream_body: Option<bool>,
    /// Headers to set on, or remove from, the response.
    pub response_headers: Option<OrOff<RawHeaders>>,
    pub before: Option<OrOff<Vec<String>>>,
    /// Pairs of values that have to be the same, such as
    /// `{ "params.org" = "auth.org" }`.
    pub require: Option<OrOff<RawRequire>>,
    #[serde(rename = "use")]
    pub use_sets: Option<Vec<String>>,
}

/// `require` as written: each key is one value and what it's set to is the
/// other. `"params.org" = "auth.org"` and, with TOML's dotted keys,
/// `params.org = "auth.org"` say the same.
pub type RawRequire = BTreeMap<String, RawRequireValue>;

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawRequireValue {
    Name(String),
    /// What an unquoted dotted key turns into: `params.org = "auth.org"`
    /// is `{ params = { org = "auth.org" } }`.
    Dotted(BTreeMap<String, String>),
}

/// One inline parameter rule: `{ type = "integer", min = 1 }`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SchemaRef {
    File(String),
    Inline(Map<String, Value>),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawRedirect {
    pub to: String,
    #[serde(default = "default_redirect")]
    pub status: u16,
}

fn default_redirect() -> u16 {
    308
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawStatic {
    pub dir: String,
    pub spa_fallback: Option<String>,
    pub index: Option<String>,
    pub cache_control: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawProxy {
    Url(String),
    Full(RawProxyFull),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawProxyFull {
    /// Where to forward: one or more URLs, or `unix:/path`.
    #[serde(default)]
    pub upstreams: Vec<String>,
    /// Or an `http` runtime, whose server Unsworn starts itself.
    pub runtime: Option<String>,
    pub strip_prefix: Option<String>,
    pub sticky: Option<String>,
    #[serde(default)]
    pub preserve_host: bool,
    /// Whether the upstream is told who the caller is, in the
    /// `Unsworn-Context` header. Default: true.
    pub context: Option<bool>,
}

/// A `[[route]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
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
    /// Rules for request headers, by name.
    pub headers: Option<BTreeMap<String, RawParam>>,
    pub body: Option<SchemaRef>,
    pub response: Option<SchemaRef>,

    // Policies (same keys as RawPolicies).
    pub auth: Option<RawAuth>,
    pub cors: Option<OrOff<RawCors>>,
    pub origin: Option<OrOff<RawOrigin>>,
    pub ip: Option<OrOff<RawIp>>,
    pub rate_limit: Option<OrOff<RawRateLimit>>,
    pub cache: Option<OrOff<RawCache>>,
    pub timeout: Option<ConfigDuration>,
    pub max_body: Option<ConfigSize>,
    /// Pass the request body to the handler as it arrives, instead of
    /// reading it first.
    pub stream_body: Option<bool>,
    /// Headers to set on, or remove from, the response.
    pub response_headers: Option<OrOff<RawHeaders>>,
    pub before: Option<OrOff<Vec<String>>>,
    /// Pairs of values that have to be the same, such as
    /// `{ "params.org" = "auth.org" }`.
    pub require: Option<OrOff<RawRequire>>,
    #[serde(rename = "use")]
    pub use_sets: Option<Vec<String>>,

    /// Offer the route as an MCP tool too.
    pub mcp: Option<RawRouteTool>,

    /// A second handler that gets a copy of each request, to compare with
    /// the route's own: a handler reference, or `{ handler, runtime, sample }`.
    /// Clients only ever get the route's own answer.
    pub shadow: Option<RawShadow>,

    /// Paths whose cached responses a successful request to this route
    /// makes out of date, such as `["/users/{id}", "/users"]`.
    pub invalidates: Option<Vec<String>>,
}

/// `shadow` on a `[[route]]`.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RawShadow {
    Handler(String),
    Full(RawShadowFull),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawShadowFull {
    pub handler: String,
    /// The runtime, when the handler file's extension doesn't say.
    pub runtime: Option<String>,
    /// The share of requests to copy, from 0 to 1. Default: 1, all of them.
    pub sample: Option<f64>,
}

/// `mcp = { tool = "get_user", description = "..." }` on a `[[route]]`:
/// the route is also an MCP tool, with arguments built from its contract.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawRouteTool {
    /// The tool name.
    pub tool: String,
    pub title: Option<String>,
    /// What the tool does. Defaults to the OpenAPI operation's summary or
    /// description.
    pub description: Option<String>,
    pub annotations: Option<RawToolAnnotations>,
    /// The method tool calls use, for a route that accepts several.
    pub method: Option<String>,
}

/// What `tools/list` shows a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListTools {
    /// Only tools whose `auth` scopes the caller's token has.
    #[default]
    Permitted,
    /// Every tool. Calls are still checked.
    All,
}

fn default_mcp_path() -> String {
    "/mcp".into()
}

/// The `[mcp]` table: one MCP endpoint and its tools.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawMcp {
    /// The endpoint path. Default: `/mcp`.
    #[serde(default = "default_mcp_path")]
    pub path: String,
    /// The server name reported to clients. Default: `unsworn`.
    pub name: Option<String>,
    /// The server version reported to clients. Default: the Unsworn version.
    pub version: Option<String>,
    /// A display name for the server.
    pub title: Option<String>,
    /// Guidance for the model on how to use this server's tools.
    pub instructions: Option<String>,
    /// An existing MCP server to forward to. Tools then only add policies.
    pub proxy: Option<RawProxy>,
    /// What `tools/list` shows: `permitted` (default) or `all`.
    #[serde(default)]
    pub list_tools: ListTools,
    /// How long clients may cache `tools/list` and `server/discover`
    /// results. Default: 5m.
    pub list_ttl: Option<ConfigDuration>,
    #[serde(default)]
    pub tool: Vec<RawMcpTool>,

    // Policies for the whole endpoint.
    pub auth: Option<RawAuth>,
    pub cors: Option<OrOff<RawCors>>,
    pub origin: Option<OrOff<RawOrigin>>,
    pub ip: Option<OrOff<RawIp>>,
    pub rate_limit: Option<OrOff<RawRateLimit>>,
    pub timeout: Option<ConfigDuration>,
    pub max_body: Option<ConfigSize>,
    pub response_headers: Option<OrOff<RawHeaders>>,
    pub before: Option<OrOff<Vec<String>>>,
    #[serde(rename = "use")]
    pub use_sets: Option<Vec<String>>,
}

impl RawMcp {
    pub fn policies(&self) -> RawPolicies {
        RawPolicies {
            auth: self.auth.clone(),
            cors: self.cors.clone(),
            origin: self.origin.clone(),
            ip: self.ip.clone(),
            rate_limit: self.rate_limit.clone(),
            timeout: self.timeout,
            max_body: self.max_body,
            response_headers: self.response_headers.clone(),
            before: self.before.clone(),
            use_sets: self.use_sets.clone(),
            ..Default::default()
        }
    }
}

/// Hints about a tool's behavior, for clients. Not enforced.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawToolAnnotations {
    /// The tool doesn't change anything.
    pub read_only: Option<bool>,
    /// The tool may delete or overwrite data.
    pub destructive: Option<bool>,
    /// Calling the tool again with the same arguments has no further effect.
    pub idempotent: Option<bool>,
    /// The tool reaches systems outside this server, such as the web.
    pub open_world: Option<bool>,
}

/// One `[[mcp.tool]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawMcpTool {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// JSON Schema for the arguments: a file, or an inline table.
    pub input: Option<SchemaRef>,
    /// JSON Schema for structured results: a file, or an inline table.
    pub output: Option<SchemaRef>,
    pub handler: Option<String>,
    pub runtime: Option<String>,
    pub annotations: Option<RawToolAnnotations>,

    // Policies for calls to this tool, on top of the `[mcp]` ones.
    pub auth: Option<RawAuth>,
    pub rate_limit: Option<OrOff<RawRateLimit>>,
    pub timeout: Option<ConfigDuration>,
    pub before: Option<OrOff<Vec<String>>>,
    #[serde(rename = "use")]
    pub use_sets: Option<Vec<String>>,
}

impl RawMcpTool {
    pub fn policies(&self) -> RawPolicies {
        RawPolicies {
            auth: self.auth.clone(),
            rate_limit: self.rate_limit.clone(),
            timeout: self.timeout,
            before: self.before.clone(),
            use_sets: self.use_sets.clone(),
            ..Default::default()
        }
    }
}

impl RawRoute {
    pub fn policies(&self) -> RawPolicies {
        RawPolicies {
            auth: self.auth.clone(),
            cors: self.cors.clone(),
            origin: self.origin.clone(),
            ip: self.ip.clone(),
            rate_limit: self.rate_limit.clone(),
            cache: self.cache.clone(),
            timeout: self.timeout,
            max_body: self.max_body,
            stream_body: self.stream_body,
            response_headers: self.response_headers.clone(),
            before: self.before.clone(),
            require: self.require.clone(),
            use_sets: self.use_sets.clone(),
        }
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

/// Deserializes any config-like document (config, schemas).
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
    table_lines(text, "[[route]]")
}

/// Line numbers of each line that opens the TOML table `header`, such as
/// `[mcp]` or `[[mcp.tool]]`, in order.
pub fn table_lines(text: &str, header: &str) -> Vec<usize> {
    let want: String = header.chars().filter(|c| !c.is_whitespace()).collect();
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let t: String = line
                .split('#')
                .next()
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            t == want
        })
        .map(|(i, _)| i + 1)
        .collect()
}

/// The JSON Schema for `unsworn.toml` (and `.yaml`/`.json`), for editors.
pub fn json_schema() -> Value {
    let schema = schemars::schema_for!(RawConfig);
    let mut value = serde_json::to_value(schema).unwrap_or_default();
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "$id".into(),
            Value::String(
                "https://raw.githubusercontent.com/j4crev/unsworn/main/schema/unsworn.schema.json"
                    .into(),
            ),
        );
        obj.insert(
            "title".into(),
            Value::String("Unsworn configuration".into()),
        );
    }
    value
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
protocol = "framed"

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
response_headers = { set = { "x-a" = "b" } }
auth = { scheme = "jwt", scopes = ["users:read"] }
cache = "off"

[[route]]
path = "/legacy/{*rest}"
proxy = "http://old:8080"
"#;
        let cfg: RawConfig = parse_document(text, Format::Toml).unwrap();
        assert_eq!(cfg.server.listen.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(cfg.route.len(), 2);
        let r = &cfg.route[0];
        assert_eq!(r.cache, Some(OrOff::Off));
        assert!(r.response_headers.is_some());
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
    fn request_header_rules_and_response_headers_are_two_keys() {
        let route: RawRoute = parse_document(
            "path = \"/\"\nheaders = { \"x-tenant\" = { required = true } }\nresponse_headers = { remove = [\"server\"] }\n",
            Format::Toml,
        )
        .unwrap();
        assert!(route.headers.unwrap().contains_key("x-tenant"));
        assert!(route.response_headers.is_some());
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
