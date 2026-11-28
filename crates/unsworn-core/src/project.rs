//! Compiling a project into a route table, and matching requests.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::Algorithm;
use serde::Serialize;
use serde_json::Value;

use crate::auth::{
    ApiKeyScheme, ApiKeys, JwtKeys, JwtScheme, KeyVerifier, ProtectedResource, Scheme, Signed,
    WebhookScheme,
};
use crate::config::{
    self, Format, OrOff, Protocol, RawAuth, RawAuthItem, RawAuthScheme, RawConfig, RawMcp,
    RawPolicies, RawProxy, RawRoute, RawRouteTool, ResponseValidation, SchemaRef, TrailingSlash,
};
use crate::diag::{Diagnostic, Diagnostics};
use crate::mcp::{self, BodyArgs, McpEndpoint, McpTool, RouteBinding, ServerInfo};
use crate::openapi;
use crate::policy::{
    AuthAlt, AuthReq, CacheRule, Cors, Effective, HandlerRef, HeaderRules, KeySource, Layer,
    Operand, RateLimit, Requirement,
};
use crate::routefiles::{self, METHODS};
use crate::rules;
use crate::validate::{BodyRule, Build, LocationRules, ParamSpec, Validation, param_schema};

/// What the core needs from its host to load a project. All paths are
/// relative to the project root and use `/`.
pub trait Host {
    fn read(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
    /// Every file below `dir`, recursively, relative to `dir`.
    fn list(&self, dir: &str) -> Result<Vec<String>, String>;
    fn env(&self, key: &str) -> Option<String>;
}

#[derive(Debug, Clone, Serialize)]
pub struct Server {
    pub listen: String,
    /// The config sets `listen`. Otherwise `listen` is the default for
    /// `unsworn serve`, and `unsworn dev` uses its own.
    pub listen_set: bool,
    pub max_body: u64,
    pub timeout: Duration,
    pub trailing_slash: TrailingSlash,
    pub tls: Option<(String, String)>,
    pub compression: bool,
    pub trusted_proxies: Vec<String>,
    pub admin_listen: Option<String>,
    pub response_validation: ResponseValidation,
    pub shutdown_timeout: Duration,
    pub max_connections: usize,
    pub idle_timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct Runtime {
    pub name: String,
    pub extensions: Vec<String>,
    pub command: String,
    pub build: Option<String>,
    pub protocol: Protocol,
    pub workers: Workers,
    pub eager: bool,
    pub env: BTreeMap<String, String>,
    pub startup_timeout: Duration,
    /// Where an `http` runtime's server listens.
    pub listen: config::Listen,
    /// What an `http` runtime's server may call through Unsworn.
    pub calls: Calls,
}

/// The functions and outbound services an `http` runtime's server may
/// call (its `calls`). A handler's runtime has none: handlers call with
/// `ctx.call` and `ctx.fetch`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Calls {
    /// Nothing: the server isn't given the endpoint, and its requests
    /// carry no token.
    #[default]
    None,
    /// Every function and outbound service (`["*"]`).
    All,
    /// These, by name. A name can be a function's, a service's, or both.
    Only(BTreeSet<String>),
}

impl Calls {
    /// Whether the server may call anything at all.
    pub fn any(&self) -> bool {
        *self != Calls::None
    }

    /// Whether the server may call the function or service `name`.
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Calls::None => false,
            Calls::All => true,
            Calls::Only(names) => names.contains(name),
        }
    }
}

/// How many worker processes a runtime runs: `min` to start with, and up
/// to `max` when they're busy. Equal for a fixed number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Workers {
    pub min: u32,
    /// `None` leaves it to the host: one for each CPU it may use.
    pub max: Option<u32>,
    /// How long a worker the others could do without is kept.
    pub idle: Duration,
}

impl Default for Workers {
    fn default() -> Workers {
        Workers {
            min: 1,
            max: None,
            idle: Workers::IDLE,
        }
    }
}

impl Workers {
    /// The default for [`Workers::idle`].
    pub const IDLE: Duration = Duration::from_secs(30);

    pub fn fixed(n: u32) -> Workers {
        Workers {
            min: n.max(1),
            max: Some(n.max(1)),
            idle: Workers::IDLE,
        }
    }

    /// The most workers to run where `cores` CPUs are available.
    pub fn most(&self, cores: usize) -> usize {
        match self.max {
            Some(max) => max as usize,
            None => cores.max(1).max(self.min as usize),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Proxy {
    pub upstreams: Vec<String>,
    pub strip_prefix: Option<String>,
    /// Lowercase header name whose value pins a client to an upstream.
    pub sticky_header: Option<String>,
    pub preserve_host: bool,
    /// Whether the upstream gets the request's context (the caller's
    /// verified claims and what `before` handlers added) in the
    /// `Unsworn-Context` header.
    pub context: bool,
    /// The `http` runtime to forward to, in place of `upstreams`: the host
    /// knows where its server listens.
    pub runtime: Option<String>,
    /// How to reach each upstream. In step with `upstreams`.
    #[serde(skip)]
    targets: Vec<Target>,
}

/// Where a proxy's upstream is, worked out once from its URL.
#[derive(Debug, Clone)]
pub enum Target {
    /// A URL that is only a scheme and a host, as most are.
    Origin(Origin),
    /// A URL with a path of its own (`http://host/base`), which goes in
    /// front of each request's path.
    Url,
    /// A Unix socket's path (`unix:/run/app.sock`).
    Unix(Arc<str>),
}

/// An upstream URL that is only a scheme and a host, ready to put in front
/// of a request's path without parsing anything.
#[derive(Debug, Clone)]
pub struct Origin {
    pub scheme: http::uri::Scheme,
    pub authority: http::uri::Authority,
    /// The authority, as the `Host` header's value.
    pub host: http::HeaderValue,
}

/// What an upstream on a Unix socket starts with.
pub const UNIX_UPSTREAM: &str = "unix:";

impl Proxy {
    pub fn new(
        upstreams: Vec<String>,
        strip_prefix: Option<String>,
        sticky_header: Option<String>,
        preserve_host: bool,
    ) -> Proxy {
        let targets = upstreams.iter().map(|u| target(u)).collect();
        Proxy {
            upstreams,
            strip_prefix,
            sticky_header,
            preserve_host,
            context: true,
            runtime: None,
            targets,
        }
    }

    /// A proxy to an `http` runtime's server.
    pub fn to_runtime(mut self, runtime: Option<String>) -> Proxy {
        self.runtime = runtime;
        self
    }

    /// Whether the upstream is told who the caller is. It is by default.
    pub fn with_context(mut self, context: bool) -> Proxy {
        self.context = context;
        self
    }

    /// How to reach the upstream at `index`.
    pub fn target(&self, index: usize) -> &Target {
        self.targets.get(index).unwrap_or(&Target::Url)
    }
}

fn target(upstream: &str) -> Target {
    Target::of(upstream)
}

impl Target {
    /// How to reach an upstream, from its URL or `unix:/path`.
    pub fn of(upstream: &str) -> Target {
        if let Some(path) = upstream.strip_prefix(UNIX_UPSTREAM) {
            return Target::Unix(path.into());
        }
        origin(upstream).map_or(Target::Url, Target::Origin)
    }
}

fn origin(upstream: &str) -> Option<Origin> {
    let parts = upstream.parse::<http::Uri>().ok()?.into_parts();
    let (scheme, authority) = (parts.scheme?, parts.authority?);
    // Only when putting the parts together gives what writing the request's
    // path after the URL would.
    if format!("{scheme}://{authority}") != upstream {
        return None;
    }
    let host = http::HeaderValue::from_str(authority.as_str()).ok()?;
    Some(Origin {
        scheme,
        authority,
        host,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RespondBody {
    None,
    Json(Value),
    Text(String),
}

#[derive(Debug, Clone)]
pub enum Action {
    Respond {
        status: u16,
        body: RespondBody,
        headers: Vec<(String, String)>,
    },
    Redirect {
        to: String,
        status: u16,
    },
    Static {
        dir: String,
        spa_fallback: Option<String>,
        index: String,
        cache_control: Option<String>,
    },
    Proxy(Arc<Proxy>),
    Handler(HandlerRef),
    /// The MCP endpoint. [`Project::mcp`] holds its tools.
    Mcp,
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Respond { status, .. } => format!("respond {status}"),
            Action::Redirect { to, status } => format!("redirect {status} → {to}"),
            Action::Static { dir, .. } => format!("static {dir}"),
            Action::Proxy(p) => match &p.runtime {
                Some(name) => format!("proxy runtime {name}"),
                None => format!("proxy {}", p.upstreams.join(", ")),
            },
            Action::Handler(h) => h.raw.clone(),
            Action::Mcp => "mcp".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Config {
        file: String,
        line: Option<usize>,
        index: usize,
    },
    RouteFile {
        path: String,
    },
    OpenApi {
        file: String,
        location: String,
    },
    Defaults,
    /// The `[mcp]` table.
    Mcp {
        file: String,
        line: Option<usize>,
    },
    /// A `[[mcp.tool]]` entry.
    McpTool {
        file: String,
        line: Option<usize>,
        index: usize,
        name: String,
    },
}

impl Source {
    /// A location for diagnostics: for config entries, points at the
    /// `[[route]]` table so field names read naturally after it.
    pub fn loc(&self) -> String {
        match self {
            Source::Config {
                file,
                line: Some(line),
                ..
            } => format!("{file}:{line} [[route]]"),
            Source::Config { file, index, .. } => format!("{file} route[{index}]"),
            Source::Mcp {
                file,
                line: Some(line),
            } => format!("{file}:{line} [mcp]"),
            Source::Mcp { file, line: None } => format!("{file} [mcp]"),
            Source::McpTool {
                file,
                line: Some(line),
                ..
            } => format!("{file}:{line} [[mcp.tool]]"),
            Source::McpTool { file, index, .. } => format!("{file} mcp.tool[{index}]"),
            other => other.to_string(),
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Config {
                file,
                line: Some(line),
                ..
            } => write!(f, "{file}:{line}"),
            Source::Config { file, index, .. } => write!(f, "{file} route[{index}]"),
            Source::RouteFile { path } => write!(f, "{path}"),
            Source::OpenApi { file, location } => write!(f, "{file}:{location}"),
            Source::Defaults => write!(f, "[defaults]"),
            Source::Mcp {
                file,
                line: Some(line),
            }
            | Source::McpTool {
                file,
                line: Some(line),
                ..
            } => write!(f, "{file}:{line}"),
            Source::Mcp { file, line: None } => write!(f, "{file} [mcp]"),
            Source::McpTool { file, index, .. } => write!(f, "{file} mcp.tool[{index}]"),
        }
    }
}

/// Operation details kept for OpenAPI output.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OpInfo {
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
}

/// A second handler that gets a copy of a route's requests, to compare
/// its answers with the route's own. See `docs/shadow.md`.
#[derive(Debug, Clone)]
pub struct Shadow {
    pub handler: HandlerRef,
    /// One request in this many is copied.
    pub every: u64,
    seen: Arc<std::sync::atomic::AtomicU64>,
}

impl Shadow {
    /// Whether this request is one to copy.
    pub fn take(&self) -> bool {
        self.seen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .is_multiple_of(self.every)
    }
}

#[derive(Debug, Clone)]
pub struct Route {
    pub id: usize,
    /// Uppercase methods, or `None` for all methods.
    pub methods: Option<Vec<String>>,
    pub path: String,
    pub host: Option<String>,
    pub action: Action,
    pub source: Source,
    pub validation: Validation,
    /// What applies before any policy entry: defaults and the OpenAPI
    /// spec.
    pub low: Layer,
    /// The route's own policies, which count as an entry for the route's
    /// path and methods (see [`Width`]).
    pub high: Layer,
    /// The layers `low` and `high` are made of, in merge order, for
    /// `unsworn routes --policies`. The first `low_layers` make up `low`.
    pub layers: Vec<(String, Layer)>,
    pub low_layers: usize,
    /// The policy entries that can apply to a request this route serves,
    /// as indexes into [`Project::entries`], widest first. Those in
    /// `wider` merge before the route's own policies and those in
    /// `narrower` after them.
    wider: Vec<usize>,
    narrower: Vec<usize>,
    pub param_names: Vec<String>,
    pub catch_all: Option<String>,
    /// The path's segments.
    segs: Vec<Seg>,
    pub op: Option<OpInfo>,
    pub shadow: Option<Shadow>,
    /// The paths whose cached responses a successful request to this
    /// route makes out of date. See [`Invalidates`].
    pub invalidates: Vec<Invalidates>,
}

/// A path a route invalidates, as `invalidates` writes it:
/// `/users/{id}`, with each parameter filled in from the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalidates {
    pub path: String,
    segs: Vec<Seg>,
}

impl Invalidates {
    /// The path this stands for in a request with these path parameters,
    /// as [`crate::store::path_key`] writes one. `None` when the request
    /// lacks one of them.
    pub fn key(&self, params: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        let mut segments: Vec<String> = Vec::new();
        for seg in &self.segs {
            match seg {
                Seg::Lit(text) => segments.push(text.clone()),
                Seg::Param(name) => segments.push(params(name)?),
                // The rest of a path is segments of its own.
                Seg::CatchAll(name) => segments.extend(
                    params(name)?
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                ),
            }
        }
        Some(crate::store::path_key(segments.iter().map(String::as_str)))
    }
}

impl Route {
    pub fn handler(&self) -> Option<&HandlerRef> {
        match &self.action {
            Action::Handler(h) => Some(h),
            _ => None,
        }
    }

    /// The tool name, for the hidden route of an MCP tool. Such routes are
    /// never matched by path; the MCP endpoint dispatches to them.
    pub fn mcp_tool(&self) -> Option<&str> {
        match &self.source {
            Source::McpTool { name, .. } => Some(name),
            _ => None,
        }
    }

    pub fn allows(&self, method: &str) -> bool {
        match &self.methods {
            None => true,
            Some(m) => m.iter().any(|x| x == method),
        }
    }

    pub fn method_label(&self) -> String {
        match &self.methods {
            None => "*".into(),
            Some(m) => m.join(","),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seg {
    Lit(String),
    Param(String),
    CatchAll(String),
}

pub(crate) fn parse_pattern(pattern: &str) -> Vec<Seg> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Some(name) = s.strip_prefix("{*").and_then(|s| s.strip_suffix('}')) {
                Seg::CatchAll(name.to_string())
            } else if let Some(name) = s
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .filter(|n| !n.starts_with('{'))
            {
                Seg::Param(name.to_string())
            } else {
                Seg::Lit(s.replace("{{", "{").replace("}}", "}"))
            }
        })
        .collect()
}

fn normalize(pattern: &str) -> String {
    normalize_segs(&parse_pattern(pattern))
}

/// A pattern without its parameters' names, which don't change what it
/// matches.
fn normalize_segs(segs: &[Seg]) -> String {
    let segs: Vec<&str> = segs
        .iter()
        .map(|s| match s {
            Seg::Lit(l) => l.as_str(),
            Seg::Param(_) => "{}",
            Seg::CatchAll(_) => "{*}",
        })
        .collect();
    format!("/{}", segs.join("/"))
}

/// Matches `path` against segments ending in a catch-all. Returns the
/// param values in order, the last one being the rest of the path.
fn match_prefix(segs: &[Seg], path: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut values = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        match seg {
            Seg::CatchAll(_) => {
                values.push(parts.get(i..).map(|p| p.join("/")).unwrap_or_default());
                return Some(values);
            }
            Seg::Lit(l) => {
                if parts.get(i) != Some(&l.as_str()) {
                    return None;
                }
            }
            Seg::Param(_) => values.push((*parts.get(i)?).to_string()),
        }
    }
    (parts.len() == segs.len()).then_some(values)
}

/// Whether `path` matches a pattern, which may end in a catch-all. A
/// prefix matches its own path too: `/x/{*rest}` matches `/x`.
fn pattern_matches(segs: &[Seg], path: &str) -> bool {
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    for seg in segs {
        match seg {
            Seg::CatchAll(_) => return true,
            Seg::Lit(l) => {
                if parts.next() != Some(l.as_str()) {
                    return false;
                }
            }
            Seg::Param(_) => {
                if parts.next().is_none() {
                    return false;
                }
            }
        }
    }
    parts.next().is_none()
}

/// A pattern's segments without a catch-all at the end, and whether it had
/// one.
fn fixed(segs: &[Seg]) -> (&[Seg], bool) {
    match segs.split_last() {
        Some((Seg::CatchAll(_), rest)) => (rest, true),
        _ => (segs, false),
    }
}

/// Whether an entry's pattern matches every path a route's pattern does.
/// `/{*rest}` and `/{team}/{*rest}` cover the route `/app/{*rest}`, and
/// `/users/{id}` covers the route `/users/me`. `/app/admin/{*rest}` covers
/// only some of what the route `/app/{*rest}` serves, and so does
/// `/admin/{*rest}` for the route `/{team}/{*rest}`.
fn covers(entry: &[Seg], route: &[Seg]) -> bool {
    let (entry, entry_prefix) = fixed(entry);
    let (route, route_prefix) = fixed(route);
    let lengths = match entry_prefix {
        true => entry.len() <= route.len(),
        false => !route_prefix && entry.len() == route.len(),
    };
    lengths
        && entry.iter().zip(route).all(|pair| match pair {
            (Seg::Param(_), _) => true,
            (Seg::Lit(a), Seg::Lit(b)) => a == b,
            _ => false,
        })
}

/// Whether some path matches both patterns.
fn overlaps(a: &[Seg], b: &[Seg]) -> bool {
    let (a, a_prefix) = fixed(a);
    let (b, b_prefix) = fixed(b);
    let lengths = match (a_prefix, b_prefix) {
        (true, true) => true,
        (true, false) => a.len() <= b.len(),
        (false, true) => b.len() <= a.len(),
        (false, false) => a.len() == b.len(),
    };
    lengths
        && a.iter()
            .zip(b)
            .all(|pair| !matches!(pair, (Seg::Lit(x), Seg::Lit(y)) if x != y))
}

/// The methods a list of them covers. `GET` covers `HEAD` too, since a
/// `GET` route answers `HEAD`: otherwise `HEAD` would reach the route
/// without the policies written for `GET`.
fn covered_methods(methods: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = methods.iter().map(String::as_str).collect();
    if out.contains(&"GET") && !out.contains(&"HEAD") {
        out.push("HEAD");
    }
    out
}

/// How many requests a statement about a path and its methods covers,
/// as a place in an order: a wider statement sorts before a narrower one.
///
/// Policies merge in this order, so the narrowest statement that sets a
/// key decides it. The route's own policies are a statement about the
/// route's path and methods. In turn:
///
/// - a prefix (`/api/{*rest}`) is wider than a full path
/// - a shorter path is wider than a longer one
/// - at the first segment where two paths differ in kind, a parameter is
///   wider than a literal (`/users/{id}` is wider than `/users/me`), which
///   is the order routes are matched in
/// - every method is wider than some methods, and more methods are wider
///   than fewer
///
/// Statements that sort the same and both apply to a request cover the
/// same requests, or overlap without one holding the other. They can't
/// set the same key: see `conflicts`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Width {
    full_path: bool,
    segments: usize,
    literals: Vec<bool>,
    fewer_methods: usize,
}

impl Width {
    fn of(segs: &[Seg], methods: Option<&[String]>) -> Width {
        let (segs, prefix) = fixed(segs);
        Width {
            full_path: !prefix,
            segments: segs.len(),
            literals: segs.iter().map(|s| matches!(s, Seg::Lit(_))).collect(),
            fewer_methods: match methods {
                None => 0,
                Some(list) => usize::MAX - covered_methods(list).len(),
            },
        }
    }
}

/// A `[[route]]` entry with policies and no action: for one path
/// (`/users/{id}`) or, ending in `{*rest}`, for every path under a prefix.
/// It applies to the requests it matches, whichever route serves them.
#[derive(Debug, Clone)]
pub struct PolicyEntry {
    pub(crate) segs: Vec<Seg>,
    pub path: String,
    pub methods: Option<Vec<String>>,
    pub layer: Layer,
    pub source: Source,
}

impl PolicyEntry {
    /// Whether the entry is for a prefix: its path ends in `{*rest}`.
    pub fn is_prefix(&self) -> bool {
        fixed(&self.segs).1
    }

    /// Whether the entry is for `method`.
    pub fn covers_method(&self, method: &str) -> bool {
        self.methods
            .as_ref()
            .is_none_or(|m| covered_methods(m).contains(&method))
    }

    /// Whether the entry is for this request.
    pub fn matches(&self, method: &str, path: &str) -> bool {
        self.covers_method(method) && pattern_matches(&self.segs, path)
    }

    fn width(&self) -> Width {
        Width::of(&self.segs, self.methods.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoreConfig {
    pub backend: config::StoreBackend,
    pub url: Option<String>,
    /// The token for a Redis REST API, when `url` is one.
    #[serde(skip)]
    pub token: Option<String>,
    /// Prepended to every key. Empty by default.
    pub prefix: String,
    pub on_error: config::OnError,
}

impl StoreConfig {
    /// Whether `url` is a Redis REST API (Upstash's protocol) instead of a
    /// Redis server. Functions use one; the binary speaks Redis itself.
    pub fn is_rest_url(url: &str) -> bool {
        url.starts_with("https://") || url.starts_with("http://")
    }

    /// Whether the store is Redis behind a REST API.
    pub fn is_rest(&self) -> bool {
        self.backend == config::StoreBackend::Redis
            && self.url.as_deref().is_some_and(Self::is_rest_url)
    }
}

/// A handler that other handlers call by name, from any runtime. It has
/// no path, so nothing outside reaches it.
#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub handler: HandlerRef,
    /// Checked before the function runs.
    pub input: Option<BodyRule>,
    /// Checked against what it answers with, as `response_validation` says.
    pub output: Option<BodyRule>,
    /// The longest a call may take.
    pub timeout: Duration,
}

/// A service that Unsworn calls for handlers (`[outbound.<name>]`), adding
/// its credential, so a handler never holds it.
#[derive(Debug, Clone)]
pub struct Outbound {
    pub name: String,
    /// Without a trailing slash. A handler's path goes after it.
    pub url: String,
    /// Added to every request, over anything a handler sets by the same
    /// name. Lowercase names. The values are secrets.
    pub headers: Vec<(String, String)>,
    /// Uppercase methods a handler may use, or `None` for any.
    pub methods: Option<Vec<String>>,
    /// Path patterns a handler may ask for, or `None` for any.
    pub paths: Option<Vec<String>>,
    path_segs: Vec<Vec<Seg>>,
    pub timeout: Duration,
    /// The largest response body, in bytes.
    pub max_response: u64,
}

impl Outbound {
    /// A service at `url` that any method and path may be sent to, with
    /// these headers added.
    pub fn open(name: &str, url: &str, headers: Vec<(String, String)>) -> Outbound {
        Outbound {
            name: name.to_string(),
            url: url.trim_end_matches('/').to_string(),
            headers,
            methods: None,
            paths: None,
            path_segs: Vec::new(),
            timeout: Duration::from_secs(30),
            max_response: 10 * 1024 * 1024,
        }
    }

    /// Whether a handler may send `method` to `path` (which may carry a
    /// query). `Err` says why not.
    pub fn allows(&self, method: &str, path: &str) -> Result<(), String> {
        let path = path.split_once('?').map_or(path, |(path, _)| path);
        if !path.starts_with('/') {
            return Err(format!("the path \"{path}\" doesn't start with `/`"));
        }
        // `/v1/../admin` would step outside what `paths`, or a URL with a
        // path of its own, allows. Encoded dots too, which a server may
        // decode before it routes.
        let dotted = path.split('/').any(|segment| {
            let segment = segment.to_ascii_lowercase().replace("%2e", ".");
            segment == "." || segment == ".."
        });
        if dotted || path.contains(['\\', '#']) || path.chars().any(char::is_control) {
            return Err(format!(
                "the path \"{path}\" has a `.` or `..` segment, a backslash or a `#`"
            ));
        }
        if let Some(methods) = &self.methods
            && !methods.iter().any(|m| m == method)
        {
            return Err(format!(
                "[outbound.{}] doesn't allow {method} (it allows {})",
                self.name,
                methods.join(", ")
            ));
        }
        if self.paths.is_some()
            && !self
                .path_segs
                .iter()
                .any(|segs| match_prefix(segs, path).is_some())
        {
            return Err(format!(
                "[outbound.{}] doesn't allow the path {path}",
                self.name
            ));
        }
        Ok(())
    }
}

/// The path spans are POSTed to, below the telemetry service's URL.
pub const TRACES_PATH: &str = "/v1/traces";

/// The path metrics are POSTed to, below the telemetry service's URL.
pub const METRICS_PATH: &str = "/v1/metrics";
/// The path logs are POSTed to, below the telemetry service's URL.
pub const LOGS_PATH: &str = "/v1/logs";

/// Where spans go (`[telemetry]`). The server sends them; see
/// `docs/telemetry-design.md`.
#[derive(Debug, Clone)]
pub struct Telemetry {
    /// The service they're sent to, which allows `POST` to
    /// [`TRACES_PATH`].
    pub outbound: Arc<Outbound>,
    /// `None` leaves the name to the host.
    pub service_name: Option<String>,
    /// The share of requests that are recorded, from 0 to 1, among those
    /// whose sampled flag isn't taken: see
    /// [`decide`](crate::telemetry::decide).
    pub sample: f64,
    /// The server's metrics go there too, with `POST` to [`METRICS_PATH`].
    pub metrics: bool,
    /// Its logs go there too, with `POST` to [`LOGS_PATH`].
    pub logs: bool,
}

/// One file of typed request models to write; see [`crate::codegen`].
#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    pub lang: crate::codegen::Lang,
    /// Relative to the project directory.
    pub file: String,
    pub options: crate::codegen::Options,
}

#[derive(Debug, Clone, Default)]
pub struct OpenApiMeta {
    pub title: Option<String>,
    pub version: Option<String>,
    pub spec_file: Option<String>,
    pub spec: Option<Value>,
}

#[derive(Debug, Clone, Copy)]
struct SlotEntry {
    route: usize,
    /// Inserted for a catch-all's base path, where the rest is empty.
    fallback: bool,
}

/// A compiled project: everything needed to serve requests.
pub struct Project {
    pub config_file: String,
    pub server: Server,
    pub runtimes: BTreeMap<String, Arc<Runtime>>,
    pub schemes: BTreeMap<String, Arc<Scheme>>,
    pub store: StoreConfig,
    pub routes: Vec<Route>,
    /// The policy-only `[[route]]` entries, widest first (see [`Width`]).
    pub entries: Vec<PolicyEntry>,
    /// Every path some route's `invalidates` names, as a pattern, so a
    /// cached response is only checked for a mark when one could exist.
    invalidated: Vec<Vec<Seg>>,
    /// The longest any cached response can live: how long an invalidation
    /// mark is kept.
    cache_ttl_max: Duration,
    pub openapi: OpenApiMeta,
    /// The MCP endpoint, when `[mcp]` is set.
    pub mcp: Option<McpEndpoint>,
    /// `[[function]]` entries, by name.
    pub functions: BTreeMap<String, Arc<Function>>,
    /// `[outbound.<name>]` entries.
    pub outbound: BTreeMap<String, Arc<Outbound>>,
    /// `[telemetry]`, when it's set.
    pub telemetry: Option<Telemetry>,
    /// `[dimedb]`, when it's set.
    pub dimedb: Option<Arc<crate::dimedb::DimeDbConfig>>,
    /// The files of generated types `[gen]` asks for.
    pub generated: Vec<Generated>,
    /// Files read while loading, for watching in dev mode.
    pub files: Vec<String>,
    pub routes_dir: Option<String>,
    /// The environment variables `[auth]` and `[store]` read credentials
    /// from: signing secrets, API keys, a verifier's headers, the store's
    /// URL and token. A handler has no use for them unless its runtime's
    /// `env` passes them on, and one that could read the signing secret
    /// could mint its own tokens.
    pub secret_env: Vec<String>,
    routers: HashMap<Option<String>, matchit::Router<usize>>,
    slots: Vec<Vec<SlotEntry>>,
    /// Catch-all routes, longest prefix first, with their segments.
    catch_alls: Vec<(usize, Vec<Seg>)>,
}

impl fmt::Debug for Project {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Project")
            .field("config_file", &self.config_file)
            .field("routes", &self.routes.len())
            .finish()
    }
}

/// The result of matching a request.
#[derive(Debug)]
pub enum MatchResult<'a> {
    Found(Matched<'a>),
    MethodNotAllowed(Vec<String>),
    NotFound,
}

#[derive(Debug)]
pub struct Matched<'a> {
    pub route: &'a Route,
    /// Raw path parameter values, by name.
    pub params: Vec<(String, String)>,
}

impl Project {
    pub fn match_request(&self, host: Option<&str>, method: &str, path: &str) -> MatchResult<'_> {
        let host = host.map(host_name);
        let mut allowed: Vec<String> = Vec::new();
        let candidates = [host, None];
        for (i, key) in candidates.iter().enumerate() {
            if i == 1 && candidates[0].is_none() {
                break;
            }
            let Some(router) = self.routers.get(key) else {
                continue;
            };
            let Ok(m) = router.at(path) else {
                continue;
            };
            let values: Vec<String> = m.params.iter().map(|(_, v)| v.to_string()).collect();
            let slot = &self.slots[*m.value];
            for want in [method, if method == "HEAD" { "GET" } else { "" }] {
                if want.is_empty() {
                    continue;
                }
                if let Some(entry) = slot.iter().find(|e| self.routes[e.route].allows(want)) {
                    let route = &self.routes[entry.route];
                    let mut values = values.clone();
                    if entry.fallback {
                        values.push(String::new());
                    }
                    return MatchResult::Found(Matched {
                        route,
                        params: route.param_names.iter().cloned().zip(values).collect(),
                    });
                }
            }
            for e in slot {
                match &self.routes[e.route].methods {
                    Some(ms) => allowed.extend(ms.iter().cloned()),
                    None => allowed.push("*".into()),
                }
            }
        }
        // The path matched, but not the method: a catch-all may still take it.
        for (id, segs) in &self.catch_alls {
            let route = &self.routes[*id];
            if !route.allows(method) {
                continue;
            }
            if route.host.is_some() {
                continue;
            }
            if let Some(values) = match_prefix(segs, path) {
                return MatchResult::Found(Matched {
                    route,
                    params: route.param_names.iter().cloned().zip(values).collect(),
                });
            }
        }
        if allowed.is_empty() {
            MatchResult::NotFound
        } else {
            // A route for `GET` answers `HEAD` too.
            if allowed.iter().any(|m| m == "GET") {
                allowed.push("HEAD".into());
            }
            allowed.sort();
            allowed.dedup();
            MatchResult::MethodNotAllowed(allowed)
        }
    }

    /// The merged policies for a request to `route`.
    pub fn effective(&self, route: &Route, method: &str, path: &str) -> Effective {
        merged_layer(route, &self.entries, method, path)
            .finalize(self.server.timeout, self.server.max_body)
    }

    /// The merged policies for a call to an MCP tool.
    ///
    /// For `[[mcp.tool]]`: `[defaults]`, prefix entries for the endpoint
    /// path, `[mcp]`, then the tool's own. For a route offered as a tool: the
    /// route's policies as a REST request sees them, with `[mcp]`'s `before`
    /// handlers first, and `[mcp]`'s `timeout` unless the route sets one.
    pub fn tool_effective(&self, tool: &McpTool) -> Effective {
        self.tool_effective_at(tool, None)
    }

    /// [`Project::tool_effective`] for one call to a route offered as a
    /// tool, whose arguments make the request for `path`. An entry for
    /// part of the route's paths applies to the calls that land there, as
    /// it does to a REST request for the same path.
    pub fn tool_effective_at(&self, tool: &McpTool, path: Option<&str>) -> Effective {
        let route = &self.routes[tool.route];
        let Some(b) = &tool.binding else {
            return self.route_effective(route);
        };
        let probe = b.pattern.replace("{*", "{");
        let path = path.unwrap_or(&probe);
        let layer = merged_layer(route, &self.entries, &b.method, path);
        let mut eff = layer.finalize(self.server.timeout, self.server.max_body);
        if let Some(mcp) = &self.mcp {
            let endpoint = merged_layer(&self.routes[mcp.route], &self.entries, "POST", &mcp.path);
            if layer.timeout.is_none()
                && let Some(t) = endpoint.timeout
            {
                eff.timeout = t;
            }
            let mut before = endpoint
                .finalize(self.server.timeout, self.server.max_body)
                .before;
            for h in eff.before {
                if !before.contains(&h) {
                    before.push(h);
                }
            }
            eff.before = before;
        }
        eff
    }

    /// The MCP tool a route is offered as, if any.
    pub fn route_tool(&self, route_id: usize) -> Option<&McpTool> {
        self.mcp
            .as_ref()?
            .tools
            .iter()
            .find(|t| t.binding.is_some() && t.route == route_id)
    }

    /// A route's policies as a typical request sees them, for display and
    /// checks: `GET` on its own path, or a call through the MCP endpoint
    /// for a tool.
    pub fn route_effective(&self, route: &Route) -> Effective {
        match (route.mcp_tool(), &self.mcp) {
            (Some(_), Some(mcp)) => self.effective(route, "POST", &mcp.path),
            _ => self.effective(route, "GET", &route.path.replace("{*", "{")),
        }
    }

    /// Whether a route's handler gets its request body as a stream.
    pub fn streams_body(&self, route: &Route) -> bool {
        // Tool arguments are always read and validated first.
        if !matches!(route.action, Action::Handler(_)) || route.mcp_tool().is_some() {
            return false;
        }
        let probe = route.path.replace("{*", "{");
        let methods: Vec<&str> = match &route.methods {
            Some(list) => list.iter().map(String::as_str).collect(),
            None => vec!["POST"],
        };
        methods
            .iter()
            .any(|m| self.effective(route, m, &probe).stream_body)
    }

    /// Handlers whose routes stream request bodies, by runtime.
    pub fn stream_handlers_by_runtime(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in self.routes.iter().filter(|r| self.streams_body(r)) {
            if let Some(h) = r.handler() {
                let list = out.entry(h.runtime.clone()).or_default();
                if !list.contains(&h.raw) {
                    list.push(h.raw.clone());
                }
            }
        }
        out
    }

    /// The policy entries that can apply to a request `route` serves, in
    /// the order they merge, for display: those wider than the route's own
    /// policies, then those narrower. An entry for some of the route's
    /// paths or methods only is marked `partial`.
    pub fn entries_for(&self, route: &Route) -> (Vec<Applied<'_>>, Vec<Applied<'_>>) {
        let applied = |ids: &[usize]| -> Vec<Applied<'_>> {
            ids.iter()
                .map(|&i| &self.entries[i])
                .filter(|e| !self.served_elsewhere(route, e))
                .map(|entry| Applied {
                    entry,
                    partial: !(covers(&entry.segs, &route.segs)
                        && route_methods(route).iter().all(|m| entry.covers_method(m))),
                })
                .collect()
        };
        (applied(&route.wider), applied(&route.narrower))
    }

    /// Whether a request for `method` and `path` goes to `route`.
    pub(crate) fn serves(&self, route: &Route, method: &str, path: &str) -> bool {
        matches!(
            self.match_request(route.host.as_deref(), method, path),
            MatchResult::Found(found) if found.route.id == route.id
        )
    }

    /// Whether another route serves every request `entry` and `route`
    /// have in common, so the entry never applies to one `route` serves.
    fn served_elsewhere(&self, route: &Route, entry: &PolicyEntry) -> bool {
        if route.mcp_tool().is_some() {
            return false;
        }
        let Some(shared) = intersect(&route.segs, &entry.segs) else {
            return true;
        };
        let path = probe(&shared);
        // The methods both are for: the entry's, on a route that takes
        // every method.
        let methods: Vec<&str> = match (&route.methods, &entry.methods) {
            (None, None) => vec!["*"],
            (None, Some(entry)) => covered_methods(entry),
            (Some(route), _) => route
                .iter()
                .map(String::as_str)
                .filter(|m| entry.covers_method(m))
                .collect(),
        };
        methods.iter().all(|m| !self.serves(route, m, &path))
    }

    /// Every handler reference, by runtime.
    pub fn handlers_by_runtime(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut add = |h: &HandlerRef| {
            let list = out.entry(h.runtime.clone()).or_default();
            if !list.contains(&h.raw) {
                list.push(h.raw.clone());
            }
        };
        for r in &self.routes {
            if let Some(h) = r.handler() {
                add(h);
            }
            if let Some(s) = &r.shadow {
                add(&s.handler);
            }
            for layer in [&r.low, &r.high] {
                if let Some(OrOff::On(list)) = &layer.before {
                    list.iter().for_each(&mut add);
                }
            }
        }
        for p in &self.entries {
            if let Some(OrOff::On(list)) = &p.layer.before {
                list.iter().for_each(&mut add);
            }
        }
        for f in self.functions.values() {
            add(&f.handler);
        }
        out
    }

    /// Whether some route's `invalidates` could name this request path, so
    /// that a response cached for it may have been marked out of date.
    pub fn may_be_invalidated(&self, path: &str) -> bool {
        self.invalidated
            .iter()
            .any(|segs| pattern_matches(segs, path))
    }

    /// How long an invalidation mark is kept: as long as the longest-lived
    /// cached response it could be about.
    pub fn invalidation_ttl(&self) -> Duration {
        self.cache_ttl_max
    }

    /// Whether one of the schemes in `auth` checks a signature made over
    /// the request's body, which then has to be read before the caller is
    /// known.
    pub fn signs_body(&self, auth: &AuthReq) -> bool {
        signs_body(&self.schemes, auth)
    }

    /// The headers that carry credentials for the schemes in `auth`.
    pub fn credential_headers(&self, auth: &AuthReq) -> Vec<String> {
        credential_headers(&self.schemes, auth)
    }

    /// Paths served for OAuth protected-resource metadata, with the document.
    pub fn protected_resource_docs(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for s in self.schemes.values() {
            if let Scheme::Jwt(jwt) = s.as_ref()
                && let Some(pr) = &jwt.protected_resource
            {
                for path in pr.well_known_paths() {
                    if !out.iter().any(|(p, _)| p == &path) {
                        out.push((path, pr.metadata()));
                    }
                }
            }
        }
        out
    }
}

/// A policy entry among those that can apply to a route's requests.
pub struct Applied<'a> {
    pub entry: &'a PolicyEntry,
    /// It's for some of the route's paths or methods, not all of them.
    pub partial: bool,
}

/// The methods to look at a route's policies for: its own, or `*` for one
/// that takes every method.
fn route_methods(route: &Route) -> Vec<&str> {
    match &route.methods {
        Some(list) => list.iter().map(String::as_str).collect(),
        None => vec!["*"],
    }
}

/// A path that stands for the paths a pattern matches and no narrower
/// pattern does: a parameter is a segment no literal equals, and a
/// catch-all is more of them than an entry for one exact path would have.
pub(crate) fn probe(segs: &[Seg]) -> String {
    let parts: Vec<String> = segs
        .iter()
        .map(|s| match s {
            Seg::Lit(l) => l.clone(),
            Seg::Param(n) => format!("{{{n}}}"),
            Seg::CatchAll(n) => vec![format!("{{{n}}}"); 8].join("/"),
        })
        .collect();
    format!("/{}", parts.join("/"))
}

/// The pattern for the paths two patterns both match, if any.
pub(crate) fn intersect(a: &[Seg], b: &[Seg]) -> Option<Vec<Seg>> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        match (a.get(i), b.get(i)) {
            (None, None) => return Some(out),
            // A catch-all takes whatever the other pattern has left, which
            // may be nothing.
            (Some(Seg::CatchAll(_)), _) => {
                out.extend_from_slice(&b[i.min(b.len())..]);
                return Some(out);
            }
            (_, Some(Seg::CatchAll(_))) => {
                out.extend_from_slice(&a[i.min(a.len())..]);
                return Some(out);
            }
            (None, Some(_)) | (Some(_), None) => return None,
            (Some(Seg::Lit(x)), Some(Seg::Lit(y))) => {
                if x != y {
                    return None;
                }
                out.push(Seg::Lit(x.clone()));
            }
            (Some(Seg::Lit(x)), Some(Seg::Param(_))) | (Some(Seg::Param(_)), Some(Seg::Lit(x))) => {
                out.push(Seg::Lit(x.clone()));
            }
            (Some(Seg::Param(x)), Some(Seg::Param(_))) => out.push(Seg::Param(x.clone())),
        }
        i += 1;
    }
}

/// A route's policies merged for one request, before defaults are filled
/// in: what applies to every request, then each statement about this one
/// from the widest to the narrowest (see [`Width`]), the route's own
/// policies among them.
fn merged_layer(route: &Route, entries: &[PolicyEntry], method: &str, path: &str) -> Layer {
    let mut layer = route.low.clone();
    let apply = |ids: &[usize], layer: &mut Layer| {
        for &i in ids {
            let entry = &entries[i];
            if entry.matches(method, path) {
                layer.merge_from(&entry.layer);
            }
        }
    };
    apply(&route.wider, &mut layer);
    layer.merge_from(&route.high);
    apply(&route.narrower, &mut layer);
    layer
}

/// Gives each route the policy entries that can apply to its requests.
/// An `[[mcp.tool]]` has no path of its own: its calls arrive at the MCP
/// endpoint, so the entries for that path apply, all of them before the
/// tool's own policies.
fn link_entries(routes: &mut [Route], entries: &[PolicyEntry], mcp_path: Option<&str>) {
    let endpoint = mcp_path.map(parse_pattern);
    for route in routes {
        let tool = route.mcp_tool().is_some();
        let segs = match (&endpoint, tool) {
            (Some(endpoint), true) => endpoint,
            _ => &route.segs,
        };
        let own = Width::of(&route.segs, route.methods.as_deref());
        route.wider.clear();
        route.narrower.clear();
        for (i, entry) in entries.iter().enumerate() {
            if !overlaps(&entry.segs, segs) {
                continue;
            }
            if tool || entry.width() <= own {
                route.wider.push(i);
            } else {
                route.narrower.push(i);
            }
        }
    }
}

/// The result of compiling: a project when there are no errors, plus all
/// diagnostics (errors and warnings).
pub struct Compiled {
    pub project: Option<Project>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Loads and compiles a project from its config file.
pub fn compile(host: &dyn Host, config_file: &str) -> Compiled {
    compile_with(host, config_file, Build::Now)
}

/// [`compile`], choosing when validation schemas are built. Pass
/// [`Build::OnFirstUse`] only for a config that `compile` already accepted.
pub fn compile_with(host: &dyn Host, config_file: &str, schemas: Build) -> Compiled {
    compile_inner(host, config_file, schemas, false)
}

/// [`compile`], leaving every `{env.NAME}` as written instead of reading
/// the variable. The result describes the project's rules on any machine,
/// which is what [`crate::plan`] needs: it doesn't depend on the
/// environment, and no variable has to be set. It can't serve requests.
pub fn compile_symbolic(host: &dyn Host, config_file: &str) -> Compiled {
    compile_inner(host, config_file, Build::Now, true)
}

fn compile_inner(host: &dyn Host, config_file: &str, schemas: Build, symbolic: bool) -> Compiled {
    let mut c = Compiler {
        host,
        schemas,
        symbolic,
        config_file: config_file.to_string(),
        diags: Diagnostics::default(),
        runtimes: BTreeMap::new(),
        schemes: BTreeMap::new(),
        sets: BTreeMap::new(),
        files: vec![config_file.to_string()],
        route_tools: Vec::new(),
        secret_env: std::collections::BTreeSet::new(),
    };
    let project = c.run();
    let mut diagnostics = c.diags.items;
    diagnostics.sort_by_key(|d| d.level);
    Compiled {
        project: if diagnostics.iter().any(Diagnostic::is_error) {
            None
        } else {
            project
        },
        diagnostics,
    }
}

struct Compiler<'h> {
    host: &'h dyn Host,
    /// Leave `{env.NAME}` in place; see [`compile_symbolic`].
    symbolic: bool,
    schemas: Build,
    config_file: String,
    diags: Diagnostics,
    runtimes: BTreeMap<String, Arc<Runtime>>,
    schemes: BTreeMap<String, Arc<Scheme>>,
    sets: BTreeMap<String, Layer>,
    files: Vec<String>,
    /// Routes to offer as MCP tools: route id, declaration, where it was
    /// declared, and whether `[openapi] mcp_tools` picked it in bulk.
    route_tools: Vec<(usize, RawRouteTool, String, bool)>,
    /// Environment variables that credentials were read from.
    secret_env: std::collections::BTreeSet<String>,
}

/// An action-less `[[route]]` entry with an exact path.
struct ExactEntry {
    path: String,
    norm: String,
    methods: Option<Vec<String>>,
    layer: Layer,
    validation: RawValidation,
    source: Source,
    used: bool,
    /// `mcp = {...}`: offer the route this entry matches as a tool.
    mcp: Option<RawRouteTool>,
    /// `shadow`, for the route this entry matches.
    shadow: Option<config::RawShadow>,
    /// `invalidates`, for the route this entry matches.
    invalidates: Option<Vec<String>>,
}

#[derive(Default, Clone)]
struct RawValidation {
    params: Option<Vec<ParamSpec>>,
    query: Option<Vec<ParamSpec>>,
    headers: Option<Vec<ParamSpec>>,
    body: Option<(Value, bool)>,
    response: Option<Value>,
}

impl RawValidation {
    fn overlay(&mut self, other: &RawValidation) {
        if other.params.is_some() {
            self.params = other.params.clone();
        }
        if other.query.is_some() {
            self.query = other.query.clone();
        }
        if other.headers.is_some() {
            self.headers = other.headers.clone();
        }
        if other.body.is_some() {
            self.body = other.body.clone();
        }
        if other.response.is_some() {
            self.response = other.response.clone();
        }
    }
}

/// A route before policies and validation are compiled.
struct Draft {
    methods: Option<Vec<String>>,
    path: String,
    host: Option<String>,
    action: Action,
    source: Source,
    own: Layer,
    /// Layers merged just below `own`, such as `[mcp]` for a tool.
    inherit: Vec<(String, Layer)>,
    validation: RawValidation,
    openapi_layer: Option<Layer>,
    op: Option<OpInfo>,
    /// `mcp = {...}` on the route's own `[[route]]` entry, or an OpenAPI
    /// operation picked by `[openapi] mcp_tools` (the flag).
    mcp: Option<(RawRouteTool, bool)>,
    /// `shadow` on the route's own `[[route]]` entry.
    shadow: Option<config::RawShadow>,
    /// `invalidates` on the route's own `[[route]]` entry.
    invalidates: Option<Vec<String>>,
}

/// The OpenAPI operations `[openapi] mcp_tools` offers as tools.
enum ToolPick {
    All,
    Tags(Vec<String>),
}

impl ToolPick {
    fn matches(&self, tags: &[String]) -> bool {
        match self {
            ToolPick::All => true,
            ToolPick::Tags(want) => tags.iter().any(|t| want.contains(t)),
        }
    }
}

/// Line numbers of the `[mcp]` table and its `[[mcp.tool]]` entries.
#[derive(Default)]
struct McpLines {
    table: Option<usize>,
    tools: Vec<usize>,
}

/// An `[mcp]` table whose routes are drafted but not yet compiled.
struct McpPending {
    endpoint: McpEndpoint,
    /// Tools without their route ids, in config order.
    tools: Vec<McpTool>,
}

impl McpPending {
    /// Links the endpoint and its tools to their compiled routes.
    fn finish(mut self, routes: &[Route], route_tools: Vec<McpTool>) -> Option<McpEndpoint> {
        self.endpoint.route = routes.iter().find(|r| matches!(r.action, Action::Mcp))?.id;
        self.endpoint.tools.extend(route_tools);
        for mut tool in self.tools {
            if let Some(r) = routes
                .iter()
                .find(|r| r.mcp_tool() == Some(tool.name.as_str()))
            {
                tool.route = r.id;
                self.endpoint.tools.push(tool);
            }
        }
        self.endpoint.tools.sort_by(|a, b| a.name.cmp(&b.name));
        Some(self.endpoint)
    }
}

/// Policies that apply to the MCP endpoint as a whole, so a tool can't set
/// them (not even through a `use` set).
fn endpoint_only_keys(layer: &Layer) -> Vec<&'static str> {
    [
        ("cors", layer.cors.is_some()),
        ("origin", layer.origin.is_some()),
        ("ip", layer.ip.is_some()),
        ("max_body", layer.max_body.is_some()),
        ("response_headers", layer.response_headers.is_some()),
    ]
    .into_iter()
    .filter(|(_, set)| *set)
    .map(|(k, _)| k)
    .collect()
}

impl<'h> Compiler<'h> {
    fn run(&mut self) -> Option<Project> {
        let Some(text) = self.host.read(&self.config_file) else {
            self.diags.push(
                Diagnostic::error(&self.config_file, "config file not found")
                    .hint("create unsworn.toml, or run `unsworn init`"),
            );
            return None;
        };
        let format = Format::from_path(&self.config_file);
        let raw: RawConfig = match config::parse_document(&text, format) {
            Ok(r) => r,
            Err(e) => {
                self.diags.error(&self.config_file, e);
                return None;
            }
        };
        let lines = if format == Format::Toml {
            config::route_lines(&text)
        } else {
            Vec::new()
        };

        let server = self.server(&raw);
        let server_timeout = server.timeout;
        self.runtimes(&raw);
        self.schemes(&raw);
        let store = self.store(&raw);

        // Policy sets can't use other sets.
        for (name, set) in &raw.policies {
            let loc = format!("[policies.{name}]");
            if set.use_sets.is_some() {
                self.diags.push(
                    Diagnostic::error(&loc, "policy sets can't `use` other sets")
                        .hint("list both sets in the route's `use` instead"),
                );
            }
            let layer = self.layer(set, &loc, &loc);
            self.sets.insert(name.clone(), layer);
        }
        let mut defaults = self.layer(&raw.defaults, "[defaults]", "[defaults]");
        self.place_params(&mut defaults, None, "[defaults]");

        let mut drafts: Vec<Draft> = Vec::new();
        let mut exacts: Vec<ExactEntry> = Vec::new();
        let mut entries: Vec<PolicyEntry> = Vec::new();

        for (i, r) in raw.route.iter().enumerate() {
            let source = Source::Config {
                file: self.config_file.clone(),
                line: lines.get(i).copied(),
                index: i + 1,
            };
            self.config_route(r, source, &mut drafts, &mut exacts, &mut entries);
        }

        let dimedb = raw.dimedb.as_ref().and_then(|d| self.compile_dimedb(d));

        let mut mcp_pending = match &raw.mcp {
            Some(m) => {
                let lines = if format == Format::Toml {
                    McpLines {
                        table: config::table_lines(&text, "[mcp]").first().copied(),
                        tools: config::table_lines(&text, "[[mcp.tool]]"),
                    }
                } else {
                    McpLines::default()
                };
                self.mcp(m, &lines, &mut drafts)
            }
            None => None,
        };
        if let (Some(dime), Some(pending)) = (&dimedb, &mut mcp_pending)
            && dime.mcp
            && pending.endpoint.hosted()
        {
            let endpoint_layer = drafts
                .iter()
                .find(|d| matches!(d.action, Action::Mcp))
                .map(|d| (d.source.loc(), d.own.clone()))
                .unwrap_or_else(|| ("[mcp]".into(), Layer::default()));
            self.register_dimedb_mcp_tools(dime, pending, endpoint_layer, &mut drafts);
        }

        let routes_dir = raw
            .routes
            .as_ref()
            .map(|r| r.dir.trim_end_matches('/').to_string());
        if let Some(dir) = &routes_dir {
            self.route_files(dir, &mut drafts);
        }

        let mut openapi_meta = OpenApiMeta::default();
        if let Some(oa) = &raw.openapi {
            openapi_meta.title = oa.title.clone();
            openapi_meta.version = oa.version.clone();
            let pick = self.tool_pick(oa, raw.mcp.as_ref());
            if let Some(spec) = &oa.spec {
                openapi_meta.spec_file = Some(spec.clone());
                openapi_meta.spec =
                    self.openapi(spec, oa.handler.as_deref(), pick.as_ref(), &mut drafts);
            }
        }

        // Duplicate actions for the same method and path.
        let mut seen: HashMap<(Option<String>, String), Vec<(Option<Vec<String>>, String)>> =
            HashMap::new();
        for d in &drafts {
            let key = (d.host.clone(), normalize(&d.path));
            let list = seen.entry(key).or_default();
            for (methods, src) in list.iter() {
                let overlap = match (methods, &d.methods) {
                    (None, _) | (_, None) => true,
                    (Some(a), Some(b)) => a.iter().any(|m| b.contains(m)),
                };
                if overlap {
                    self.diags.push(
                        Diagnostic::error(
                            d.source.to_string(),
                            format!(
                                "{} {} is already defined by {src}",
                                label(&d.methods),
                                d.path
                            ),
                        )
                        .hint("each method and path can have only one action"),
                    );
                }
            }
            list.push((d.methods.clone(), d.source.to_string()));
        }

        let drafts_catch_alls: Vec<Vec<Seg>> = drafts
            .iter()
            .filter(|d| d.host.is_none())
            .map(|d| parse_pattern(&d.path))
            .filter(|segs| matches!(segs.last(), Some(Seg::CatchAll(_))))
            .collect();
        let mut routes = Vec::new();
        for (id, d) in drafts.into_iter().enumerate() {
            if let Some(route) = self.finish_route(id, d, &defaults, &mut exacts) {
                routes.push(route);
            }
        }
        // An entry for one path has to be some route's path, or a path a
        // catch-all serves: otherwise it's most likely a typo.
        for e in &exacts {
            let covered = drafts_catch_alls
                .iter()
                .any(|segs| match_prefix(segs, &e.path).is_some());
            if !e.used && !covered {
                self.diags.push(
                    Diagnostic::error(
                        e.source.loc(),
                        format!("no route matches {} {}", label(&e.methods), e.path),
                    )
                    .hint("policy-only entries must match a route from config, route files or the OpenAPI spec; end the path with `{*rest}` to cover a prefix"),
                );
                continue;
            }
            if !e.used {
                let v = &e.validation;
                if v.params.is_some()
                    || v.query.is_some()
                    || v.headers.is_some()
                    || v.body.is_some()
                    || v.response.is_some()
                {
                    self.diags.error(
                        e.source.loc(),
                        "validation for a path served by a catch-all isn't supported; only policies",
                    );
                }
                if e.mcp.is_some() {
                    self.diags.error(
                        format!("{}.mcp", e.source.loc()),
                        "a path served by a catch-all route can't be an MCP tool on its own",
                    );
                }
            }
            // One that only carries validation has no policies to apply.
            if e.layer.is_empty() {
                continue;
            }
            entries.push(PolicyEntry {
                segs: parse_pattern(&e.path),
                path: e.path.clone(),
                methods: e.methods.clone(),
                layer: e.layer.clone(),
                source: e.source.clone(),
            });
        }
        // Widest first, and as written among entries of the same width.
        entries.sort_by_cached_key(PolicyEntry::width);
        let mcp_path = mcp_pending.as_ref().map(|p| p.endpoint.path.as_str());
        link_entries(&mut routes, &entries, mcp_path);
        self.conflicts(&routes, &entries);

        // What `invalidates` needs at request time.
        let mut invalidated: Vec<Vec<Seg>> = Vec::new();
        for route in &routes {
            for target in &route.invalidates {
                if !invalidated.contains(&target.segs) {
                    invalidated.push(target.segs.clone());
                }
            }
        }
        let cache_ttl_max = routes
            .iter()
            .flat_map(|r| [&r.low, &r.high])
            .chain(entries.iter().map(|e| &e.layer))
            .filter_map(|layer| match &layer.cache {
                Some(OrOff::On(rule)) => Some(rule.ttl),
                _ => None,
            })
            .max()
            .unwrap_or_default();
        if cache_ttl_max.is_zero()
            && let Some(route) = routes.iter().find(|r| !r.invalidates.is_empty())
        {
            self.diags.push(
                Diagnostic::warning(
                    format!("{}.invalidates", route.source.loc()),
                    "nothing in this project is cached, so there's nothing to invalidate",
                )
                .hint("set `cache` on the routes whose responses this one makes out of date"),
            );
        }

        let route_tools = self.compile_route_tools(&routes, &entries, mcp_pending.as_ref());
        let mcp = mcp_pending.and_then(|p| p.finish(&routes, route_tools));
        if let Some(m) = &mcp
            && m.tools.is_empty()
            && m.hosted()
        {
            self.diags.push(
                Diagnostic::warning(routes[m.route].source.loc(), "the MCP endpoint has no tools")
                    .hint("add [[mcp.tool]] entries, set `mcp` on routes, or set `proxy` to forward to an existing MCP server"),
            );
        }

        self.check_handlers(&routes, &entries);
        self.warnings(&routes, &entries);

        let (routers, slots, catch_alls) = self.build_routers(&routes);
        let mut functions = self.functions(&raw, server_timeout);
        if let Some(dime) = &dimedb {
            self.register_dimedb_functions(dime, server_timeout, &mut functions);
        }
        let outbound = self.outbound(&raw, server_timeout);
        self.check_calls(&functions, &outbound);
        let telemetry = self.telemetry(&raw, &outbound);

        Some(Project {
            config_file: self.config_file.clone(),
            server,
            runtimes: self.runtimes.clone(),
            schemes: self.schemes.clone(),
            store,
            routes,
            entries,
            invalidated,
            cache_ttl_max,
            openapi: openapi_meta,
            mcp,
            functions,
            outbound,
            telemetry,
            dimedb,
            generated: self.generated(&raw),
            files: std::mem::take(&mut self.files),
            routes_dir,
            secret_env: std::mem::take(&mut self.secret_env).into_iter().collect(),
            routers,
            slots,
            catch_alls,
        })
    }

    /// Whether `value` still has a variable in it, which only a symbolic
    /// compile leaves there. Checks of the value's form don't apply to it.
    fn unresolved(&self, value: &str) -> bool {
        self.symbolic && value.contains("{env.")
    }

    /// Every name in an `http` runtime's `calls` is a function or an
    /// outbound service.
    fn check_calls(
        &mut self,
        functions: &BTreeMap<String, Arc<Function>>,
        outbound: &BTreeMap<String, Arc<Outbound>>,
    ) {
        for (name, runtime) in &self.runtimes {
            let loc = format!("[runtimes.{name}].calls");
            match &runtime.calls {
                Calls::None => {}
                Calls::All if functions.is_empty() && outbound.is_empty() => {
                    self.diags.push(
                        Diagnostic::error(
                            &loc,
                            "this project has no functions or outbound services to call",
                        )
                        .hint("add a [[function]] or an [outbound.<name>], or remove `calls`"),
                    );
                }
                Calls::All => {}
                Calls::Only(names) => {
                    for called in names {
                        if !functions.contains_key(called) && !outbound.contains_key(called) {
                            self.diags.push(
                                Diagnostic::error(
                                    &loc,
                                    format!("no [[function]] or [outbound.{called}] is named \"{called}\""),
                                )
                                .hint("name a function or an outbound service in this config, or use \"*\" for all of them"),
                            );
                        }
                    }
                }
            }
        }
    }

    fn functions(
        &mut self,
        raw: &RawConfig,
        default_timeout: Duration,
    ) -> BTreeMap<String, Arc<Function>> {
        let mut out: BTreeMap<String, Arc<Function>> = BTreeMap::new();
        for (i, f) in raw.function.iter().enumerate() {
            let loc = format!("{} function[{i}] \"{}\"", self.config_file, f.name);
            let valid = !f.name.is_empty()
                && f.name.len() <= 128
                && f.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            if !valid {
                self.diags.error(
                    format!("{loc}.name"),
                    "a function name is 1 to 128 letters, digits, `_`, `-` and `.`",
                );
                continue;
            }
            if out.contains_key(&f.name) {
                self.diags.error(
                    format!("{loc}.name"),
                    format!("there is already a function named \"{}\"", f.name),
                );
                continue;
            }
            let Some(handler) =
                self.handler_ref(&f.handler, f.runtime.as_deref(), &format!("{loc}.handler"))
            else {
                continue;
            };
            if !self.host.exists(&handler.file) {
                self.diags.push(
                    Diagnostic::error(
                        format!("{loc}.handler"),
                        format!("handler file \"{}\" not found", handler.file),
                    )
                    .hint("handler paths are relative to the project root"),
                );
            }
            let rule = |c: &mut Self, schema: &Option<SchemaRef>, key: &str| {
                let at = format!("{loc}.{key}");
                let schema = c.schema(schema.as_ref()?, &at)?;
                match BodyRule::new(schema, true, c.schemas) {
                    Ok(rule) => Some(rule),
                    Err(e) => {
                        c.diags.error(at, e);
                        None
                    }
                }
            };
            let input = rule(self, &f.input, "input");
            let output = rule(self, &f.output, "output");
            if f.timeout.is_some_and(|t| t.0.is_zero()) {
                self.diags
                    .error(format!("{loc}.timeout"), "timeout must be longer than 0");
            }
            out.insert(
                f.name.clone(),
                Arc::new(Function {
                    name: f.name.clone(),
                    handler,
                    input,
                    output,
                    timeout: f.timeout.map_or(default_timeout, |t| t.0),
                }),
            );
        }
        out
    }

    fn compile_dimedb(
        &mut self,
        raw: &config::RawDimeDb,
    ) -> Option<Arc<crate::dimedb::DimeDbConfig>> {
        let loc = "[dimedb]";
        let Some(schema_toml) = self.host.read(&raw.schema) else {
            self.diags.push(
                Diagnostic::error(
                    format!("{loc}.schema"),
                    format!("schema file \"{}\" not found", raw.schema),
                )
                .hint("schema paths are relative to the project root"),
            );
            return None;
        };
        self.files.push(raw.schema.clone());
        let schema = match crate::dimedb::DimeSchema::from_toml(&schema_toml) {
            Ok(s) => s,
            Err(e) => {
                self.diags.error(format!("{loc}.schema"), e);
                return None;
            }
        };
        let prefix = raw
            .prefix
            .as_deref()
            .unwrap_or(crate::dimedb::RUNTIME)
            .to_string();
        if prefix.is_empty()
            || !prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            self.diags.error(
                format!("{loc}.prefix"),
                "prefix must be letters, digits, `_` or `-`",
            );
            return None;
        }
        let url = raw
            .url
            .as_ref()
            .map(|u| self.subst(u, &format!("{loc}.url")));
        match raw.mode {
            config::DimeDbMode::Embedded => {
                if let Some(rules) = &raw.rules {
                    if !self.host.exists(rules) {
                        self.diags.push(
                            Diagnostic::error(
                                format!("{loc}.rules"),
                                format!("WebAssembly rules file \"{rules}\" not found"),
                            )
                            .hint("compile your guest rules to wasm32-unknown-unknown"),
                        );
                    } else {
                        self.files.push(rules.clone());
                    }
                } else {
                    self.diags.error(
                        format!("{loc}.rules"),
                        "embedded [dimedb] requires `rules` pointing to a compiled .wasm module",
                    );
                }
            }
            config::DimeDbMode::Remote => {
                if let Some(u) = &url {
                    self.check_url(u, &format!("{loc}.url"));
                    self.warn_plain_http(
                        u,
                        &format!("{loc}.url"),
                        "dimedb commands and credentials travel in plaintext",
                    );
                } else {
                    self.diags.error(
                        format!("{loc}.url"),
                        "remote [dimedb] requires `url` pointing to a dimeDB server",
                    );
                }
            }
        }
        let mut headers = BTreeMap::new();
        for (k, v) in &raw.headers {
            let val = self.subst_secret(v, &format!("{loc}.headers.{k}"));
            headers.insert(k.clone(), val);
        }
        if let Some(allowed) = &raw.mcp_commands {
            for name in allowed {
                if !schema.commands.iter().any(|c| c.name == *name) {
                    self.diags.error(
                        format!("{loc}.mcp_commands"),
                        format!("unknown command \"{name}\" in dimedb schema"),
                    );
                }
            }
        }
        if let Some(allowed) = &raw.mcp_tables {
            for name in allowed {
                if !schema.tables.iter().any(|t| t.name == *name) {
                    self.diags.error(
                        format!("{loc}.mcp_tables"),
                        format!("unknown table \"{name}\" in dimedb schema"),
                    );
                }
            }
        }
        const DEFAULT_MAX_ROWS: u64 = 1_000_000;
        const DEFAULT_MAX_REQUEST_IDS: u64 = 1_000_000;
        const DEFAULT_DISK_LIMIT_MB: u64 = 4096;
        let max_rows = match raw.max_rows {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(DEFAULT_MAX_ROWS),
        };
        let max_request_ids = match raw.max_request_ids {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(DEFAULT_MAX_REQUEST_IDS),
        };
        let disk_limit = match raw.disk_limit_mb {
            Some(0) => None,
            Some(mb) => Some(mb.saturating_mul(1 << 20)),
            None => Some(DEFAULT_DISK_LIMIT_MB << 20),
        };
        Some(Arc::new(crate::dimedb::DimeDbConfig {
            mode: raw.mode,
            schema_path: raw.schema.clone(),
            schema_toml,
            schema,
            dir: raw.dir.clone().unwrap_or_else(|| "data/dimedb".into()),
            rules_path: raw.rules.clone(),
            url: url.map(|u| u.trim_end_matches('/').to_string()),
            headers,
            retry_window: raw.retry_window.map(|d| d.0),
            lookup_window: raw.lookup_window.map(|d| d.0),
            prefix,
            mcp: raw.mcp,
            mcp_commands: raw.mcp_commands.clone(),
            mcp_tables: raw.mcp_tables.clone(),
            mcp_lookup: raw.mcp_lookup,
            fuel_per_check: raw.fuel_per_check.unwrap_or(200_000),
            max_rows,
            max_request_ids,
            disk_limit,
        }))
    }

    fn register_dimedb_functions(
        &mut self,
        dime: &crate::dimedb::DimeDbConfig,
        timeout: Duration,
        out: &mut BTreeMap<String, Arc<Function>>,
    ) {
        let mut insert_fn = |name: String, op: String, input_schema: Value| {
            let input = BodyRule::new(input_schema, true, self.schemas).ok();
            out.entry(name.clone()).or_insert_with(|| {
                Arc::new(Function {
                    name,
                    handler: crate::dimedb::handler_ref(&op),
                    input,
                    output: None,
                    timeout,
                })
            });
        };
        for cmd in &dime.schema.commands {
            let fn_name = format!("{}.{}", dime.prefix, crate::dimedb::to_snake(&cmd.name));
            insert_fn(
                fn_name,
                format!("cmd:{}", cmd.name),
                crate::dimedb::command_input_schema(cmd),
            );
        }
        insert_fn(
            format!("{}.lookup", dime.prefix),
            "lookup".into(),
            crate::dimedb::lookup_input_schema(),
        );
        for table in &dime.schema.tables {
            insert_fn(
                format!("{}.get.{}", dime.prefix, table.name),
                format!("get:{}", table.name),
                crate::dimedb::get_input_schema(table),
            );
        }
    }

    fn register_dimedb_mcp_tools(
        &mut self,
        dime: &crate::dimedb::DimeDbConfig,
        pending: &mut McpPending,
        endpoint: (String, Layer),
        drafts: &mut Vec<Draft>,
    ) {
        let mut add_tool =
            |name: String, desc: String, op: String, input: Value, read_only: bool| {
                if pending.tools.iter().any(|t| t.name == name) {
                    return;
                }
                let input_rule = BodyRule::new(input.clone(), true, self.schemas).ok();
                let annotations = if read_only {
                    Some(serde_json::json!({ "readOnlyHint": true, "idempotentHint": true }))
                } else {
                    Some(serde_json::json!({ "idempotentHint": true }))
                };
                drafts.push(Draft {
                    methods: Some(vec!["POST".into()]),
                    path: format!("mcp:{name}"),
                    host: None,
                    action: Action::Handler(crate::dimedb::handler_ref(&op)),
                    source: Source::McpTool {
                        file: self.config_file.clone(),
                        line: None,
                        index: pending.tools.len() + 1,
                        name: name.clone(),
                    },
                    own: Layer::default(),
                    inherit: vec![endpoint.clone()],
                    validation: RawValidation {
                        body: Some((input.clone(), true)),
                        response: None,
                        ..Default::default()
                    },
                    openapi_layer: None,
                    op: None,
                    mcp: None,
                    shadow: None,
                    invalidates: None,
                });
                pending.tools.push(McpTool {
                    name,
                    title: None,
                    description: Some(desc),
                    input_schema: input,
                    output_schema: None,
                    annotations,
                    param_headers: Vec::new(),
                    route: 0,
                    own: Layer::default(),
                    input_rule,
                    binding: None,
                });
            };

        for cmd in &dime.schema.commands {
            if let Some(allowed) = &dime.mcp_commands
                && !allowed.iter().any(|n| n == &cmd.name)
            {
                continue;
            }
            let name = format!("{}_{}", dime.prefix, crate::dimedb::to_snake(&cmd.name));
            let desc = format!(
                "Execute dimeDB command `{}` at most once. Pass a unique `request_id` (UUIDv7 or integer) and reuse it on retries.",
                cmd.name
            );
            add_tool(
                name,
                desc,
                format!("cmd:{}", cmd.name),
                crate::dimedb::command_input_schema(cmd),
                false,
            );
        }
        if dime.mcp_lookup {
            add_tool(
                format!("{}_lookup", dime.prefix),
                "Look up what became of a previous dimeDB `request_id` (applied, not_applied, or unknown).".into(),
                "lookup".into(),
                crate::dimedb::lookup_input_schema(),
                true,
            );
        }
        for table in &dime.schema.tables {
            if let Some(allowed) = &dime.mcp_tables
                && !allowed.iter().any(|n| n == &table.name)
            {
                continue;
            }
            add_tool(
                format!("{}_get_{}", dime.prefix, table.name),
                format!(
                    "Read a row from dimeDB table `{}` by primary key.",
                    table.name
                ),
                format!("get:{}", table.name),
                crate::dimedb::get_input_schema(table),
                true,
            );
        }
    }

    fn outbound(
        &mut self,
        raw: &RawConfig,
        default_timeout: Duration,
    ) -> BTreeMap<String, Arc<Outbound>> {
        let mut out = BTreeMap::new();
        for (name, o) in &raw.outbound {
            let loc = format!("[outbound.{name}]");
            let named = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            if !named {
                self.diags
                    .error(&loc, "a service name is letters, digits, `_`, `-` and `.`");
                continue;
            }
            let url = self.subst(&o.url, &format!("{loc}.url"));
            let url = url.trim_end_matches('/').to_string();
            self.check_url(&url, &format!("{loc}.url"));
            if !o.headers.is_empty() {
                self.warn_plain_http(
                    &url,
                    &format!("{loc}.url"),
                    "the headers Unsworn adds can be read on the network path",
                );
            }
            if url.contains(['?', '#']) {
                self.diags.error(
                    format!("{loc}.url"),
                    "the URL can't have a query or a fragment: a handler's path goes after it",
                );
            }
            let mut headers = Vec::new();
            for (header, value) in &o.headers {
                let at = format!("{loc}.headers.{header}");
                let value = self.subst_secret(value, &at);
                if !valid_header_name(header) {
                    self.diags.error(&at, "not a valid header name");
                } else if value.chars().any(|c| c.is_control() && c != '\t') {
                    self.diags
                        .error(&at, "header values can't contain line breaks");
                } else {
                    headers.push((header.to_ascii_lowercase(), value));
                }
            }
            let methods = self.methods(o.methods.clone(), &loc);
            let mut path_segs = Vec::new();
            for path in o.paths.iter().flatten() {
                if self.check_path(path, &loc) {
                    path_segs.push(parse_pattern(path));
                }
            }
            if o.timeout.is_some_and(|t| t.0.is_zero()) {
                self.diags
                    .error(format!("{loc}.timeout"), "timeout must be longer than 0");
            }
            out.insert(
                name.clone(),
                Arc::new(Outbound {
                    name: name.clone(),
                    url,
                    headers,
                    methods,
                    paths: o.paths.clone(),
                    path_segs,
                    timeout: o.timeout.map_or(default_timeout, |t| t.0),
                    max_response: o.max_response.map_or(10 * 1024 * 1024, |s| s.0),
                }),
            );
        }
        out
    }

    /// Spans go only where an outbound service says they may, checked as a
    /// handler's `fetch` is.
    fn telemetry(
        &mut self,
        raw: &RawConfig,
        outbound: &BTreeMap<String, Arc<Outbound>>,
    ) -> Option<Telemetry> {
        let t = raw.telemetry.as_ref()?;
        let sample = t.sample.unwrap_or(0.0);
        if !(0.0..=1.0).contains(&sample) {
            self.diags.error(
                "[telemetry].sample",
                format!("sample is a share of requests, from 0 to 1, not {sample}"),
            );
        }
        let at = "[telemetry].outbound";
        let service = match crate::outbound::service(outbound, &t.outbound) {
            Ok(service) => service,
            Err((_, why)) => {
                self.diags.error(at, why);
                return None;
            }
        };
        let metrics = t.metrics.unwrap_or(false);
        let logs = t.logs.unwrap_or(false);
        let sent = [
            (true, at, "spans", TRACES_PATH),
            (metrics, "[telemetry].metrics", "metrics", METRICS_PATH),
            (logs, "[telemetry].logs", "logs", LOGS_PATH),
        ];
        for (_, at, what, path) in sent.into_iter().filter(|(on, ..)| *on) {
            if let Err(why) = service.allows("POST", path) {
                self.diags.push(Diagnostic::error(at, why).hint(format!(
                    "{what} are sent with `POST {path}`, which the service's `methods` and `paths` have to allow"
                )));
            }
        }
        self.warn_plain_http(
            &service.url,
            at,
            "spans, which carry clients' addresses, can be read on the network path",
        );
        Some(Telemetry {
            outbound: service.clone(),
            service_name: t.service_name.clone(),
            sample,
            metrics,
            logs,
        })
    }

    fn generated(&mut self, raw: &RawConfig) -> Vec<Generated> {
        use crate::codegen::{Lang, Options};
        let Some(g) = &raw.types else {
            return Vec::new();
        };
        let go = g.go.as_ref().map(|go| match go {
            config::RawGenGo::File(file) => (file.clone(), None),
            config::RawGenGo::Full(f) => (f.file.clone(), f.package.clone()),
        });
        let (go_file, go_package) = go.unzip();
        let wanted = [
            (Lang::TypeScript, g.ts.clone(), None),
            (Lang::Python, g.python.clone(), None),
            (Lang::Go, go_file, go_package.flatten()),
            (Lang::Rust, g.rust.clone(), None),
        ];
        let mut out = Vec::new();
        for (lang, file, go_package) in wanted {
            let Some(file) = file else { continue };
            let loc = format!("[gen].{}", lang.name());
            // Somewhere inside the project: `unsworn dev` writes these
            // files without being asked each time.
            let inside = !file.is_empty()
                && !file.starts_with('/')
                && !file.split('/').any(|part| part == "..");
            if !inside {
                self.diags.push(
                    Diagnostic::error(&loc, format!("\"{file}\" isn't a path inside the project"))
                        .hint("write it relative to the project directory, such as \"src/unsworn.gen.ts\""),
                );
                continue;
            }
            if let Some(p) = &go_package
                && !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                self.diags.error(
                    format!("{loc}.package"),
                    format!("\"{p}\" isn't a Go package name"),
                );
                continue;
            }
            out.push(Generated {
                lang,
                file: file.clone(),
                options: Options {
                    go_package,
                    file: Some(file),
                },
            });
        }
        out
    }

    fn subst(&mut self, value: &str, loc: &str) -> String {
        if self.symbolic {
            return value.to_string();
        }
        let mut out = String::new();
        let mut rest = value;
        while let Some(start) = rest.find("{env.") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 5..];
            let Some(end) = after.find('}') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let name = &after[..end];
            match self.host.env(name) {
                Some(v) => out.push_str(&v),
                None => self.diags.push(
                    Diagnostic::error(loc, format!("environment variable {name} isn't set")).hint(
                        format!("set {name}, or remove the {{env.{name}}} reference"),
                    ),
                ),
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// As [`Compiler::subst`], for a value that is a credential: a signing
    /// secret, API keys, a verifier's or the store's own credentials. The
    /// variables it reads are noted, so hosts can keep them from handlers.
    fn subst_secret(&mut self, value: &str, loc: &str) -> String {
        let mut rest = value;
        while let Some(start) = rest.find("{env.") {
            let after = &rest[start + 5..];
            let Some(end) = after.find('}') else { break };
            self.secret_env.insert(after[..end].to_string());
            rest = &after[end + 1..];
        }
        self.subst(value, loc)
    }

    /// Reports a URL Unsworn fetches that isn't `http://` or `https://`.
    fn check_url(&mut self, url: &str, loc: &str) {
        // An unset variable has already been reported.
        if url.is_empty() || self.unresolved(url) {
            return;
        }
        let ok = ["http://", "https://"].iter().any(|scheme| {
            url.get(..scheme.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
                && url.len() > scheme.len()
        });
        if !ok {
            self.diags.push(
                Diagnostic::error(loc, format!("\"{url}\" isn't an http:// or https:// URL"))
                    .hint("use a full URL, such as https://auth.example.com/verify-key"),
            );
        }
    }

    /// Warns about a URL that credentials or signing keys travel over in
    /// the clear. This machine's own addresses are fine.
    fn warn_plain_http(&mut self, url: &str, loc: &str, risk: &str) {
        if self.unresolved(url) {
            return;
        }
        let Ok(uri) = url.parse::<http::Uri>() else {
            return;
        };
        if !uri
            .scheme_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("http"))
        {
            return;
        }
        let host = uri.host().unwrap_or_default();
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let local = host.eq_ignore_ascii_case("localhost")
            || bare
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !local {
            self.diags.push(
                Diagnostic::warning(loc, format!("\"{url}\" isn't encrypted, so {risk}"))
                    .hint("use an https:// URL"),
            );
        }
    }

    fn server(&mut self, raw: &RawConfig) -> Server {
        let s = &raw.server;
        if let Some(listen) = &s.listen
            && listen.parse::<std::net::SocketAddr>().is_err()
        {
            self.diags.push(Diagnostic::error(
                "[server].listen",
                format!("\"{listen}\" isn't an address like \"0.0.0.0:8080\""),
            ));
        }
        if s.max_connections == Some(0) {
            self.diags.error(
                "[server].max_connections",
                "max_connections must be at least 1",
            );
        }
        if s.idle_timeout.is_some_and(|d| d.0.is_zero()) {
            self.diags.error(
                "[server].idle_timeout",
                "idle_timeout must be longer than 0",
            );
        }
        if s.trusted_proxies.iter().any(|t| t == "all") {
            self.diags.push(
                Diagnostic::warning(
                    "[server].trusted_proxies",
                    "\"all\" takes every client's `X-Forwarded-For` at its word, so a client that reaches Unsworn directly can pick the address its `ip` rules and rate limits see",
                )
                .hint("list the addresses or ranges of your load balancer instead, or make sure nothing but it can reach Unsworn's port"),
            );
        }
        let tls = s.tls.as_ref().map(|t| (t.cert.clone(), t.key.clone()));
        if let Some((cert, key)) = &tls {
            for (name, path) in [("cert", cert), ("key", key)] {
                if !self.host.exists(path) {
                    self.diags.error(
                        format!("[server.tls].{name}"),
                        format!("file \"{path}\" not found"),
                    );
                }
            }
        }
        Server {
            listen: s
                .listen
                .clone()
                .unwrap_or_else(|| config::DEFAULT_LISTEN.into()),
            listen_set: s.listen.is_some(),
            max_body: s.max_body.map_or(1024 * 1024, |b| b.0),
            timeout: s.timeout.map_or(Duration::from_secs(30), |d| d.0),
            trailing_slash: s.trailing_slash,
            tls,
            compression: s.compression,
            trusted_proxies: s.trusted_proxies.clone(),
            admin_listen: s.admin_listen.clone(),
            response_validation: s.response_validation,
            shutdown_timeout: s.shutdown_timeout.map_or(Duration::from_secs(10), |d| d.0),
            max_connections: s.max_connections.unwrap_or(10_000) as usize,
            idle_timeout: s.idle_timeout.map_or(Duration::from_secs(300), |d| d.0),
        }
    }

    fn runtimes(&mut self, raw: &RawConfig) {
        let mut by_ext: HashMap<String, String> = HashMap::new();
        for (name, r) in &raw.runtimes {
            let loc = format!("[runtimes.{name}]");
            for ext in &r.extensions {
                if !ext.starts_with('.') || ext.len() < 2 {
                    self.diags.push(Diagnostic::error(
                        format!("{loc}.extensions"),
                        format!("\"{ext}\" should start with a dot, like \".ts\""),
                    ));
                }
                if let Some(other) = by_ext.insert(ext.clone(), name.clone()) {
                    self.diags.error(
                        format!("{loc}.extensions"),
                        format!("\"{ext}\" is also claimed by [runtimes.{other}]"),
                    );
                }
            }
            if r.command.trim().is_empty() {
                self.diags
                    .error(format!("{loc}.command"), "command is empty");
            }
            let protocol = if r.protocol == Protocol::Binary {
                self.diags.push(
                    Diagnostic::warning(
                        format!("{loc}.protocol"),
                        "\"binary\" is the old name for \"framed\"",
                    )
                    .hint("set protocol = \"framed\""),
                );
                Protocol::Framed
            } else {
                r.protocol
            };
            let http = protocol == Protocol::Http;
            if http && !r.extensions.is_empty() {
                self.diags.push(
                    Diagnostic::error(
                        format!("{loc}.extensions"),
                        "an `http` runtime doesn't run handler files",
                    )
                    .hint(format!("remove `extensions`; routes reach it with `proxy = {{ runtime = \"{name}\" }}`")),
                );
            }
            if !http && r.extensions.is_empty() {
                self.diags.push(
                    Diagnostic::error(
                        format!("{loc}.extensions"),
                        "no extensions, so no handler file would be run by it",
                    )
                    .hint(
                        "list the extensions of its handler files, such as extensions = [\".ts\"]",
                    ),
                );
            }
            if !http && r.listen.is_some() {
                self.diags.error(
                    format!("{loc}.listen"),
                    "only an `http` runtime listens: this one's workers connect to Unsworn",
                );
            }
            let calls = if r.calls.is_empty() {
                Calls::None
            } else if !http {
                self.diags.push(
                    Diagnostic::error(
                        format!("{loc}.calls"),
                        "`calls` is for an `http` runtime's server: handlers call with `ctx.call` and `ctx.fetch`",
                    )
                    .hint("remove `calls`"),
                );
                Calls::None
            } else if r.calls.iter().any(|c| c == "*") {
                if r.calls.len() > 1 {
                    self.diags.push(
                        Diagnostic::error(
                            format!("{loc}.calls"),
                            "\"*\" already allows every function and outbound service",
                        )
                        .hint("set calls = [\"*\"], or name only the ones it calls"),
                    );
                }
                Calls::All
            } else {
                Calls::Only(r.calls.iter().cloned().collect())
            };
            let mut env = BTreeMap::new();
            for (k, v) in &r.env {
                env.insert(k.clone(), self.subst(v, &format!("{loc}.env.{k}")));
            }
            let workers = match r.workers {
                // An HTTP server takes many requests at once on its own,
                // and a second copy of one is a decision for whoever runs it.
                None if http => Workers::fixed(1),
                None => Workers::default(),
                Some(config::RawWorkers::Fixed(n)) => Workers::fixed(n),
                Some(config::RawWorkers::Range(range)) => {
                    let min = range.min.unwrap_or(1);
                    if min == 0 {
                        self.diags.push(
                            Diagnostic::error(
                                format!("{loc}.workers.min"),
                                "a runtime keeps at least one worker",
                            )
                            .hint("leave `eager` off to start the first one on the first request"),
                        );
                    }
                    if range.max.is_some_and(|max| max < min) {
                        self.diags.error(
                            format!("{loc}.workers.max"),
                            format!("max is less than min ({min})"),
                        );
                    }
                    Workers {
                        min: min.max(1),
                        max: range.max,
                        idle: range.idle.map_or(Workers::IDLE, |d| d.0),
                    }
                }
            };
            self.runtimes.insert(
                name.clone(),
                Arc::new(Runtime {
                    name: name.clone(),
                    extensions: r.extensions.clone(),
                    command: r.command.clone(),
                    build: r.build.clone(),
                    protocol,
                    workers,
                    eager: r.eager,
                    env,
                    startup_timeout: r.startup_timeout.map_or(Duration::from_secs(30), |d| d.0),
                    listen: r.listen.unwrap_or_default(),
                    calls,
                }),
            );
        }
    }

    fn schemes(&mut self, raw: &RawConfig) {
        for (name, scheme) in &raw.auth {
            let loc = format!("[auth.{name}]");
            if name == "none" {
                self.diags
                    .error(&loc, "\"none\" is reserved for public routes");
                continue;
            }
            let scheme = match scheme {
                RawAuthScheme::Jwt(j) => {
                    if j.jwks_url.is_some() {
                        let diag = if j.url.is_some() {
                            Diagnostic::error(
                                format!("{loc}.jwks_url"),
                                "set `url` or `jwks_url`, not both",
                            )
                        } else {
                            Diagnostic::warning(
                                format!("{loc}.jwks_url"),
                                "`jwks_url` is the old name for `url`",
                            )
                        };
                        self.diags.push(diag.hint("rename jwks_url to url"));
                        if j.url.is_some() {
                            continue;
                        }
                    }
                    let url = j.url.as_ref().or(j.jwks_url.as_ref());
                    let sources = [url.is_some(), j.secret.is_some(), j.public_key.is_some()]
                        .iter()
                        .filter(|b| **b)
                        .count();
                    if sources != 1 {
                        self.diags.push(Diagnostic::error(
                            &loc,
                            "set exactly one of `url`, `secret` or `public_key`",
                        ));
                        continue;
                    }
                    let keys = if let Some(url) = url {
                        let url = self.subst(url, &format!("{loc}.url"));
                        self.check_url(&url, &format!("{loc}.url"));
                        self.warn_plain_http(
                            &url,
                            &format!("{loc}.url"),
                            "anyone on the network path can swap in their own signing keys and mint tokens",
                        );
                        if j.issuer.is_none() && j.audience.is_none() {
                            self.diags.push(
                                Diagnostic::warning(
                                    &loc,
                                    "without `issuer` or `audience`, any token signed by these keys is accepted",
                                )
                                .hint("set `audience` (and `issuer`): key sets such as Firebase's sign tokens for every project, not only yours"),
                            );
                        }
                        JwtKeys::Jwks(url)
                    } else if let Some(secret) = &j.secret {
                        let at = format!("{loc}.secret");
                        let reported = self.diags.items.len();
                        let secret = self.subst_secret(secret, &at);
                        // An unset variable has already been reported.
                        if secret.is_empty() && self.diags.items.len() == reported {
                            self.diags.push(
                                Diagnostic::error(&at, "the secret is empty")
                                    .hint("anyone can sign a token with an empty secret; set one of at least 32 random bytes"),
                            );
                        } else if !secret.is_empty()
                            && secret.len() < MIN_SECRET_BYTES
                            && !self.unresolved(&secret)
                        {
                            self.diags.push(
                                Diagnostic::warning(
                                    &at,
                                    format!(
                                        "the secret is shorter than {MIN_SECRET_BYTES} bytes; one this short can be guessed from a single token"
                                    ),
                                )
                                .hint(format!("use at least {MIN_SECRET_BYTES} random bytes, e.g. from `openssl rand -base64 32`")),
                            );
                        }
                        JwtKeys::Secret(secret)
                    } else {
                        let path = j.public_key.clone().unwrap_or_default();
                        self.files.push(path.clone());
                        match self.host.read(&path) {
                            Some(pem) => JwtKeys::PublicKey(pem),
                            None => {
                                self.diags.error(
                                    format!("{loc}.public_key"),
                                    format!("file \"{path}\" not found"),
                                );
                                continue;
                            }
                        }
                    };
                    let algorithms = match &j.algorithms {
                        Some(list) => {
                            let mut out = Vec::new();
                            for a in list {
                                match a.parse::<Algorithm>() {
                                    Ok(alg) => out.push(alg),
                                    Err(_) => self.diags.error(
                                        format!("{loc}.algorithms"),
                                        format!("unknown algorithm \"{a}\""),
                                    ),
                                }
                            }
                            out
                        }
                        None => match keys {
                            JwtKeys::Secret(_) => vec![Algorithm::HS256],
                            _ => vec![
                                Algorithm::RS256,
                                Algorithm::PS256,
                                Algorithm::ES256,
                                Algorithm::EdDSA,
                            ],
                        },
                    };
                    let audience: Vec<String> = j
                        .audience
                        .as_ref()
                        .map(|a| a.to_vec())
                        .unwrap_or_default()
                        .iter()
                        .map(|a| self.subst(a, &loc))
                        .collect();
                    let protected_resource =
                        j.protected_resource.as_ref().map(|p| ProtectedResource {
                            resource: p.resource.clone(),
                            authorization_servers: p.authorization_servers.clone(),
                            scopes_supported: p.scopes_supported.clone(),
                            resource_name: p.resource_name.clone(),
                        });
                    if protected_resource.is_some() && audience.is_empty() {
                        self.diags.push(
                            Diagnostic::error(
                                format!("{loc}.audience"),
                                "a scheme with `protected_resource` must set `audience`",
                            )
                            .hint("set audience to the resource URL, so tokens issued for other services are rejected"),
                        );
                    }
                    let issuer = j.issuer.as_ref().map(|i| self.subst(i, &loc));
                    Scheme::Jwt(JwtScheme {
                        name: name.clone(),
                        header: j
                            .header
                            .clone()
                            .unwrap_or_else(|| "authorization".into())
                            .to_ascii_lowercase(),
                        issuer,
                        audience,
                        algorithms,
                        leeway: j.leeway.map_or(30, |d| d.0.as_secs()),
                        keys,
                        protected_resource,
                        cached: Default::default(),
                    })
                }
                RawAuthScheme::ApiKey(k) => {
                    let keys = match (&k.keys, &k.url) {
                        (Some(_), Some(_)) | (None, None) => {
                            self.diags.push(Diagnostic::error(
                                &loc,
                                "set exactly one of `keys` or `url`",
                            ));
                            continue;
                        }
                        (Some(list), None) => {
                            let url_only = [
                                ("headers", k.headers.is_some()),
                                ("timeout", k.timeout.is_some()),
                                ("cache", k.cache.is_some()),
                            ];
                            for (field, _) in url_only.iter().filter(|(_, set)| *set) {
                                self.diags.error(
                                    format!("{loc}.{field}"),
                                    format!("`{field}` only applies with `url`"),
                                );
                            }
                            let mut keys = Vec::new();
                            for key in list.to_vec() {
                                let value = self.subst_secret(&key, &format!("{loc}.keys"));
                                keys.extend(
                                    value
                                        .split(',')
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(String::from),
                                );
                            }
                            if keys.is_empty() {
                                self.diags
                                    .error(format!("{loc}.keys"), "no keys configured");
                            }
                            ApiKeys::Static(keys)
                        }
                        (None, Some(url)) => {
                            let url = self.subst(url, &format!("{loc}.url"));
                            self.check_url(&url, &format!("{loc}.url"));
                            self.warn_plain_http(
                                &url,
                                &format!("{loc}.url"),
                                "every API key sent to it can be read on the network path",
                            );
                            let mut headers = Vec::new();
                            for (name, value) in k.headers.iter().flatten() {
                                let at = format!("{loc}.headers.{name}");
                                let value = self.subst_secret(value, &at);
                                if !valid_header_name(name) {
                                    self.diags.error(&at, "not a valid header name");
                                } else if value.chars().any(|c| c.is_control() && c != '\t') {
                                    self.diags
                                        .error(&at, "header values can't contain line breaks");
                                } else {
                                    headers.push((name.to_ascii_lowercase(), value));
                                }
                            }
                            ApiKeys::Remote(KeyVerifier {
                                url,
                                headers,
                                timeout: k.timeout.map_or(Duration::from_secs(2), |d| d.0),
                                cache: match k.cache.map(|d| d.0) {
                                    Some(d) if d.is_zero() => {
                                        self.diags.push(
                                            Diagnostic::error(
                                                format!("{loc}.cache"),
                                                "cache must be longer than 0",
                                            )
                                            .hint("remove `cache` to ask the verifier every time"),
                                        );
                                        None
                                    }
                                    d => d,
                                },
                            })
                        }
                    };
                    let header = match (&k.header, &k.query) {
                        (None, None) => Some("x-api-key".into()),
                        (h, _) => h.as_ref().map(|h| h.to_ascii_lowercase()),
                    };
                    Scheme::ApiKey(ApiKeyScheme {
                        name: name.clone(),
                        header,
                        query: k.query.clone(),
                        keys,
                    })
                }
                RawAuthScheme::Webhook(w) => {
                    let Some(scheme) = self.webhook(name, w, &loc) else {
                        continue;
                    };
                    Scheme::Webhook(scheme)
                }
            };
            self.schemes.insert(name.clone(), Arc::new(scheme));
        }
    }

    /// A scheme for requests signed with a shared secret.
    fn webhook(&mut self, name: &str, w: &config::RawWebhook, loc: &str) -> Option<WebhookScheme> {
        let custom = w.header.is_some() || w.encoding.is_some() || w.prefix.is_some();
        let signed = match (&w.provider, &w.header) {
            (Some(provider), _) if custom => {
                self.diags.push(
                    Diagnostic::error(
                        loc,
                        format!("`provider = \"{provider}\"` already says where the signature is and how it's written"),
                    )
                    .hint("remove `header`, `encoding` and `prefix`, or remove `provider` to describe the signature yourself"),
                );
                return None;
            }
            (Some(provider), _) => match Signed::provider(provider) {
                Some(signed) => signed,
                None => {
                    self.diags.push(
                        Diagnostic::error(
                            format!("{loc}.provider"),
                            format!("unknown provider \"{provider}\""),
                        )
                        .hint(format!(
                            "one of {}; for another service, set `header` to the one that holds an HMAC-SHA256 of the body",
                            Signed::PROVIDERS.join(", ")
                        )),
                    );
                    return None;
                }
            },
            (None, Some(header)) => {
                if !valid_header_name(header) {
                    self.diags.error(
                        format!("{loc}.header"),
                        format!("\"{header}\" isn't a header name"),
                    );
                }
                let base64 = match w.encoding.as_deref() {
                    None | Some("hex") => false,
                    Some("base64") => true,
                    Some(other) => {
                        self.diags.error(
                            format!("{loc}.encoding"),
                            format!("\"{other}\" isn't an encoding: use \"hex\" or \"base64\""),
                        );
                        false
                    }
                };
                Signed::Body {
                    provider: None,
                    header: header.to_ascii_lowercase(),
                    prefix: w.prefix.clone().unwrap_or_default(),
                    base64,
                }
            }
            (None, None) => {
                self.diags.push(
                    Diagnostic::error(
                        loc,
                        "set `provider`, or `header` for a service Unsworn has no name for",
                    )
                    .hint(format!("providers: {}", Signed::PROVIDERS.join(", "))),
                );
                return None;
            }
        };
        if w.tolerance.is_some() && !signed.timed() {
            self.diags.error(
                format!("{loc}.tolerance"),
                "this signature is over the body alone, with no time in it to be too old",
            );
        }
        let at = format!("{loc}.secret");
        let reported = self.diags.items.len();
        let secret = self.subst_secret(&w.secret, &at);
        if secret.is_empty() && self.diags.items.len() == reported {
            self.diags.push(
                Diagnostic::error(&at, "the secret is empty")
                    .hint("anyone can sign a request with an empty secret"),
            );
        }
        // A Standard Webhooks secret is base64 after `whsec_`. Every
        // other one is used as it's written.
        let key = match &signed {
            Signed::Standard { .. } if !self.unresolved(&secret) => {
                use base64::Engine;
                let encoded = secret.strip_prefix("whsec_").unwrap_or(&secret);
                match base64::engine::general_purpose::STANDARD.decode(encoded) {
                    Ok(key) => key,
                    Err(_) => {
                        self.diags.push(
                            Diagnostic::error(
                                &at,
                                "the secret isn't base64, which this provider's secrets are",
                            )
                            .hint("copy it as the service shows it, `whsec_` and all"),
                        );
                        Vec::new()
                    }
                }
            }
            _ => secret.into_bytes(),
        };
        let tolerance = w.tolerance.map_or(300, |d| d.0.as_secs());
        Some(WebhookScheme::new(name, key, signed, tolerance))
    }

    fn store(&mut self, raw: &RawConfig) -> StoreConfig {
        let store = raw.store.clone().unwrap_or_default();
        let url = store
            .url
            .as_ref()
            .map(|u| self.subst_secret(u, "[store].url"));
        match (store.backend, &url) {
            (config::StoreBackend::Redis, None) => {
                self.diags.push(
                    Diagnostic::error("[store].url", "the redis store needs a `url`")
                        .hint("url = \"{env.REDIS_URL}\""),
                );
            }
            (config::StoreBackend::Redis, Some(u))
                if !u.is_empty()
                    && !self.unresolved(u)
                    && !StoreConfig::is_rest_url(u)
                    && !["redis://", "rediss://", "redis+unix://", "unix://"]
                        .iter()
                        .any(|s| u.starts_with(s)) =>
            {
                self.diags.push(
                    Diagnostic::error(
                        "[store].url",
                        "must start with redis://, rediss:// (TLS), redis+unix://, or https:// for a Redis REST API",
                    ),
                );
            }
            (config::StoreBackend::Memory, Some(_)) => {
                self.diags.push(Diagnostic::warning(
                    "[store].url",
                    "`url` is only used by the redis store",
                ));
            }
            _ => {}
        }
        let token = store
            .token
            .as_ref()
            .map(|t| self.subst_secret(t, "[store].token"));
        let rest = url.as_deref().is_some_and(StoreConfig::is_rest_url);
        if store.backend == config::StoreBackend::Redis && rest && token.is_none() {
            self.diags.push(
                Diagnostic::error("[store].token", "a Redis REST API needs a `token`")
                    .hint("token = \"{env.UPSTASH_REDIS_REST_TOKEN}\""),
            );
        }
        if token.is_some() && !rest && !url.as_deref().is_some_and(|u| self.unresolved(u)) {
            self.diags.push(Diagnostic::warning(
                "[store].token",
                "`token` is only used with an https:// `url` (a Redis REST API)",
            ));
        }
        StoreConfig {
            backend: store.backend,
            url,
            token,
            prefix: store.prefix.unwrap_or_default(),
            on_error: store.on_error,
        }
    }

    /// The runtime for a handler file no `[runtimes.*]` claims, when it's
    /// in a language with an SDK: the command that SDK's own instructions
    /// give. Which tool, where there's a choice, is read from the
    /// project's lock files. A project writes a runtime of its own only
    /// to run things differently.
    fn default_runtime(&mut self, ext: &str, file: &str) -> Option<String> {
        const JS: &[&str] = &[".ts", ".mts", ".js", ".mjs"];
        let (name, extensions, build, command, startup): (
            &str,
            &[&str],
            Option<String>,
            String,
            u64,
        ) = match ext {
            ".ts" | ".mts" | ".js" | ".mjs" => {
                let worker = "node_modules/@unswornjs/sdk/src/worker.ts";
                let tool = match self.host.exists("bun.lock") || self.host.exists("bun.lockb") {
                    true => "bun run",
                    false => "node",
                };
                ("ts", JS, None, format!("{tool} {worker}"), 30)
            }
            ".py" => {
                let python = match self.host.exists("uv.lock") {
                    true => "uv run python",
                    false => "python3",
                };
                (
                    "python",
                    &[".py"],
                    None,
                    format!("{python} -u -m unsworn.worker"),
                    30,
                )
            }
            ".go" => {
                // One worker program, built from the package the
                // handlers are in.
                let package = match file.rsplit_once('/') {
                    Some((dir, _)) => format!("./{dir}"),
                    None => ".".to_string(),
                };
                (
                    "go",
                    &[".go"],
                    Some(format!("go build -o .unsworn/go-worker {package}")),
                    ".unsworn/go-worker".to_string(),
                    120,
                )
            }
            ".rs" => {
                // The worker is the crate's binary, whose name is in
                // its manifest.
                let manifest = self.host.read("Cargo.toml")?;
                let manifest: toml::Value = toml::from_str(&manifest).ok()?;
                let name = |table: &toml::Value| table.get("name")?.as_str().map(String::from);
                let bin = manifest
                    .get("bin")
                    .and_then(|bins| bins.as_array()?.first().and_then(name))
                    .or_else(|| manifest.get("package").and_then(name))?;
                self.files.push("Cargo.toml".into());
                (
                    "rust",
                    &[".rs"],
                    Some("cargo build --release".to_string()),
                    format!("target/release/{bin}"),
                    300,
                )
            }
            _ => return None,
        };
        // A runtime of that name with other extensions is someone's own.
        let mut key = name.to_string();
        while self.runtimes.contains_key(&key) {
            key.push_str("-default");
        }
        self.runtimes.insert(
            key.clone(),
            Arc::new(Runtime {
                name: key.clone(),
                extensions: extensions.iter().map(|e| e.to_string()).collect(),
                command,
                build,
                protocol: Protocol::Framed,
                workers: Workers::default(),
                eager: false,
                env: BTreeMap::new(),
                startup_timeout: Duration::from_secs(startup),
                listen: config::Listen::default(),
                calls: Calls::None,
            }),
        );
        Some(key)
    }

    fn handler_ref(&mut self, raw: &str, runtime: Option<&str>, loc: &str) -> Option<HandlerRef> {
        let (file, name) = HandlerRef::split(raw);
        if file.is_empty() {
            self.diags.error(loc, format!("\"{raw}\" has no file part"));
            return None;
        }
        let runtime = match runtime {
            Some(r) => {
                if !self.runtimes.contains_key(r) {
                    self.diags.push(
                        Diagnostic::error(loc, format!("unknown runtime \"{r}\""))
                            .hint(format!("define [runtimes.{r}]")),
                    );
                    return None;
                }
                if self.runtimes[r].protocol == Protocol::Http {
                    self.diags.push(
                        Diagnostic::error(
                            loc,
                            format!("[runtimes.{r}] is an HTTP server, which has no handler files"),
                        )
                        .hint(format!(
                            "send the route to it with `proxy = {{ runtime = \"{r}\" }}`"
                        )),
                    );
                    return None;
                }
                r.to_string()
            }
            None => {
                let ext = file.rfind('.').map(|i| &file[i..]).unwrap_or("");
                let found = self
                    .runtimes
                    .values()
                    .find(|r| r.extensions.iter().any(|e| e == ext));
                let found = found.map(|r| r.name.clone());
                // A language Unsworn has an SDK for runs the usual way
                // unless a runtime says otherwise.
                match found.or_else(|| self.default_runtime(ext, file)) {
                    Some(name) => name,
                    None => {
                        let shown = if ext.is_empty() { file } else { ext };
                        self.diags.push(
                            Diagnostic::error(loc, format!("no runtime handles \"{shown}\""))
                                .hint(if ext.is_empty() {
                                    "set `runtime` on the route".to_string()
                                } else {
                                    format!(
                                        "add [runtimes.<name>] with extensions = [\"{ext}\"], or change the handler"
                                    )
                                }),
                        );
                        return None;
                    }
                }
            }
        };
        Some(HandlerRef {
            raw: raw.to_string(),
            file: file.trim_start_matches("./").to_string(),
            name: name.map(String::from),
            runtime,
        })
    }

    fn auth(&mut self, raw: &RawAuth, loc: &str) -> Option<AuthReq> {
        let mut alts = Vec::new();
        let mut check = |c: &mut Self, scheme: &str, scopes: Vec<String>| {
            if !c.schemes.contains_key(scheme) {
                c.diags.push(
                    Diagnostic::error(loc, format!("unknown auth scheme \"{scheme}\"")).hint(
                        format!("define [auth.{scheme}], or use \"none\" for a public route"),
                    ),
                );
            }
            alts.push(AuthAlt {
                scheme: scheme.to_string(),
                scopes,
            });
        };
        match raw {
            RawAuth::Name(n) if n == "none" => return Some(AuthReq::Public),
            RawAuth::Name(n) => check(self, n, Vec::new()),
            RawAuth::One(a) => check(self, &a.scheme, a.scopes.clone()),
            RawAuth::Many(items) => {
                for item in items {
                    match item {
                        RawAuthItem::Name(n) if n == "none" => {
                            self.diags
                                .error(loc, "\"none\" can't be combined with other schemes");
                        }
                        RawAuthItem::Name(n) => check(self, n, Vec::new()),
                        RawAuthItem::Alt(a) => check(self, &a.scheme, a.scopes.clone()),
                    }
                }
            }
        }
        Some(AuthReq::AnyOf(alts))
    }

    /// Converts policy keys into a layer. `use` sets apply first. `counter`
    /// names the table for a rate limit written in it (see
    /// [`RateLimit::counter`]); a limit from a set keeps the set's.
    fn layer(&mut self, raw: &RawPolicies, loc: &str, counter: &str) -> Layer {
        let mut layer = Layer::default();
        if let Some(sets) = &raw.use_sets {
            for name in sets {
                match self.sets.get(name) {
                    Some(set) => layer.merge_from(&set.clone()),
                    None => self.diags.push(
                        Diagnostic::error(
                            format!("{loc}.use"),
                            format!("unknown policy set \"{name}\""),
                        )
                        .hint(format!("define [policies.{name}]")),
                    ),
                }
            }
        }
        let mut own = Layer::default();
        if let Some(a) = &raw.auth {
            own.auth = self.auth(a, &format!("{loc}.auth"));
        }
        own.cors = raw.cors.as_ref().map(|c| match c {
            OrOff::Off => OrOff::Off,
            OrOff::On(c) => {
                let origins = c.origins.to_vec();
                let any_origin = origins.iter().any(|o| o == "*");
                // Browsers refuse `*` with credentials. Answering every
                // origin by name instead would let any website read
                // responses meant for a signed-in user.
                if any_origin && c.credentials {
                    self.diags.push(
                        Diagnostic::error(
                            format!("{loc}.cors"),
                            "`credentials = true` can't be used with the origin \"*\"",
                        )
                        .hint("list the origins that may send credentials, such as [\"https://app.example.com\"]"),
                    );
                }
                OrOff::On(Cors {
                    origins: if any_origin { None } else { Some(origins) },
                    credentials: c.credentials,
                    methods: c.methods.clone(),
                    headers: c.headers.clone(),
                    expose: c.expose.clone(),
                    max_age: c.max_age.map(|d| d.0),
                })
            }
        });
        own.ip = match &raw.ip {
            None => None,
            Some(OrOff::Off) => Some(OrOff::Off),
            Some(OrOff::On(r)) => {
                for entry in r.allow.iter().chain(&r.deny) {
                    if !valid_ip_or_cidr(entry) {
                        self.diags.error(
                            format!("{loc}.ip"),
                            format!("\"{entry}\" isn't an IP address or CIDR like \"10.0.0.0/8\""),
                        );
                    }
                }
                Some(OrOff::On(crate::policy::IpRules {
                    allow: r.allow.clone(),
                    deny: r.deny.clone(),
                }))
            }
        };
        own.origin = raw.origin.as_ref().map(|o| match o {
            OrOff::Off => OrOff::Off,
            OrOff::On(o) => OrOff::On(o.allow.clone()),
        });
        own.rate_limit = match &raw.rate_limit {
            None => None,
            Some(OrOff::Off) => Some(OrOff::Off),
            Some(OrOff::On(r)) => {
                let key = match KeySource::parse(r.key.as_deref().unwrap_or("ip")) {
                    Ok(k) => k,
                    Err(e) => {
                        self.diags.error(format!("{loc}.rate_limit.key"), e);
                        KeySource::Ip
                    }
                };
                if r.requests == 0 || r.per.0.is_zero() {
                    self.diags.error(
                        format!("{loc}.rate_limit"),
                        "`requests` and `per` must be greater than zero",
                    );
                }
                Some(OrOff::On(RateLimit {
                    requests: r.requests,
                    per: r.per.0,
                    key,
                    counter: counter.to_string(),
                }))
            }
        };
        own.cache = match &raw.cache {
            None => None,
            Some(OrOff::Off) => Some(OrOff::Off),
            Some(OrOff::On(c)) => {
                let key = match c.key.as_deref().map(KeySource::parse) {
                    None => None,
                    Some(Ok(k)) => Some(k),
                    Some(Err(e)) => {
                        self.diags.error(format!("{loc}.cache.key"), e);
                        None
                    }
                };
                if c.ttl.0.is_zero() {
                    self.diags
                        .error(format!("{loc}.cache.ttl"), "ttl must be greater than zero");
                }
                Some(OrOff::On(CacheRule {
                    ttl: c.ttl.0,
                    vary: c.vary.iter().map(|v| v.to_ascii_lowercase()).collect(),
                    key,
                }))
            }
        };
        own.timeout = raw.timeout.map(|d| d.0);
        own.max_body = raw.max_body.map(|b| b.0);
        own.stream_body = raw.stream_body;
        own.response_headers = raw.response_headers.as_ref().map(|h| match h {
            OrOff::Off => OrOff::Off,
            OrOff::On(h) => OrOff::On(HeaderRules {
                set: h
                    .set
                    .iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                    .collect(),
                remove: h.remove.iter().map(|k| k.to_ascii_lowercase()).collect(),
            }),
        });
        own.before = match &raw.before {
            None => None,
            Some(OrOff::Off) => Some(OrOff::Off),
            Some(OrOff::On(list)) => {
                let refs = list
                    .iter()
                    .filter_map(|h| self.handler_ref(h, None, &format!("{loc}.before")))
                    .collect();
                Some(OrOff::On(refs))
            }
        };
        own.require = match &raw.require {
            None => None,
            Some(OrOff::Off) => Some(OrOff::Off),
            Some(OrOff::On(pairs)) => {
                // `"params.org" = "auth.org"`, or the same with an unquoted
                // key, which TOML reads as a table inside a table.
                let mut written: Vec<(String, &String)> = Vec::new();
                for (key, value) in pairs {
                    match value {
                        config::RawRequireValue::Name(other) => written.push((key.clone(), other)),
                        config::RawRequireValue::Dotted(inner) => {
                            written.extend(inner.iter().map(|(k, v)| (format!("{key}.{k}"), v)));
                        }
                    }
                }
                let mut rules = Vec::new();
                for (left, right) in written {
                    let at = format!("{loc}.require.\"{left}\"");
                    match (Operand::parse(&left), Operand::parse(right)) {
                        (Ok(left), Ok(right)) if left == right => {
                            self.diags.error(at, "both sides are the same value")
                        }
                        (Ok(left), Ok(right)) => rules.push(Requirement { left, right }),
                        (Err(e), _) | (_, Err(e)) => self.diags.error(at, e),
                    }
                }
                Some(OrOff::On(rules))
            }
        };
        layer.merge_from(&own);
        layer
    }

    /// Finds where each `params.<name>` of a layer's `require` rules is in
    /// `path`, the path the layer is written for. `[defaults]`, `[mcp]`
    /// and a tool have none, so such a rule can't be written there: on a
    /// route without that parameter it would have nothing to compare.
    fn place_params(&mut self, layer: &mut Layer, path: Option<&str>, loc: &str) {
        let Some(OrOff::On(rules)) = &mut layer.require else {
            return;
        };
        let segs = path.map(parse_pattern);
        for rule in rules {
            for side in [&mut rule.left, &mut rule.right] {
                let Operand::Param {
                    name,
                    segment,
                    rest,
                } = side
                else {
                    continue;
                };
                let found = segs
                    .iter()
                    .flatten()
                    .enumerate()
                    .find_map(|(i, seg)| match seg {
                        Seg::Param(n) if n == name => Some((i, false)),
                        Seg::CatchAll(n) if n == name => Some((i, true)),
                        _ => None,
                    });
                match (found, path) {
                    (Some((i, all)), _) => (*segment, *rest) = (Some(i), all),
                    (None, Some(path)) => self.diags.push(
                        Diagnostic::error(
                            format!("{loc}.require"),
                            format!("the path {path} has no parameter named \"{name}\""),
                        )
                        .hint(format!("`params.{name}` is a `{{{name}}}` in the path of the entry the rule is written in")),
                    ),
                    (None, None) => self.diags.push(
                        Diagnostic::error(
                            format!("{loc}.require"),
                            format!("`params.{name}` needs a path, and this table has none"),
                        )
                        .hint(format!("write the rule in a `[[route]]` entry for the paths that have it, such as `path = \"/orgs/{{{name}}}/{{*rest}}\"`")),
                    ),
                }
            }
        }
    }

    fn methods(&mut self, raw: Option<Vec<String>>, loc: &str) -> Option<Vec<String>> {
        let list = raw?;
        let mut out = Vec::new();
        for m in list {
            let upper = m.to_ascii_uppercase();
            if !METHODS.contains(&upper.as_str()) {
                self.diags
                    .error(format!("{loc}.method"), format!("unknown method \"{m}\""));
            }
            out.push(upper);
        }
        Some(out)
    }

    fn check_path(&mut self, path: &str, loc: &str) -> bool {
        if !path.starts_with('/') {
            self.diags.error(
                format!("{loc}.path"),
                format!("\"{path}\" must start with `/`"),
            );
            return false;
        }
        let segs = parse_pattern(path);
        if let Some(pos) = segs.iter().position(|s| matches!(s, Seg::CatchAll(_)))
            && pos != segs.len() - 1
        {
            self.diags.error(
                format!("{loc}.path"),
                "`{*name}` matches the rest of the path, so it must come last",
            );
            return false;
        }
        true
    }

    fn schema(&mut self, s: &SchemaRef, loc: &str) -> Option<Value> {
        match s {
            SchemaRef::Inline(map) => Some(Value::Object(map.clone())),
            SchemaRef::File(path) => {
                self.files.push(path.clone());
                let Some(text) = self.host.read(path) else {
                    self.diags
                        .error(loc, format!("schema file \"{path}\" not found"));
                    return None;
                };
                match config::parse_document::<Value>(&text, Format::from_path(path)) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        self.diags.error(loc, format!("{path}: {e}"));
                        None
                    }
                }
            }
        }
    }

    fn raw_validation(&mut self, r: &RawRoute, loc: &str) -> RawValidation {
        let mut v = RawValidation::default();
        let convert = |c: &mut Self,
                       rules: &BTreeMap<String, config::RawParam>,
                       kind: &str,
                       required_default: bool| {
            let mut specs = Vec::new();
            for (name, rule) in rules {
                match param_schema(rule) {
                    Ok(schema) => specs.push(ParamSpec {
                        name: if kind == "headers" {
                            name.to_ascii_lowercase()
                        } else {
                            name.clone()
                        },
                        schema,
                        required: rule.required.unwrap_or(required_default),
                    }),
                    Err(e) => c.diags.error(format!("{loc}.{kind}.{name}"), e),
                }
            }
            specs
        };
        if let Some(p) = &r.params {
            v.params = Some(convert(self, p, "params", true));
        }
        if let Some(q) = &r.query {
            v.query = Some(convert(self, q, "query", false));
        }
        if let Some(h) = &r.headers {
            v.headers = Some(convert(self, h, "headers", false));
        }
        if let Some(b) = &r.body {
            v.body = self.schema(b, &format!("{loc}.body")).map(|s| (s, true));
        }
        if let Some(b) = &r.response {
            v.response = self.schema(b, &format!("{loc}.response"));
        }
        v
    }

    fn config_route(
        &mut self,
        r: &RawRoute,
        source: Source,
        drafts: &mut Vec<Draft>,
        exacts: &mut Vec<ExactEntry>,
        entries: &mut Vec<PolicyEntry>,
    ) {
        let loc = source.loc();
        if !self.check_path(&r.path, &loc) {
            return;
        }
        let methods = self.methods(r.method.as_ref().map(|m| m.to_vec()), &loc);
        let policies = r.policies();
        // A route or an entry is known by its methods and path, which no
        // other statement that sets the same policy can share.
        let counter = format!(
            "{} {}{}",
            label(&methods),
            r.host.as_deref().unwrap_or_default().to_ascii_lowercase(),
            r.path
        );
        let mut layer = self.layer(&policies, &loc, &counter);
        self.place_params(&mut layer, Some(&r.path), &loc);
        let validation = self.raw_validation(r, &loc);
        let has_catch_all = parse_pattern(&r.path)
            .last()
            .is_some_and(|s| matches!(s, Seg::CatchAll(_)));

        match r.action_count() {
            0 if has_catch_all => {
                if r.params.is_some()
                    || r.query.is_some()
                    || r.body.is_some()
                    || r.response.is_some()
                {
                    self.diags.error(
                        &loc,
                        "prefix entries only take policies; put validation on the routes themselves",
                    );
                }
                if r.mcp.is_some() {
                    self.diags.error(
                        format!("{loc}.mcp"),
                        "a prefix entry can't be an MCP tool; set `mcp` on a route with one path",
                    );
                }
                if r.invalidates.is_some() {
                    self.diags.error(
                        format!("{loc}.invalidates"),
                        "`invalidates` belongs to the route that does the writing, or an entry for its path",
                    );
                }
                entries.push(PolicyEntry {
                    segs: parse_pattern(&r.path),
                    path: r.path.clone(),
                    methods,
                    layer,
                    source,
                });
            }
            0 => exacts.push(ExactEntry {
                path: r.path.clone(),
                norm: normalize(&r.path),
                methods,
                layer,
                validation,
                source,
                used: false,
                mcp: r.mcp.clone(),
                shadow: r.shadow.clone(),
                invalidates: r.invalidates.clone(),
            }),
            1 => {
                let action = self.action(r, &loc);
                if let Some(action) = action {
                    drafts.push(Draft {
                        methods,
                        path: r.path.clone(),
                        host: r.host.as_ref().map(|h| h.to_ascii_lowercase()),
                        action,
                        source,
                        own: layer,
                        inherit: Vec::new(),
                        validation,
                        openapi_layer: None,
                        op: None,
                        mcp: r.mcp.clone().map(|m| (m, false)),
                        shadow: r.shadow.clone(),
                        invalidates: r.invalidates.clone(),
                    });
                }
            }
            _ => self.diags.push(
                Diagnostic::error(&loc, "a route can have only one action")
                    .hint("pick one of respond, redirect, static, proxy or handler"),
            ),
        }
    }

    fn action(&mut self, r: &RawRoute, loc: &str) -> Option<Action> {
        if r.runtime.is_some() && r.handler.is_none() {
            self.diags.error(
                format!("{loc}.runtime"),
                "`runtime` only applies to handler routes",
            );
        }
        if let Some(resp) = &r.respond {
            if resp.json.is_some() && resp.text.is_some() {
                self.diags
                    .error(format!("{loc}.respond"), "set `json` or `text`, not both");
            }
            let body = match (&resp.json, &resp.text) {
                (Some(j), _) => RespondBody::Json(j.clone()),
                (None, Some(t)) => RespondBody::Text(t.clone()),
                _ => RespondBody::None,
            };
            return Some(Action::Respond {
                status: resp.status,
                body,
                headers: resp
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                    .collect(),
            });
        }
        if let Some(red) = &r.redirect {
            if !(300..400).contains(&red.status) {
                self.diags.error(
                    format!("{loc}.redirect.status"),
                    "a redirect status must be 3xx",
                );
            }
            return Some(Action::Redirect {
                to: red.to.clone(),
                status: red.status,
            });
        }
        if let Some(st) = &r.static_files {
            if !self.host.exists(&st.dir) {
                self.diags.error(
                    format!("{loc}.static.dir"),
                    format!("directory \"{}\" not found", st.dir),
                );
            }
            return Some(Action::Static {
                dir: st.dir.trim_end_matches('/').to_string(),
                spa_fallback: st.spa_fallback.clone(),
                index: st.index.clone().unwrap_or_else(|| "index.html".into()),
                cache_control: st.cache_control.clone(),
            });
        }
        if let Some(p) = &r.proxy {
            return Some(Action::Proxy(self.proxy(p, loc)));
        }
        let h = r.handler.as_ref()?;
        self.handler_ref(h, r.runtime.as_deref(), &format!("{loc}.handler"))
            .map(Action::Handler)
    }

    fn proxy(&mut self, p: &RawProxy, loc: &str) -> Arc<Proxy> {
        {
            let (upstreams, strip_prefix, sticky, preserve_host, context) = match p {
                RawProxy::Url(u) => (vec![u.clone()], None, None, false, true),
                RawProxy::Full(f) => (
                    f.upstreams.clone(),
                    f.strip_prefix.clone(),
                    f.sticky.clone(),
                    f.preserve_host,
                    f.context.unwrap_or(true),
                ),
            };
            let runtime = match p {
                RawProxy::Full(f) => f.runtime.clone(),
                RawProxy::Url(_) => None,
            };
            if let Some(name) = &runtime {
                let at = format!("{loc}.proxy.runtime");
                match self.runtimes.get(name) {
                    None => self.diags.push(
                        Diagnostic::error(&at, format!("no runtime named \"{name}\""))
                            .hint(format!("add [runtimes.{name}] with protocol = \"http\"")),
                    ),
                    Some(r) if r.protocol != Protocol::Http => self.diags.push(
                        Diagnostic::error(
                            &at,
                            format!("[runtimes.{name}] runs handlers, not an HTTP server"),
                        )
                        .hint(format!("give the route a `handler` in one of its files, or set protocol = \"http\" in [runtimes.{name}]")),
                    ),
                    Some(_) => {}
                }
                if !upstreams.is_empty() {
                    self.diags.error(
                        format!("{loc}.proxy"),
                        "`runtime` and `upstreams` are two places to forward to; pick one",
                    );
                }
                if matches!(p, RawProxy::Full(f) if f.context == Some(false)) {
                    self.diags.error(
                        format!("{loc}.proxy.context"),
                        "a runtime's server is the project's own, and is always told who the caller is",
                    );
                }
                if sticky.is_some() {
                    self.diags.error(
                        format!("{loc}.proxy.sticky"),
                        "`sticky` picks among upstreams; a runtime's workers take requests as they come",
                    );
                }
                return Arc::new(
                    Proxy::new(Vec::new(), strip_prefix, None, preserve_host).to_runtime(runtime),
                );
            }
            let upstreams: Vec<String> = upstreams
                .iter()
                .map(|u| self.subst(u, &format!("{loc}.proxy")))
                .map(|u| u.trim_end_matches('/').to_string())
                .collect();
            if upstreams.is_empty() {
                self.diags.error(format!("{loc}.proxy"), "no upstreams");
            }
            for u in &upstreams {
                if self.unresolved(u) {
                    continue;
                }
                if let Some(path) = u.strip_prefix(UNIX_UPSTREAM) {
                    if !path.starts_with('/') {
                        self.diags.push(
                            Diagnostic::error(
                                format!("{loc}.proxy"),
                                format!("\"{u}\" needs the socket's full path"),
                            )
                            .hint("write it from the root, such as unix:/run/app.sock"),
                        );
                    }
                } else if !(u.starts_with("http://") || u.starts_with("https://")) {
                    self.diags.error(
                        format!("{loc}.proxy"),
                        format!("\"{u}\" must start with http://, https:// or unix:"),
                    );
                }
            }
            let sticky_header = match sticky {
                None => None,
                Some(s) => match s.strip_prefix("header:") {
                    Some(h) => Some(h.trim().to_ascii_lowercase()),
                    None => {
                        self.diags.error(
                            format!("{loc}.proxy.sticky"),
                            "use the form \"header:<name>\", e.g. \"header:x-tenant-id\"",
                        );
                        None
                    }
                },
            };
            Arc::new(
                Proxy::new(upstreams, strip_prefix, sticky_header, preserve_host)
                    .with_context(context),
            )
        }
    }

    /// Drafts the MCP endpoint route and one hidden route per tool.
    fn mcp(
        &mut self,
        raw: &RawMcp,
        lines: &McpLines,
        drafts: &mut Vec<Draft>,
    ) -> Option<McpPending> {
        let source = Source::Mcp {
            file: self.config_file.clone(),
            line: lines.table,
        };
        let loc = source.loc();
        if !self.check_path(&raw.path, &loc) {
            return None;
        }
        if parse_pattern(&raw.path)
            .iter()
            .any(|s| !matches!(s, Seg::Lit(_)))
        {
            self.diags.error(
                format!("{loc}.path"),
                "the MCP endpoint path can't have parameters",
            );
            return None;
        }
        let proxy = raw
            .proxy
            .as_ref()
            .map(|p| self.proxy(p, &format!("{loc}.proxy")));
        let mut layer = self.layer(&raw.policies(), &loc, "[mcp]");
        self.place_params(&mut layer, None, &loc);
        for (key, set) in [
            ("cache", layer.cache.is_some()),
            ("stream_body", layer.stream_body.is_some()),
        ] {
            if set {
                self.diags.push(
                    Diagnostic::error(&loc, format!("`{key}` doesn't apply to an MCP endpoint"))
                        .hint("remove it from the policy sets that [mcp] uses"),
                );
            }
        }
        let list_ttl = match raw.list_ttl.map(|d| d.0) {
            Some(d) if d.is_zero() => {
                self.diags
                    .error(format!("{loc}.list_ttl"), "list_ttl must be longer than 0");
                Duration::from_secs(300)
            }
            Some(d) => d,
            None => Duration::from_secs(300),
        };
        drafts.push(Draft {
            methods: Some(vec!["POST".into()]),
            path: raw.path.clone(),
            host: None,
            action: Action::Mcp,
            source: source.clone(),
            own: layer.clone(),
            inherit: Vec::new(),
            validation: RawValidation::default(),
            openapi_layer: None,
            op: None,
            mcp: None,
            shadow: None,
            invalidates: None,
        });

        let mut tools = Vec::new();
        let mut names: HashMap<String, String> = HashMap::new();
        for (i, t) in raw.tool.iter().enumerate() {
            let source = Source::McpTool {
                file: self.config_file.clone(),
                line: lines.tools.get(i).copied(),
                index: i + 1,
                name: t.name.clone(),
            };
            let tloc = source.loc();
            if let Err(e) = mcp::check_tool_name(&t.name) {
                self.diags.error(format!("{tloc}.name"), e);
                continue;
            }
            if let Some(other) = names.insert(t.name.clone(), source.to_string()) {
                self.diags.error(
                    format!("{tloc}.name"),
                    format!("tool \"{}\" is already defined at {other}", t.name),
                );
                continue;
            }
            let endpoint = (loc.clone(), layer.clone());
            if let Some(t) = self.mcp_tool(t, source, proxy.as_ref(), endpoint, drafts) {
                tools.push(t);
            }
        }
        Some(McpPending {
            endpoint: McpEndpoint {
                path: raw.path.clone(),
                route: 0,
                server: ServerInfo {
                    name: raw.name.clone().unwrap_or_else(|| "unsworn".into()),
                    version: raw.version.clone().unwrap_or_else(|| crate::VERSION.into()),
                    title: raw.title.clone(),
                },
                instructions: raw.instructions.clone(),
                list_tools: raw.list_tools,
                list_ttl,
                proxy,
                tools: Vec::new(),
            },
            tools,
        })
    }

    fn mcp_tool(
        &mut self,
        t: &config::RawMcpTool,
        source: Source,
        proxy: Option<&Arc<Proxy>>,
        endpoint: (String, Layer),
        drafts: &mut Vec<Draft>,
    ) -> Option<McpTool> {
        let loc = source.loc();
        let mut own = self.layer(&t.policies(), &loc, &format!("[[mcp.tool]] {}", t.name));
        self.place_params(&mut own, None, &loc);
        for key in endpoint_only_keys(&own)
            .into_iter()
            .chain(own.cache.is_some().then_some("cache"))
            .chain(own.stream_body.is_some().then_some("stream_body"))
        {
            self.diags.push(
                Diagnostic::error(&loc, format!("`{key}` can't be set on a tool"))
                    .hint("it applies to the whole endpoint; set it in [mcp], or remove it from the policy sets this tool uses"),
            );
        }
        // A tool's policies are checked on top of the endpoint's, so they can
        // only add requirements.
        if matches!(own.auth, Some(AuthReq::Public)) {
            self.diags.push(
                Diagnostic::error(
                    format!("{loc}.auth"),
                    "a tool can't loosen the endpoint's `auth`",
                )
                .hint("tool auth is checked in addition to [mcp] auth; remove `auth = \"none\"`"),
            );
        }
        if matches!(own.rate_limit, Some(OrOff::Off)) {
            self.diags.push(
                Diagnostic::error(format!("{loc}.rate_limit"), "a tool can't switch off the endpoint's rate limit")
                    .hint("the [mcp] rate limit counts every request to the endpoint; a tool's own limit is counted in addition"),
            );
        }

        // Without `input`, a hosted tool takes no arguments, while a proxied
        // tool's arguments are left for the server behind the proxy to check.
        let validate_input = t.input.is_some() || proxy.is_none();
        let input = match &t.input {
            Some(s) => self.schema(s, &format!("{loc}.input"))?,
            None if proxy.is_some() => serde_json::json!({"type": "object"}),
            None => serde_json::json!({"type": "object", "additionalProperties": false}),
        };
        if let Err(e) = mcp::check_input_schema(&input) {
            self.diags.error(format!("{loc}.input"), e);
        }
        let param_headers = match mcp::param_headers(&input) {
            Ok(h) => h,
            Err(errors) => {
                for e in errors {
                    self.diags.error(format!("{loc}.input"), e);
                }
                Vec::new()
            }
        };
        let output = match &t.output {
            Some(s) => Some(self.schema(s, &format!("{loc}.output"))?),
            None => None,
        };

        let action = match proxy {
            Some(p) => {
                for (key, set) in [
                    ("handler", t.handler.is_some()),
                    ("runtime", t.runtime.is_some()),
                    ("output", t.output.is_some()),
                    ("title", t.title.is_some()),
                    ("description", t.description.is_some()),
                    ("annotations", t.annotations.is_some()),
                ] {
                    if set {
                        self.diags.push(
                            Diagnostic::error(
                                format!("{loc}.{key}"),
                                format!("`{key}` doesn't apply to a tool of a proxied MCP server"),
                            )
                            .hint("the server behind the proxy defines its tools; here a tool only adds policies and input validation"),
                        );
                    }
                }
                Action::Proxy(p.clone())
            }
            None => {
                let Some(h) = &t.handler else {
                    self.diags.push(
                        Diagnostic::error(&loc, format!("tool \"{}\" has no handler", t.name))
                            .hint("set `handler`, or set `proxy` in [mcp] to forward to an existing MCP server"),
                    );
                    return None;
                };
                if t.description.is_none() {
                    self.diags.push(
                        Diagnostic::warning(
                            &loc,
                            format!("tool \"{}\" has no description", t.name),
                        )
                        .hint("models choose tools by their descriptions"),
                    );
                }
                Action::Handler(self.handler_ref(
                    h,
                    t.runtime.as_deref(),
                    &format!("{loc}.handler"),
                )?)
            }
        };

        let input_for_rule = input.clone();
        drafts.push(Draft {
            methods: Some(vec!["POST".into()]),
            path: format!("mcp:{}", t.name),
            host: None,
            action,
            source: source.clone(),
            own: own.clone(),
            inherit: vec![endpoint],
            validation: RawValidation {
                body: validate_input.then(|| (input.clone(), true)),
                response: output.clone(),
                ..Default::default()
            },
            openapi_layer: None,
            op: None,
            mcp: None,
            shadow: None,
            invalidates: None,
        });
        Some(McpTool {
            name: t.name.clone(),
            title: t.title.clone(),
            description: t.description.clone(),
            input_schema: input,
            output_schema: output,
            annotations: t.annotations.as_ref().and_then(mcp::annotations),
            param_headers,
            route: 0,
            own,
            input_rule: validate_input
                .then(|| crate::validate::BodyRule::new(input_for_rule, true, self.schemas).ok())
                .flatten(),
            binding: None,
        })
    }

    /// Turns each route marked with `mcp = {...}` into an MCP tool whose
    /// arguments are built from the route's contract.
    fn compile_route_tools(
        &mut self,
        routes: &[Route],
        entries: &[PolicyEntry],
        pending: Option<&McpPending>,
    ) -> Vec<McpTool> {
        let decls = std::mem::take(&mut self.route_tools);
        let mut tools: Vec<McpTool> = Vec::new();
        let mut names: HashMap<String, String> = pending
            .map(|p| {
                p.tools
                    .iter()
                    .map(|t| (t.name.clone(), "[[mcp.tool]]".to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let endpoint_auth = routes
            .iter()
            .find(|r| matches!(r.action, Action::Mcp))
            .zip(pending)
            .map(|(r, p)| {
                merged_layer(r, entries, "POST", &p.endpoint.path)
                    .finalize(Duration::from_secs(30), 0)
                    .auth
            });
        for (id, decl, at, bulk) in decls {
            let at = format!("{at}.mcp");
            let tools_before = tools.len();
            // For a bulk pick, the checks after the name's are downgraded.
            let mut diags_before = None;
            'tool: {
                let Some(pending) = pending else {
                    self.diags.push(
                        Diagnostic::error(
                            &at,
                            "offering a route as an MCP tool needs an [mcp] table",
                        )
                        .hint("add [mcp], which sets the endpoint the tools are served on"),
                    );
                    break 'tool;
                };
                if pending.endpoint.proxy.is_some() {
                    self.diags.push(
                        Diagnostic::error(&at, "routes can't be tools of a proxied MCP server")
                            .hint("remove `proxy` from [mcp] so Unsworn is the MCP server"),
                    );
                    break 'tool;
                }
                let Some(route) = routes.iter().find(|r| r.id == id) else {
                    break 'tool;
                };
                if let Err(e) = mcp::check_tool_name(&decl.tool) {
                    self.diags.error(format!("{at}.tool"), e);
                    break 'tool;
                }
                if let Some(other) = names.insert(decl.tool.clone(), at.clone()) {
                    self.diags.error(
                        format!("{at}.tool"),
                        format!("tool \"{}\" is already defined at {other}", decl.tool),
                    );
                    break 'tool;
                }
                diags_before = Some(self.diags.items.len());
                if !matches!(route.action, Action::Handler(_)) {
                    self.diags.push(
                        Diagnostic::error(
                            &at,
                            format!(
                                "{} {} has no handler to run as a tool",
                                route.method_label(),
                                route.path
                            ),
                        )
                        .hint("only routes with a `handler` can be MCP tools"),
                    );
                    break 'tool;
                }
                let method = match (&decl.method, &route.methods) {
                    (Some(m), methods) => {
                        let m = m.to_ascii_uppercase();
                        if methods.as_ref().is_some_and(|list| !list.contains(&m)) {
                            self.diags.error(
                                format!("{at}.method"),
                                format!("the route doesn't accept {m}"),
                            );
                            break 'tool;
                        }
                        m
                    }
                    (None, Some(list)) if list.len() == 1 => list[0].clone(),
                    (None, _) => {
                        self.diags.push(
                            Diagnostic::error(&at, format!("{} {} accepts several methods", route.method_label(), route.path))
                                .hint("set the one tool calls use, e.g. mcp = { tool = \"...\", method = \"GET\" }"),
                        );
                        break 'tool;
                    }
                };
                let probe = route.path.replace("{*", "{");
                let eff = merged_layer(route, entries, &method, &probe)
                    .finalize(Duration::from_secs(30), 0);
                if eff.stream_body {
                    self.diags.push(
                        Diagnostic::error(&at, "a route with `stream_body` can't be an MCP tool")
                            .hint("tool arguments arrive as one JSON object, not a stream"),
                    );
                    break 'tool;
                }
                if let Some(h) = route
                    .validation
                    .headers
                    .as_ref()
                    .and_then(|h| h.specs.iter().find(|s| s.required))
                {
                    self.diags.push(
                        Diagnostic::error(
                            &at,
                            format!(
                                "the route requires the header {}, which MCP clients don't send",
                                h.name
                            ),
                        )
                        .hint("make the header optional, or don't offer this route as a tool"),
                    );
                    break 'tool;
                }
                // The tool call's credentials are the ones sent to /mcp.
                if let (AuthReq::AnyOf(route_alts), Some(AuthReq::AnyOf(mcp_alts))) =
                    (&eff.auth, &endpoint_auth)
                    && !route_alts
                        .iter()
                        .any(|a| mcp_alts.iter().any(|m| m.scheme == a.scheme))
                {
                    let names = |alts: &[crate::policy::AuthAlt]| {
                        alts.iter()
                            .map(|a| a.scheme.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    self.diags.push(
                        Diagnostic::error(
                            &at,
                            format!(
                                "the route's auth ({}) shares no scheme with [mcp] auth ({}), so no tool call could pass it",
                                names(route_alts),
                                names(mcp_alts)
                            ),
                        )
                        .hint("tool calls carry the credentials sent to the MCP endpoint; let the route also accept the [mcp] scheme"),
                    );
                    break 'tool;
                }

                let Some((input, binding)) = self.route_tool_schema(route, &method, &at) else {
                    break 'tool;
                };
                let input_rule =
                    match crate::validate::BodyRule::new(input.clone(), true, self.schemas) {
                        Ok(r) => r,
                        Err(e) => {
                            self.diags.error(&at, e);
                            break 'tool;
                        }
                    };
                let param_headers = match mcp::param_headers(&input) {
                    Ok(h) => h,
                    Err(errors) => {
                        for e in errors {
                            self.diags.error(&at, e);
                        }
                        break 'tool;
                    }
                };
                let description = decl.description.clone().or_else(|| {
                    route
                        .op
                        .as_ref()
                        .and_then(|op| op.description.clone().or_else(|| op.summary.clone()))
                });
                if description.is_none() {
                    self.diags.push(
                        Diagnostic::warning(
                            &at,
                            format!("tool \"{}\" has no description", decl.tool),
                        )
                        .hint("models choose tools by their descriptions; set `description`"),
                    );
                }
                let own = Layer {
                    auth: matches!(eff.auth, AuthReq::AnyOf(_)).then(|| eff.auth.clone()),
                    rate_limit: eff.rate_limit.clone().map(OrOff::On),
                    ..Default::default()
                };
                tools.push(McpTool {
                    name: decl.tool.clone(),
                    title: decl.title.clone(),
                    description,
                    input_schema: input,
                    output_schema: route.validation.response.as_ref().map(|r| r.schema.clone()),
                    annotations: decl.annotations.as_ref().and_then(mcp::annotations),
                    param_headers,
                    route: route.id,
                    own,
                    input_rule: Some(input_rule),
                    binding: Some(binding),
                });
            }
            // An operation picked in bulk that can't be a tool is skipped,
            // not an error.
            if let Some(from) = diags_before.filter(|_| bulk && tools.len() == tools_before) {
                for d in &mut self.diags.items[from..] {
                    if d.is_error() {
                        d.level = crate::diag::Level::Warning;
                        d.message
                            .push_str("; the operation isn't offered as an MCP tool");
                    }
                }
            }
        }
        tools
    }

    /// A route tool's input schema: path and query parameters as top-level
    /// arguments, and the body's fields next to them when that's
    /// unambiguous, or else under `body`.
    fn route_tool_schema(
        &mut self,
        route: &Route,
        method: &str,
        at: &str,
    ) -> Option<(Value, RouteBinding)> {
        let mut properties = serde_json::Map::new();
        let mut required: Vec<Value> = Vec::new();
        let param_specs = route.validation.params.as_ref().map(|p| &p.specs);
        for name in &route.param_names {
            let schema = param_specs
                .and_then(|specs| specs.iter().find(|s| &s.name == name))
                .map(|s| s.schema.clone())
                .unwrap_or_else(|| serde_json::json!({"type": "string"}));
            properties.insert(name.clone(), schema);
            if Some(name) != route.catch_all.as_ref() {
                required.push(Value::String(name.clone()));
            }
        }
        let mut query = Vec::new();
        for spec in route.validation.query.iter().flat_map(|q| &q.specs) {
            if properties.contains_key(&spec.name) {
                self.diags.error(
                    at,
                    format!("\"{}\" is both a path and a query parameter, so tool arguments can't tell them apart", spec.name),
                );
                return None;
            }
            properties.insert(spec.name.clone(), spec.schema.clone());
            if spec.required {
                required.push(Value::String(spec.name.clone()));
            }
            query.push((spec.name.clone(), spec.schema.get("default").cloned()));
        }

        let mut additional = Some(Value::Bool(false));
        let body = match &route.validation.body {
            None => {
                if matches!(method, "POST" | "PUT" | "PATCH") {
                    self.diags.push(
                        Diagnostic::warning(
                            at,
                            format!(
                                "{method} {} has no `body` schema, so its tool can't send a body",
                                route.path
                            ),
                        )
                        .hint("add `body` to the route to let tool calls send one"),
                    );
                }
                BodyArgs::None
            }
            Some(rule) => {
                const FLAT_KEYS: &[&str] = &[
                    "type",
                    "properties",
                    "required",
                    "additionalProperties",
                    "title",
                    "description",
                    "$schema",
                ];
                let schema = &rule.schema;
                let flat = rule.required
                    && schema.get("type").and_then(Value::as_str) == Some("object")
                    && schema
                        .as_object()
                        .is_some_and(|o| o.keys().all(|k| FLAT_KEYS.contains(&k.as_str())))
                    && schema
                        .get("properties")
                        .and_then(Value::as_object)
                        .is_some_and(|p| p.keys().all(|k| !properties.contains_key(k)));
                if flat {
                    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                        for (k, v) in props {
                            properties.insert(k.clone(), v.clone());
                        }
                    }
                    if let Some(list) = schema.get("required").and_then(Value::as_array) {
                        required.extend(list.iter().cloned());
                    }
                    // Extra arguments go to the body when the body allows them.
                    additional = schema.get("additionalProperties").cloned();
                    BodyArgs::Flat
                } else if properties.contains_key("body") {
                    self.diags.error(
                        at,
                        "a parameter named \"body\" leaves no argument for the request body",
                    );
                    return None;
                } else {
                    properties.insert("body".into(), schema.clone());
                    if rule.required {
                        required.push(Value::String("body".into()));
                    }
                    BodyArgs::Nested
                }
            }
        };

        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), Value::String("object".into()));
        schema.insert("properties".into(), Value::Object(properties));
        if !required.is_empty() {
            schema.insert("required".into(), Value::Array(required));
        }
        if let Some(a) = additional {
            schema.insert("additionalProperties".into(), a);
        }
        Some((
            Value::Object(schema),
            RouteBinding {
                method: method.to_string(),
                pattern: route.path.clone(),
                params: route.param_names.clone(),
                query,
                body,
            },
        ))
    }

    fn route_files(&mut self, dir: &str, drafts: &mut Vec<Draft>) {
        let files = match self.host.list(dir) {
            Ok(f) => f,
            Err(e) => {
                self.diags.push(
                    Diagnostic::error("[routes].dir", format!("can't read \"{dir}\": {e}"))
                        .hint("create the directory, or remove [routes]"),
                );
                return;
            }
        };
        let scan = match routefiles::scan(&files) {
            Ok(s) => s,
            Err(errors) => {
                for (file, e) in errors {
                    let e = e.replace(routefiles::ROUTES_DIR, dir);
                    self.diags.error(format!("{dir}/{file}"), e);
                }
                return;
            }
        };

        for fr in scan.routes {
            let path = format!("{dir}/{}", fr.file);
            let Some(h) = self.handler_ref(&path, None, &path) else {
                continue;
            };
            drafts.push(Draft {
                methods: Some(vec![fr.method]),
                path: fr.path,
                host: None,
                action: Action::Handler(h),
                source: Source::RouteFile { path },
                own: Layer::default(),
                inherit: Vec::new(),
                validation: RawValidation::default(),
                openapi_layer: None,
                op: None,
                mcp: None,
                shadow: None,
                invalidates: None,
            });
        }
    }

    /// Which OpenAPI operations `[openapi] mcp_tools` offers as tools.
    fn tool_pick(&mut self, oa: &config::RawOpenApi, mcp: Option<&RawMcp>) -> Option<ToolPick> {
        let loc = "[openapi].mcp_tools";
        let pick = match oa.mcp_tools.as_ref()? {
            config::RawOpenApiTools::All(s) if s == "all" => ToolPick::All,
            config::RawOpenApiTools::All(s) => {
                self.diags.push(
                    Diagnostic::error(loc, format!("\"{s}\" isn't a valid choice"))
                        .hint("use \"all\", or { tags = [\"...\"] }"),
                );
                return None;
            }
            config::RawOpenApiTools::Tags(t) => ToolPick::Tags(t.tags.clone()),
        };
        match mcp {
            None => {
                self.diags.push(
                    Diagnostic::error(loc, "offering operations as MCP tools needs an [mcp] table")
                        .hint("add [mcp], which sets the endpoint the tools are served on"),
                );
                return None;
            }
            Some(m) if m.proxy.is_some() => {
                self.diags.push(
                    Diagnostic::error(loc, "operations can't be tools of a proxied MCP server")
                        .hint("remove `proxy` from [mcp] so Unsworn is the MCP server"),
                );
                return None;
            }
            Some(_) => {}
        }
        if oa.spec.is_none() {
            self.diags.push(Diagnostic::warning(
                loc,
                "there's no `spec`, so no operations to offer",
            ));
            return None;
        }
        Some(pick)
    }

    fn openapi(
        &mut self,
        spec: &str,
        template: Option<&str>,
        pick: Option<&ToolPick>,
        drafts: &mut Vec<Draft>,
    ) -> Option<Value> {
        self.files.push(spec.to_string());
        let loc = "[openapi].spec";
        let Some(text) = self.host.read(spec) else {
            self.diags.error(loc, format!("file \"{spec}\" not found"));
            return None;
        };
        let doc: Value = match config::parse_document(&text, Format::from_path(spec)) {
            Ok(d) => d,
            Err(e) => {
                self.diags.error(spec, e);
                return None;
            }
        };
        let ops = match openapi::import(&doc) {
            Ok(ops) => ops,
            Err(errors) => {
                for e in errors {
                    self.diags.error(spec, e);
                }
                return Some(doc);
            }
        };
        let mut picked = 0;
        for op in ops {
            let source = Source::OpenApi {
                file: spec.to_string(),
                location: op.location.clone(),
            };
            let validation = RawValidation {
                params: (!op.params.is_empty()).then(|| op.params.clone()),
                query: (!op.query.is_empty()).then(|| op.query.clone()),
                headers: (!op.headers.is_empty()).then(|| op.headers.clone()),
                body: op.body.clone(),
                response: op.response.clone(),
            };
            let layer = op.security.as_ref().map(|sec| {
                let auth = if sec.is_empty() {
                    AuthReq::Public
                } else {
                    for (scheme, _) in sec {
                        if !self.schemes.contains_key(scheme) {
                            self.diags.push(
                                Diagnostic::error(
                                    source.to_string(),
                                    format!("security scheme \"{scheme}\" has no matching [auth.{scheme}]"),
                                )
                                .hint("name the [auth.*] scheme after the spec's security scheme"),
                            );
                        }
                    }
                    AuthReq::AnyOf(
                        sec.iter()
                            .map(|(scheme, scopes)| AuthAlt {
                                scheme: scheme.clone(),
                                scopes: scopes.clone(),
                            })
                            .collect(),
                    )
                };
                Layer {
                    auth: Some(auth),
                    ..Default::default()
                }
            });
            let info = OpInfo {
                operation_id: op.operation_id.clone(),
                summary: op.summary.clone(),
                description: op.description.clone(),
                tags: op.tags.clone(),
            };
            let tool = match pick {
                Some(p) if p.matches(&op.tags) => {
                    picked += 1;
                    match &op.operation_id {
                        Some(id) => Some(RawRouteTool {
                            tool: id.clone(),
                            ..Default::default()
                        }),
                        None => {
                            self.diags.push(
                                Diagnostic::warning(
                                    source.to_string(),
                                    "the operation has no operationId, so it isn't offered as an MCP tool",
                                )
                                .hint("add an operationId; it becomes the tool's name"),
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            // A config route with an action can serve this operation.
            let norm = normalize(&op.path);
            if let Some(existing) = drafts.iter_mut().find(|d| {
                normalize(&d.path) == norm
                    && d.methods.as_ref().is_some_and(|m| m.contains(&op.method))
                    && matches!(d.source, Source::Config { .. })
            }) {
                let mut v = validation;
                v.overlay(&existing.validation);
                existing.validation = v;
                existing.openapi_layer = layer;
                existing.op = Some(info);
                // An `mcp` on the config route itself wins.
                if existing.mcp.is_none() {
                    existing.mcp = tool.map(|t| (t, true));
                }
                continue;
            }
            let handler = match (&op.handler, template) {
                (Some(h), _) => Some(h.clone()),
                (None, Some(t)) => match openapi::handler_from_template(t, &op) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        self.diags.error(source.to_string(), e);
                        continue;
                    }
                },
                (None, None) => None,
            };
            let Some(handler) = handler else {
                self.diags.push(
                    Diagnostic::error(source.to_string(), "operation has no handler")
                        .hint("add `x-unsworn-handler`, set `[openapi] handler` to a template, or give it a `[[route]]` with an action"),
                );
                continue;
            };
            let Some(h) = self.handler_ref(&handler, None, &source.to_string()) else {
                continue;
            };
            drafts.push(Draft {
                methods: Some(vec![op.method.clone()]),
                path: op.path.clone(),
                host: None,
                action: Action::Handler(h),
                source,
                own: Layer::default(),
                inherit: Vec::new(),
                validation,
                openapi_layer: layer,
                op: Some(info),
                mcp: tool.map(|t| (t, true)),
                shadow: None,
                invalidates: None,
            });
        }
        if pick.is_some() && picked == 0 {
            self.diags.push(Diagnostic::warning(
                "[openapi].mcp_tools",
                "no operations match, so none are offered as MCP tools",
            ));
        }
        Some(doc)
    }

    fn finish_route(
        &mut self,
        id: usize,
        d: Draft,
        defaults: &Layer,
        exacts: &mut [ExactEntry],
    ) -> Option<Route> {
        let loc = d.source.loc();
        let mut layers: Vec<(String, Layer)> = Vec::new();
        if !defaults.is_empty() {
            layers.push((Source::Defaults.to_string(), defaults.clone()));
        }
        if let Some(l) = &d.openapi_layer {
            let name = match &d.source {
                Source::OpenApi { file, .. } => file.clone(),
                _ => "[openapi].spec".to_string(),
            };
            layers.push((name, l.clone()));
        }
        let mut low = Layer::default();
        for (_, l) in &layers {
            low.merge_from(l);
        }

        let mut high = Layer::default();
        let mut high_layers = Vec::new();
        for (name, l) in &d.inherit {
            high.merge_from(l);
            if !l.is_empty() {
                high_layers.push((name.clone(), l.clone()));
            }
        }
        high.merge_from(&d.own);
        if !d.own.is_empty() {
            high_layers.push((loc.clone(), d.own.clone()));
        }
        let mut raw_validation = d.validation.clone();
        let mut tool = d.mcp.clone().map(|(m, bulk)| (m, loc.clone(), bulk));
        let mut shadow = d.shadow.clone().map(|s| (s, loc.clone()));
        let mut invalidates = d.invalidates.clone().map(|list| (list, loc.clone()));
        let norm = normalize(&d.path);
        for e in exacts.iter_mut() {
            if e.norm != norm {
                continue;
            }
            let applies = match (&e.methods, &d.methods) {
                (None, _) | (_, None) => true,
                (Some(a), Some(b)) => a.iter().any(|m| b.contains(m)),
            };
            if !applies {
                continue;
            }
            e.used = true;
            // The entry's policies apply by request, among the other
            // entries (see `merged_layer`). What it says about the
            // contract belongs to the route, which checks every request
            // the same way whatever its method.
            let v = &e.validation;
            let validates = v.params.is_some()
                || v.query.is_some()
                || v.headers.is_some()
                || v.body.is_some()
                || v.response.is_some();
            let every_method = match (&e.methods, &d.methods) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(entry), Some(route)) => {
                    let entry = covered_methods(entry);
                    route.iter().all(|m| entry.contains(&m.as_str()))
                }
            };
            if validates && !every_method {
                self.diags.push(
                    Diagnostic::error(
                        e.source.loc(),
                        format!(
                            "validation for {} only, on a route that takes {}: {} {} checks every request the same way",
                            label(&e.methods),
                            match &d.methods {
                                None => "every method".to_string(),
                                Some(m) => m.join(", "),
                            },
                            label(&d.methods),
                            d.path
                        ),
                    )
                    .hint("drop `method` from this entry, or give each method a route of its own"),
                );
                continue;
            }
            raw_validation.overlay(&e.validation);
            if let Some(s) = &e.shadow {
                shadow = Some((s.clone(), e.source.loc()));
            }
            if let Some(list) = &e.invalidates {
                invalidates = Some((list.clone(), e.source.loc()));
            }
            if let Some(m) = &e.mcp {
                match &tool {
                    // An explicit `mcp` replaces a bulk pick.
                    Some((_, first, false)) => self.diags.error(
                        format!("{}.mcp", e.source.loc()),
                        format!("this route is already offered as a tool at {first}"),
                    ),
                    _ => tool = Some((m.clone(), e.source.loc(), false)),
                }
            }
        }
        if let Some((m, at, bulk)) = tool {
            self.route_tools.push((id, m, at, bulk));
        }
        let low_layers = layers.len();
        layers.extend(high_layers);

        let segs = parse_pattern(&d.path);
        let param_names: Vec<String> = segs
            .iter()
            .filter_map(|s| match s {
                Seg::Param(n) | Seg::CatchAll(n) => Some(n.clone()),
                Seg::Lit(_) => None,
            })
            .collect();
        let catch_all = segs.iter().find_map(|s| match s {
            Seg::CatchAll(n) => Some(n.clone()),
            _ => None,
        });

        let mut validation = Validation::default();
        if let Some(params) = &raw_validation.params {
            for p in params {
                if !param_names.contains(&p.name) {
                    self.diags.error(
                        format!("{loc}.params.{}", p.name),
                        format!("the path {} has no parameter named \"{}\"", d.path, p.name),
                    );
                }
            }
            let mut specs = params.clone();
            for name in &param_names {
                if !specs.iter().any(|s| &s.name == name) {
                    specs.push(ParamSpec {
                        name: name.clone(),
                        schema: serde_json::json!({"type": "string"}),
                        required: Some(name) != catch_all.as_ref(),
                    });
                }
            }
            validation.params = self.location("params", specs, &loc);
        }
        if let Some(q) = &raw_validation.query {
            validation.query = self.location("query", q.clone(), &loc);
        }
        if let Some(h) = &raw_validation.headers {
            validation.headers = self.location("headers", h.clone(), &loc);
        }
        if let Some((schema, required)) = &raw_validation.body {
            match BodyRule::new(schema.clone(), *required, self.schemas) {
                Ok(r) => validation.body = Some(r),
                Err(e) => self.diags.error(format!("{loc}.body"), e),
            }
        }
        if let Some(schema) = &raw_validation.response {
            match BodyRule::new(schema.clone(), false, self.schemas) {
                Ok(r) => validation.response = Some(r),
                Err(e) => self.diags.error(format!("{loc}.response"), e),
            }
        }
        if validation.body.is_some() && !matches!(d.action, Action::Handler(_) | Action::Proxy(_)) {
            self.diags.push(Diagnostic::warning(
                &loc,
                "`body` validation only matters for handler and proxy routes",
            ));
        }

        let invalidates: Vec<Invalidates> = invalidates
            .map(|(list, at)| {
                let at = format!("{at}.invalidates");
                let mut out = Vec::new();
                for path in list {
                    if !self.check_path(&path, &at) {
                        continue;
                    }
                    let segs = parse_pattern(&path);
                    // Each parameter is filled in from the request, so it
                    // has to be one the route's own path has.
                    let unknown = segs.iter().find_map(|seg| match seg {
                        Seg::Param(n) | Seg::CatchAll(n) if !param_names.contains(n) => Some(n),
                        _ => None,
                    });
                    if let Some(name) = unknown {
                        self.diags.push(
                            Diagnostic::error(
                                &at,
                                format!("\"{path}\" has `{{{name}}}`, which the route's path {} doesn't", d.path),
                            )
                            .hint("a path here is filled in from the request, so it can only use the route's own parameters"),
                        );
                        continue;
                    }
                    out.push(Invalidates { path, segs });
                }
                out
            })
            .unwrap_or_default();

        let shadow = shadow.and_then(|(raw, at)| {
            let at = format!("{at}.shadow");
            let (handler, runtime, sample) = match &raw {
                config::RawShadow::Handler(h) => (h.as_str(), None, 1.0),
                config::RawShadow::Full(f) => (
                    f.handler.as_str(),
                    f.runtime.as_deref(),
                    f.sample.unwrap_or(1.0),
                ),
            };
            let Action::Handler(own) = &d.action else {
                self.diags.error(
                    &at,
                    "`shadow` compares with a route's handler, and this route has none",
                );
                return None;
            };
            if !(sample > 0.0 && sample <= 1.0) {
                self.diags.error(
                    format!("{at}.sample"),
                    "sample is a share of requests: more than 0, and 1 at most",
                );
                return None;
            }
            let handler = self.handler_ref(handler, runtime, &at)?;
            if handler == *own {
                self.diags
                    .error(&at, "the shadow is the route's own handler");
                return None;
            }
            if !self.host.exists(&handler.file) {
                self.diags.push(
                    Diagnostic::error(&at, format!("handler file \"{}\" not found", handler.file))
                        .hint("handler paths are relative to the project root"),
                );
            }
            Some(Shadow {
                handler,
                every: (1.0 / sample).round().max(1.0) as u64,
                seen: Arc::default(),
            })
        });

        Some(Route {
            id,
            methods: d.methods,
            path: d.path,
            host: d.host,
            action: d.action,
            source: d.source,
            validation,
            low,
            high,
            layers,
            low_layers,
            wider: Vec::new(),
            narrower: Vec::new(),
            param_names,
            catch_all,
            segs,
            op: d.op,
            shadow,
            invalidates,
        })
    }

    fn location(
        &mut self,
        prefix: &'static str,
        specs: Vec<ParamSpec>,
        loc: &str,
    ) -> Option<LocationRules> {
        match LocationRules::new(prefix, specs, self.schemas) {
            Ok(r) => Some(r),
            Err(e) => {
                self.diags.error(format!("{loc}.{prefix}"), e);
                None
            }
        }
    }

    fn check_handlers(&mut self, routes: &[Route], entries: &[PolicyEntry]) {
        let mut checked = std::collections::HashSet::new();
        let mut check = |c: &mut Self, h: &HandlerRef, loc: String| {
            if h.runtime == crate::dimedb::RUNTIME {
                return;
            }
            if checked.insert(h.file.clone()) && !c.host.exists(&h.file) {
                c.diags.push(
                    Diagnostic::error(loc, format!("handler file \"{}\" not found", h.file))
                        .hint("handler paths are relative to the project root"),
                );
            }
        };
        for r in routes {
            if let Some(h) = r.handler() {
                check(self, h, format!("{}.handler", r.source.loc()));
            }
            for (src, l) in &r.layers {
                if let Some(OrOff::On(list)) = &l.before {
                    for h in list {
                        check(self, h, format!("{src}.before"));
                    }
                }
            }
        }
        for p in entries {
            if let Some(OrOff::On(list)) = &p.layer.before {
                for h in list {
                    check(self, h, format!("{}.before", p.source.loc()));
                }
            }
        }
    }

    /// Two statements about the same requests that set the same policy.
    /// Neither is the narrower one, so there is no saying which should
    /// win: one of them has to go.
    fn conflicts(&mut self, routes: &[Route], entries: &[PolicyEntry]) {
        struct Statement<'a> {
            segs: &'a [Seg],
            methods: Option<&'a [String]>,
            layer: &'a Layer,
            loc: String,
            path: &'a str,
        }
        let mut statements: Vec<Statement> = entries
            .iter()
            .map(|e| Statement {
                segs: &e.segs,
                methods: e.methods.as_deref(),
                layer: &e.layer,
                loc: e.source.loc(),
                path: &e.path,
            })
            .collect();
        // A route's own policies are a statement about its path and
        // methods. One for a `host` is narrower than any entry, and a
        // tool's has no path.
        statements.extend(
            routes
                .iter()
                .filter(|r| r.host.is_none() && r.mcp_tool().is_none() && !r.high.is_empty())
                .map(|r| Statement {
                    segs: &r.segs,
                    methods: r.methods.as_deref(),
                    layer: &r.high,
                    loc: r.source.loc(),
                    path: &r.path,
                }),
        );
        for (i, b) in statements.iter().enumerate() {
            for a in &statements[..i] {
                if normalize_segs(a.segs) != normalize_segs(b.segs) {
                    continue;
                }
                // With every method, or with all of the other's and more,
                // one of them is the wider.
                let shared: Vec<&str> = match (a.methods, b.methods) {
                    (None, None) => vec!["*"],
                    (None, Some(_)) | (Some(_), None) => continue,
                    (Some(x), Some(y)) => {
                        let (x, y) = (covered_methods(x), covered_methods(y));
                        let shared: Vec<&str> =
                            x.iter().copied().filter(|m| y.contains(m)).collect();
                        if shared.is_empty()
                            || (shared.len() == x.len()) != (shared.len() == y.len())
                        {
                            continue;
                        }
                        shared
                    }
                };
                for key in a.layer.keys() {
                    // `before` and `require` lists add up, so two of
                    // them don't clash.
                    if matches!(key, "before" | "require") || !b.layer.keys().contains(&key) {
                        continue;
                    }
                    self.diags.push(
                        Diagnostic::error(
                            format!("{}.{key}", b.loc),
                            format!(
                                "`{key}` for {} {} is also set at {}",
                                shared.join(", "),
                                b.path,
                                a.loc
                            ),
                        )
                        .hint("two entries for the same requests can't both set a policy: keep one, or narrow one of them with `method` or a longer path"),
                    );
                }
            }
        }
    }

    fn warnings(&mut self, routes: &[Route], entries: &[PolicyEntry]) {
        // Entries already reported as guarding only some spellings of a path.
        let mut reported: Vec<String> = Vec::new();
        // Tools never stream or cache; `[mcp]` checks their policies.
        for r in routes.iter().filter(|r| r.mcp_tool().is_none()) {
            let probe = r.path.replace("{*", "{");
            // A route's policies differ by method when an entry is for
            // some of its methods only, so each is looked at.
            let mut methods = route_methods(r);
            for &i in r.wider.iter().chain(&r.narrower) {
                for m in entries[i].methods.iter().flatten() {
                    if r.allows(m) && !methods.contains(&m.as_str()) {
                        methods.push(m);
                    }
                }
            }
            let first = self.diags.items.len();
            let mut whole_route = None;
            let mut auth_unset: Vec<&str> = Vec::new();
            let mut callerless: Vec<(&str, Requirement)> = Vec::new();
            for method in methods {
                let layer = merged_layer(r, entries, method, &probe);
                if layer.auth.is_none() {
                    auth_unset.push(method);
                }
                let eff = layer.finalize(Duration::from_secs(30), 0);
                if eff.auth == AuthReq::Public {
                    for rule in eff.require.iter().filter(|rule| rule.needs_caller()) {
                        if !callerless.iter().any(|(_, seen)| seen == rule) {
                            callerless.push((method, rule.clone()));
                        }
                    }
                }
                whole_route.get_or_insert_with(|| eff.clone());
                if let Some(shadow) = &r.shadow {
                    if eff.stream_body {
                        self.diags.push(
                        Diagnostic::error(
                            format!("{}", r.source),
                            format!(
                                "{} {} sets both `shadow` and `stream_body`, but a streamed body can only be read once",
                                r.method_label(),
                                r.path
                            ),
                        )
                        .hint("remove `shadow`, or let Unsworn read the body first"),
                    );
                    }
                    // Whatever the shadow writes, it writes on top of what the
                    // route's own handler wrote.
                    let writes = r
                        .methods
                        .as_ref()
                        .is_none_or(|m| m.iter().any(|m| !matches!(m.as_str(), "GET" | "HEAD")));
                    if writes {
                        self.diags.push(
                        Diagnostic::warning(
                            format!("{}.shadow", r.source.loc()),
                            format!(
                                "{} runs for every {} {} as well as the route's handler: anything it writes or sends is done twice",
                                shadow.handler.raw,
                                r.method_label(),
                                r.path
                            ),
                        )
                        .hint("shadow a route that only reads, or make sure the shadow handler changes nothing"),
                    );
                    }
                }
                if eff.stream_body && signs_body(&self.schemes, &eff.auth) {
                    self.diags.push(
                    Diagnostic::error(
                        format!("{}", r.source),
                        format!(
                            "{} {} sets `stream_body`, but its `auth` checks a signature made over the whole body, which has to be read first",
                            r.method_label(),
                            r.path
                        ),
                    )
                    .hint("remove `stream_body` from this route"),
                );
                }
                if eff.stream_body && r.validation.body.is_some() {
                    self.diags.push(
                    Diagnostic::error(
                        format!("{}", r.source),
                        format!(
                            "{} {} sets both `stream_body` and `body` validation, but a streamed body isn't read before the handler runs",
                            r.method_label(),
                            r.path
                        ),
                    )
                    .hint("remove `body`, or set `stream_body = false` for this route"),
                );
                }
                if let (Some(cache), AuthReq::AnyOf(_)) = (&eff.cache, &eff.auth)
                    && !rules::cache_separates_users(
                        cache,
                        &credential_headers(&self.schemes, &eff.auth),
                    )
                {
                    self.diags.push(
                        Diagnostic::warning(
                            format!("{}", r.source),
                            format!(
                                "{} {} sets both `cache` and `auth`, so its responses won't be cached",
                                r.method_label(),
                                r.path
                            ),
                        )
                        .hint("add `vary = [\"Authorization\"]` or `key = \"auth.sub\"` to cache per user"),
                    );
                }
                // Static API keys verify as `{type, key}` claims only, so a cache
                // keyed on any other claim never caches their callers' responses.
                if let (
                    Some(CacheRule {
                        key: Some(KeySource::Claim(claim)),
                        ..
                    }),
                    AuthReq::AnyOf(alts),
                ) = (&eff.cache, &eff.auth)
                    && !matches!(claim.as_str(), "type" | "key")
                {
                    let keyless: Vec<&str> = alts
                        .iter()
                        .filter(|a| {
                            self.schemes.get(&a.scheme).is_some_and(|s| {
                                matches!(
                                    s.as_ref(),
                                    Scheme::ApiKey(ApiKeyScheme {
                                        keys: ApiKeys::Static(_),
                                        ..
                                    })
                                )
                            })
                        })
                        .map(|a| a.scheme.as_str())
                        .collect();
                    if !keyless.is_empty() {
                        self.diags.push(
                        Diagnostic::warning(
                            format!("{}", r.source),
                            format!(
                                "{} {} caches by `auth.{claim}`, which the API keys of `{}` don't have, so their callers' responses won't be cached",
                                r.method_label(),
                                r.path,
                                keyless.join("`, `"),
                            ),
                        )
                        .hint("add the header those keys are sent in to `vary` to cache per key (e.g. `vary = [\"X-API-Key\"]`)"),
                    );
                    }
                }
                // A header the route's auth doesn't read is whatever the client
                // says it is. (`Mcp-Param-*` headers are checked against the
                // call's arguments, and keying on one is a documented use.)
                if let Some(RateLimit {
                    key: KeySource::Header(h),
                    ..
                }) = &eff.rate_limit
                    && !h.starts_with("mcp-param-")
                    && !credential_headers(&self.schemes, &eff.auth).contains(h)
                {
                    self.diags.push(
                    Diagnostic::warning(
                        format!("{}", r.source),
                        format!(
                            "{} {} counts its rate limit by the `{h}` header, which the client sets: changing it starts a new count",
                            r.method_label(),
                            r.path
                        ),
                    )
                    .hint("use `key = \"ip\"` or `key = \"auth.<claim>\"`, unless a proxy you trust sets this header"),
                );
                }
            }
            // The same finding for two methods is one finding.
            let mut seen: Vec<(String, String)> = Vec::new();
            let mut at = 0;
            self.diags.items.retain(|d| {
                at += 1;
                let key = (d.location.clone(), d.message.clone());
                if at <= first || !seen.contains(&key) {
                    seen.push(key);
                    return true;
                }
                false
            });
            // A rule that compares with the caller's claims can never
            // hold where there is no caller.
            for (method, rule) in &callerless {
                self.diags.push(
                    Diagnostic::error(
                        r.source.loc(),
                        format!(
                            "`require` has `{}`, but {method} {} is public: a request there has no caller to compare with",
                            rule.describe(),
                            r.path
                        ),
                    )
                    .hint("set `auth` for it, or `require = \"off\"` to leave the rule out there"),
                );
            }
            // In a project with auth schemes, a route without `auth` is
            // as likely forgotten as meant to be public, and nothing else
            // tells the two apart. So each route says which it is.
            if !self.schemes.is_empty() && !auth_unset.is_empty() {
                let methods = match auth_unset.contains(&"*") {
                    true => r.method_label(),
                    false => auth_unset.join(","),
                };
                self.diags.push(
                    Diagnostic::error(
                        r.source.loc(),
                        format!("`auth` isn't set for {methods} {}", r.path),
                    )
                    .hint("say who may call it: `auth = \"<scheme>\"`, or `auth = \"none\"` if anyone may. `[defaults]` can say either for every route, and a `[[route]]` entry for a path or a prefix for the routes under it"),
                );
            }
            let Some(eff) = whole_route else { continue };

            // A catch-all hands every path under it to an app that does its
            // own routing. A policy entry for some of those paths matches
            // them as written, but the app may read `/Admin/x` or
            // `/admin%2Fx` as `/admin/x` and serve it without the policy.
            let serves_paths = matches!(
                r.action,
                Action::Proxy(_) | Action::Handler(_) | Action::Static { .. }
            );
            if r.catch_all.is_none() || !serves_paths {
                continue;
            }
            let narrower = r
                .narrower
                .iter()
                .map(|&i| &entries[i])
                .filter(|e| !covers(&e.segs, &r.segs));
            for entry in narrower {
                let adds_auth = eff.auth == AuthReq::Public
                    && matches!(entry.layer.auth, Some(AuthReq::AnyOf(_)));
                let adds_ip = eff.ip.is_none() && matches!(entry.layer.ip, Some(OrOff::On(_)));
                let key = if adds_auth {
                    "auth"
                } else if adds_ip {
                    "ip"
                } else {
                    continue;
                };
                let loc = entry.source.loc();
                if reported.contains(&loc) {
                    continue;
                }
                reported.push(loc.clone());
                self.diags.push(
                    Diagnostic::warning(
                        format!("{loc}.{key}"),
                        format!(
                            "`{key}` on {} only covers paths written that way, and {} {} serves every other path without it",
                            entry.path,
                            r.method_label(),
                            r.path
                        ),
                    )
                    .hint(format!(
                        "an app that reads `/Admin/x` or `/admin%2Fx` as `/admin/x` would answer them unchecked; set `{key}` on {} (or in [defaults]) and open the public paths instead{}",
                        r.path,
                        if adds_auth { " with `auth = \"none\"`" } else { "" }
                    )),
                );
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn build_routers(
        &mut self,
        routes: &[Route],
    ) -> (
        HashMap<Option<String>, matchit::Router<usize>>,
        Vec<Vec<SlotEntry>>,
        Vec<(usize, Vec<Seg>)>,
    ) {
        let mut routers: HashMap<Option<String>, matchit::Router<usize>> = HashMap::new();
        let mut slots: Vec<Vec<SlotEntry>> = Vec::new();
        let mut keys: HashMap<(Option<String>, String), usize> = HashMap::new();
        let mut insert =
            |c: &mut Self, host: &Option<String>, pattern: &str, entry: SlotEntry, loc: &str| {
                let key = (host.clone(), normalize(pattern));
                if let Some(slot) = keys.get(&key) {
                    slots[*slot].push(entry);
                    return;
                }
                let router = routers.entry(host.clone()).or_default();
                match router.insert(pattern, slots.len()) {
                    Ok(()) => {
                        keys.insert(key, slots.len());
                        slots.push(vec![entry]);
                    }
                    Err(e) if entry.fallback => {
                        let _ = e; // the base path is already taken by another route
                    }
                    Err(e) => c.diags.error(loc, format!("can't add {pattern}: {e}")),
                }
            };
        for r in routes.iter().filter(|r| r.mcp_tool().is_none()) {
            insert(
                self,
                &r.host,
                &r.path,
                SlotEntry {
                    route: r.id,
                    fallback: false,
                },
                &r.source.to_string(),
            );
        }
        let mut catch_alls = Vec::new();
        for r in routes.iter().filter(|r| r.catch_all.is_some()) {
            let base = r.path[..r.path.rfind("/{*").unwrap_or(0)].to_string();
            let bases = if base.is_empty() {
                vec!["/".to_string()]
            } else {
                vec![base.clone(), format!("{base}/")]
            };
            for b in bases {
                insert(
                    self,
                    &r.host,
                    &b,
                    SlotEntry {
                        route: r.id,
                        fallback: true,
                    },
                    &r.source.to_string(),
                );
            }
            catch_alls.push((r.id, parse_pattern(&r.path)));
        }
        catch_alls.sort_by_key(|(_, segs)| std::cmp::Reverse(segs.len()));
        (routers, slots, catch_alls)
    }
}

/// An HMAC secret shorter than this draws a warning: HS256's key should be
/// as long as its 256-bit output (RFC 7518 section 3.2).
const MIN_SECRET_BYTES: usize = 32;

fn signs_body(schemes: &BTreeMap<String, Arc<Scheme>>, auth: &AuthReq) -> bool {
    let AuthReq::AnyOf(alts) = auth else {
        return false;
    };
    alts.iter()
        .filter_map(|a| schemes.get(&a.scheme))
        .any(|s| matches!(s.as_ref(), Scheme::Webhook(_)))
}

fn credential_headers(schemes: &BTreeMap<String, Arc<Scheme>>, auth: &AuthReq) -> Vec<String> {
    let AuthReq::AnyOf(alts) = auth else {
        return Vec::new();
    };
    alts.iter()
        .filter_map(|a| schemes.get(&a.scheme))
        .filter_map(|s| match s.as_ref() {
            Scheme::Jwt(j) => Some(j.header.clone()),
            Scheme::ApiKey(k) => k.header.clone(),
            Scheme::Webhook(w) => Some(w.signed.header()),
        })
        .collect()
}

/// The host of a `Host` header or URI authority, lowercase and without the
/// port. An IPv6 literal keeps its brackets (`[::1]`).
fn host_name(authority: &str) -> String {
    match http::uri::Authority::try_from(authority) {
        Ok(a) => a.host().to_ascii_lowercase(),
        Err(_) => authority.to_ascii_lowercase(),
    }
}

/// An HTTP header name: one or more RFC 9110 token characters.
pub(crate) fn valid_header_name(name: &str) -> bool {
    http::HeaderName::from_bytes(name.as_bytes()).is_ok()
}

fn valid_ip_or_cidr(s: &str) -> bool {
    match s.split_once('/') {
        Some((ip, bits)) => {
            let Ok(ip) = ip.parse::<std::net::IpAddr>() else {
                return false;
            };
            let max = if ip.is_ipv4() { 32 } else { 128 };
            bits.parse::<u8>().is_ok_and(|b| b <= max)
        }
        None => s.parse::<std::net::IpAddr>().is_ok(),
    }
}

fn label(methods: &Option<Vec<String>>) -> String {
    match methods {
        None => "*".into(),
        Some(m) => m.join(","),
    }
}

#[cfg(test)]
mod reference;
#[cfg(test)]
pub(crate) mod tests;
