//! Compiling a project into a route table, and matching requests.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::Algorithm;
use serde::Serialize;
use serde_json::Value;

use crate::auth::{ApiKeyScheme, JwtKeys, JwtScheme, ProtectedResource, Scheme};
use crate::config::{
    self, Format, OrOff, Protocol, RawAuth, RawAuthItem, RawAuthScheme, RawConfig, RawPolicies,
    RawProxy, RawRoute, ResponseValidation, SchemaRef, TrailingSlash,
};
use crate::diag::{Diagnostic, Diagnostics};
use crate::openapi;
use crate::policy::{
    AuthAlt, AuthReq, CacheRule, Cors, Effective, HandlerRef, HeaderRules, KeySource, Layer,
    RateLimit,
};
use crate::routefiles::{self, METHODS};
use crate::rules;
use crate::validate::{BodyRule, LocationRules, ParamSpec, Validation, param_schema};

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
    pub max_body: u64,
    pub timeout: Duration,
    pub trailing_slash: TrailingSlash,
    pub tls: Option<(String, String)>,
    pub compression: bool,
    pub trusted_proxies: Vec<String>,
    pub admin_listen: Option<String>,
    pub response_validation: ResponseValidation,
    pub shutdown_timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct Runtime {
    pub name: String,
    pub extensions: Vec<String>,
    pub command: String,
    pub build: Option<String>,
    pub protocol: Protocol,
    pub workers: Option<u32>,
    pub eager: bool,
    pub env: BTreeMap<String, String>,
    pub startup_timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct Proxy {
    pub upstreams: Vec<String>,
    pub strip_prefix: Option<String>,
    /// Lowercase header name whose value pins a client to an upstream.
    pub sticky_header: Option<String>,
    pub preserve_host: bool,
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
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Respond { status, .. } => format!("respond {status}"),
            Action::Redirect { to, status } => format!("redirect {status} → {to}"),
            Action::Static { dir, .. } => format!("static {dir}"),
            Action::Proxy(p) => format!("proxy {}", p.upstreams.join(", ")),
            Action::Handler(h) => h.raw.clone(),
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
    FolderPolicies {
        path: String,
    },
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
            Source::FolderPolicies { path } => write!(f, "{path}"),
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
    /// Layers below prefix entries: defaults, OpenAPI, folder policies.
    pub low: Layer,
    /// Layers above prefix entries: the route's own policies and exact entries.
    pub high: Layer,
    /// The static layers in merge order, for `unsworn routes --policies`.
    pub layers: Vec<(String, Layer)>,
    pub param_names: Vec<String>,
    pub catch_all: Option<String>,
    pub op: Option<OpInfo>,
}

impl Route {
    pub fn handler(&self) -> Option<&HandlerRef> {
        match &self.action {
            Action::Handler(h) => Some(h),
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
enum Seg {
    Lit(String),
    Param(String),
    CatchAll(String),
}

fn parse_pattern(pattern: &str) -> Vec<Seg> {
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
    let segs: Vec<String> = parse_pattern(pattern)
        .into_iter()
        .map(|s| match s {
            Seg::Lit(l) => l,
            Seg::Param(_) => "{}".into(),
            Seg::CatchAll(_) => "{*}".into(),
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

/// A policy-only entry whose path ends in `{*rest}`.
#[derive(Debug, Clone)]
pub struct PrefixEntry {
    segs: Vec<Seg>,
    pub path: String,
    pub methods: Option<Vec<String>>,
    pub layer: Layer,
    pub source: Source,
}

impl PrefixEntry {
    fn matches_exact(&self, method: &str, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        self.methods
            .as_ref()
            .is_none_or(|m| m.iter().any(|x| x == method))
            && parts.len() == self.segs.len()
            && self.segs.iter().zip(&parts).all(|(seg, part)| match seg {
                Seg::Lit(l) => l == part,
                Seg::Param(_) => true,
                Seg::CatchAll(_) => false,
            })
    }

    pub fn matches(&self, method: &str, path: &str) -> bool {
        self.methods
            .as_ref()
            .is_none_or(|m| m.iter().any(|x| x == method))
            && match_prefix(&self.segs, path).is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreConfig {
    pub backend: String,
    pub url: Option<String>,
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
    pub prefixes: Vec<PrefixEntry>,
    /// Exact policy entries for paths inside a catch-all route.
    pub path_entries: Vec<PrefixEntry>,
    pub openapi: OpenApiMeta,
    /// Files read while loading, for watching in dev mode.
    pub files: Vec<String>,
    pub routes_dir: Option<String>,
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
        let host = host.map(|h| h.split(':').next().unwrap_or(h).to_ascii_lowercase());
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
            allowed.sort();
            allowed.dedup();
            MatchResult::MethodNotAllowed(allowed)
        }
    }

    /// The merged policies for a request to `route`.
    pub fn effective(&self, route: &Route, method: &str, path: &str) -> Effective {
        let mut layer = route.low.clone();
        for p in &self.prefixes {
            if p.matches(method, path) {
                layer.merge_from(&p.layer);
            }
        }
        layer.merge_from(&route.high);
        if route.catch_all.is_some() {
            for e in &self.path_entries {
                if e.matches_exact(method, path) {
                    layer.merge_from(&e.layer);
                }
            }
        }
        layer.finalize(self.server.timeout, self.server.max_body)
    }

    /// The prefix entries that apply to a route's own pattern, for display.
    pub fn prefixes_for(&self, route: &Route) -> Vec<&PrefixEntry> {
        let probe = route.path.replace("{*", "{");
        self.prefixes
            .iter()
            .filter(|p| match_prefix(&p.segs, &probe).is_some())
            .collect()
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
            for layer in [&r.low, &r.high] {
                if let Some(OrOff::On(list)) = &layer.before {
                    list.iter().for_each(&mut add);
                }
            }
        }
        for p in &self.prefixes {
            if let Some(OrOff::On(list)) = &p.layer.before {
                list.iter().for_each(&mut add);
            }
        }
        out
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

/// The result of compiling: a project when there are no errors, plus all
/// diagnostics (errors and warnings).
pub struct Compiled {
    pub project: Option<Project>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Loads and compiles a project from its config file.
pub fn compile(host: &dyn Host, config_file: &str) -> Compiled {
    let mut c = Compiler {
        host,
        config_file: config_file.to_string(),
        diags: Diagnostics::default(),
        runtimes: BTreeMap::new(),
        schemes: BTreeMap::new(),
        sets: BTreeMap::new(),
        files: vec![config_file.to_string()],
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
    config_file: String,
    diags: Diagnostics,
    runtimes: BTreeMap<String, Arc<Runtime>>,
    schemes: BTreeMap<String, Arc<Scheme>>,
    sets: BTreeMap<String, Layer>,
    files: Vec<String>,
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
    validation: RawValidation,
    openapi_layer: Option<Layer>,
    folder_layers: Vec<(String, Layer)>,
    op: Option<OpInfo>,
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

        if raw.mcp.is_some() {
            self.diags.push(
                Diagnostic::error("[mcp]", "MCP tools as handlers are planned for v2")
                    .hint("for now, put Unsworn in front of an MCP server with a `proxy` route; see docs/mcp.md"),
            );
        }

        let server = self.server(&raw);
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
            let layer = self.layer(set, &loc, None);
            self.sets.insert(name.clone(), layer);
        }
        let defaults = self.layer(&raw.defaults, "[defaults]", None);

        let mut drafts: Vec<Draft> = Vec::new();
        let mut exacts: Vec<ExactEntry> = Vec::new();
        let mut prefixes: Vec<PrefixEntry> = Vec::new();

        for (i, r) in raw.route.iter().enumerate() {
            let source = Source::Config {
                file: self.config_file.clone(),
                line: lines.get(i).copied(),
                index: i + 1,
            };
            self.config_route(r, source, &mut drafts, &mut exacts, &mut prefixes);
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
            if oa.mcp_tools.is_some() {
                self.diags.error(
                    "[openapi].mcp_tools",
                    "offering OpenAPI operations as MCP tools is planned for v3",
                );
            }
            if let Some(spec) = &oa.spec {
                openapi_meta.spec_file = Some(spec.clone());
                openapi_meta.spec = self.openapi(spec, oa.handler.as_deref(), &mut drafts);
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
        // Exact entries for paths served by a catch-all apply at request time.
        let mut path_entries = Vec::new();
        for e in &exacts {
            if e.used {
                continue;
            }
            let probe = e.path.clone();
            let covered = drafts_catch_alls
                .iter()
                .any(|segs| match_prefix(segs, &probe).is_some());
            if covered {
                let v = &e.validation;
                if v.params.is_some()
                    || v.query.is_some()
                    || v.headers.is_some()
                    || v.body.is_some()
                    || v.response.is_some()
                {
                    self.diags.error(
                        e.source.to_string(),
                        "validation for a path served by a catch-all isn't supported; only policies",
                    );
                }
                path_entries.push(PrefixEntry {
                    segs: parse_pattern(&e.path),
                    path: e.path.clone(),
                    methods: e.methods.clone(),
                    layer: e.layer.clone(),
                    source: e.source.clone(),
                });
                continue;
            }
            {
                self.diags.push(
                    Diagnostic::error(
                        e.source.to_string(),
                        format!("no route matches {} {}", label(&e.methods), e.path),
                    )
                    .hint("policy-only entries must match a route from config, route files or the OpenAPI spec; end the path with `{*rest}` to cover a prefix"),
                );
            }
        }
        // Shorter prefixes merge first.
        prefixes.sort_by_key(|p| p.segs.len());

        self.check_handlers(&routes, &prefixes);
        self.warnings(&routes, &prefixes);

        let (routers, slots, catch_alls) = self.build_routers(&routes);

        Some(Project {
            config_file: self.config_file.clone(),
            server,
            runtimes: self.runtimes.clone(),
            schemes: self.schemes.clone(),
            store,
            routes,
            prefixes,
            path_entries,
            openapi: openapi_meta,
            files: std::mem::take(&mut self.files),
            routes_dir,
            routers,
            slots,
            catch_alls,
        })
    }

    fn subst(&mut self, value: &str, loc: &str) -> String {
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

    fn server(&mut self, raw: &RawConfig) -> Server {
        let s = &raw.server;
        if s.listen.parse::<std::net::SocketAddr>().is_err() {
            self.diags.push(Diagnostic::error(
                "[server].listen",
                format!("\"{}\" isn't an address like \"0.0.0.0:8080\"", s.listen),
            ));
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
            listen: s.listen.clone(),
            max_body: s.max_body.map_or(1024 * 1024, |b| b.0),
            timeout: s.timeout.map_or(Duration::from_secs(30), |d| d.0),
            trailing_slash: s.trailing_slash,
            tls,
            compression: s.compression,
            trusted_proxies: s.trusted_proxies.clone(),
            admin_listen: s.admin_listen.clone(),
            response_validation: s.response_validation,
            shutdown_timeout: s.shutdown_timeout.map_or(Duration::from_secs(10), |d| d.0),
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
            let mut env = BTreeMap::new();
            for (k, v) in &r.env {
                env.insert(k.clone(), self.subst(v, &format!("{loc}.env.{k}")));
            }
            self.runtimes.insert(
                name.clone(),
                Arc::new(Runtime {
                    name: name.clone(),
                    extensions: r.extensions.clone(),
                    command: r.command.clone(),
                    build: r.build.clone(),
                    protocol: r.protocol,
                    workers: r.workers,
                    eager: r.eager,
                    env,
                    startup_timeout: r.startup_timeout.map_or(Duration::from_secs(30), |d| d.0),
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
                    let sources = [
                        j.jwks_url.is_some(),
                        j.secret.is_some(),
                        j.public_key.is_some(),
                    ]
                    .iter()
                    .filter(|b| **b)
                    .count();
                    if sources != 1 {
                        self.diags.push(Diagnostic::error(
                            &loc,
                            "set exactly one of `jwks_url`, `secret` or `public_key`",
                        ));
                        continue;
                    }
                    let keys = if let Some(url) = &j.jwks_url {
                        JwtKeys::Jwks(self.subst(url, &loc))
                    } else if let Some(secret) = &j.secret {
                        JwtKeys::Secret(self.subst(secret, &loc))
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
                    })
                }
                RawAuthScheme::ApiKey(k) => {
                    let mut keys = Vec::new();
                    for key in k.keys.to_vec() {
                        let value = self.subst(&key, &format!("{loc}.keys"));
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
            };
            self.schemes.insert(name.clone(), Arc::new(scheme));
        }
    }

    fn store(&mut self, raw: &RawConfig) -> StoreConfig {
        let store = raw.store.clone().unwrap_or(config::RawStore {
            backend: "memory".into(),
            url: None,
        });
        if store.backend != "memory" {
            self.diags.push(
                Diagnostic::error(
                    "[store].backend",
                    format!("the \"{}\" store isn't supported yet", store.backend),
                )
                .hint("v1 supports `backend = \"memory\"`"),
            );
        }
        let url = store.url.as_ref().map(|u| self.subst(u, "[store].url"));
        StoreConfig {
            backend: store.backend,
            url,
        }
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
                r.to_string()
            }
            None => {
                let ext = file.rfind('.').map(|i| &file[i..]).unwrap_or("");
                let found = self
                    .runtimes
                    .values()
                    .find(|r| r.extensions.iter().any(|e| e == ext));
                match found {
                    Some(r) => r.name.clone(),
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

    /// Converts policy keys into a layer. `use` sets apply first.
    fn layer(&mut self, raw: &RawPolicies, loc: &str, _scope: Option<&str>) -> Layer {
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
                OrOff::On(Cors {
                    origins: if origins.iter().any(|o| o == "*") {
                        None
                    } else {
                        Some(origins)
                    },
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
        own.headers = raw.headers.as_ref().map(|h| match h {
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
        layer.merge_from(&own);
        layer
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
        match r.split_headers() {
            Ok(split) => {
                if let Some(h) = &split.validation {
                    v.headers = Some(convert(self, h, "headers", false));
                }
            }
            Err(e) => self.diags.error(format!("{loc}.headers"), e),
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
        prefixes: &mut Vec<PrefixEntry>,
    ) {
        let loc = source.to_string();
        if !self.check_path(&r.path, &loc) {
            return;
        }
        if r.mcp.is_some() {
            self.diags.error(
                format!("{loc}.mcp"),
                "offering routes as MCP tools is planned for v3",
            );
        }
        let methods = self.methods(r.method.as_ref().map(|m| m.to_vec()), &loc);
        let policies = match r.policies() {
            Ok(p) => p,
            Err(e) => {
                self.diags.error(&loc, e);
                return;
            }
        };
        let layer = self.layer(&policies, &loc, None);
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
                prefixes.push(PrefixEntry {
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
                        validation,
                        openapi_layer: None,
                        folder_layers: Vec::new(),
                        op: None,
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
            let (upstreams, strip_prefix, sticky, preserve_host) = match p {
                RawProxy::Url(u) => (vec![u.clone()], None, None, false),
                RawProxy::Full(f) => (
                    f.upstreams.clone(),
                    f.strip_prefix.clone(),
                    f.sticky.clone(),
                    f.preserve_host,
                ),
            };
            let upstreams: Vec<String> = upstreams
                .iter()
                .map(|u| self.subst(u, &format!("{loc}.proxy")))
                .map(|u| u.trim_end_matches('/').to_string())
                .collect();
            if upstreams.is_empty() {
                self.diags.error(format!("{loc}.proxy"), "no upstreams");
            }
            for u in &upstreams {
                if !(u.starts_with("http://") || u.starts_with("https://")) {
                    self.diags.error(
                        format!("{loc}.proxy"),
                        format!("\"{u}\" must start with http:// or https://"),
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
                            "use the form \"header:<name>\", e.g. \"header:mcp-session-id\"",
                        );
                        None
                    }
                },
            };
            return Some(Action::Proxy(Arc::new(Proxy {
                upstreams,
                strip_prefix,
                sticky_header,
                preserve_host,
            })));
        }
        let h = r.handler.as_ref()?;
        self.handler_ref(h, r.runtime.as_deref(), &format!("{loc}.handler"))
            .map(Action::Handler)
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
                    self.diags.error(format!("{dir}/{file}"), e);
                }
                return;
            }
        };
        let mut folder_layers: Vec<(Vec<String>, String, Layer)> = Vec::new();
        for p in &scan.policies {
            let path = format!("{dir}/{}", p.file);
            self.files.push(path.clone());
            let Some(text) = self.host.read(&path) else {
                continue;
            };
            match config::parse_document::<RawPolicies>(&text, Format::Toml) {
                Ok(raw) => {
                    let layer = self.layer(&raw, &path, None);
                    folder_layers.push((p.scope.clone(), path, layer));
                }
                Err(e) => self.diags.error(&path, e),
            }
        }
        for b in &scan.befores {
            let path = format!("{dir}/{}", b.file);
            if let Some(h) = self.handler_ref(&path, None, &path) {
                folder_layers.push((
                    b.scope.clone(),
                    path,
                    Layer {
                        before: Some(OrOff::On(vec![h])),
                        ..Default::default()
                    },
                ));
            }
        }
        // Outermost folders first; policies before `_before` handlers at the same depth.
        folder_layers.sort_by_key(|(scope, _, _)| scope.len());

        for fr in scan.routes {
            let path = format!("{dir}/{}", fr.file);
            let Some(h) = self.handler_ref(&path, None, &path) else {
                continue;
            };
            let layers = folder_layers
                .iter()
                .filter(|(scope, _, _)| routefiles::in_scope(scope, &fr.chain))
                .map(|(_, p, l)| (p.clone(), l.clone()))
                .collect();
            drafts.push(Draft {
                methods: Some(vec![fr.method]),
                path: fr.path,
                host: None,
                action: Action::Handler(h),
                source: Source::RouteFile { path },
                own: Layer::default(),
                validation: RawValidation::default(),
                openapi_layer: None,
                folder_layers: layers,
                op: None,
            });
        }
    }

    fn openapi(
        &mut self,
        spec: &str,
        template: Option<&str>,
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
                validation,
                openapi_layer: layer,
                folder_layers: Vec::new(),
                op: Some(info),
            });
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
        let loc = d.source.to_string();
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
        layers.extend(d.folder_layers.iter().cloned());
        let mut low = Layer::default();
        for (_, l) in &layers {
            low.merge_from(l);
        }

        let mut high = d.own.clone();
        let mut high_layers = Vec::new();
        if !d.own.is_empty() {
            high_layers.push((loc.clone(), d.own.clone()));
        }
        let mut raw_validation = d.validation.clone();
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
            high.merge_from(&e.layer);
            high_layers.push((e.source.to_string(), e.layer.clone()));
            raw_validation.overlay(&e.validation);
        }
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
            match BodyRule::new(schema.clone(), *required) {
                Ok(r) => validation.body = Some(r),
                Err(e) => self.diags.error(format!("{loc}.body"), e),
            }
        }
        if let Some(schema) = &raw_validation.response {
            match BodyRule::new(schema.clone(), false) {
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
            param_names,
            catch_all,
            op: d.op,
        })
    }

    fn location(
        &mut self,
        prefix: &'static str,
        specs: Vec<ParamSpec>,
        loc: &str,
    ) -> Option<LocationRules> {
        match LocationRules::new(prefix, specs) {
            Ok(r) => Some(r),
            Err(e) => {
                self.diags.error(format!("{loc}.{prefix}"), e);
                None
            }
        }
    }

    fn check_handlers(&mut self, routes: &[Route], prefixes: &[PrefixEntry]) {
        let mut checked = std::collections::HashSet::new();
        let mut check = |c: &mut Self, h: &HandlerRef, loc: String| {
            if checked.insert(h.file.clone()) && !c.host.exists(&h.file) {
                c.diags.push(
                    Diagnostic::error(loc, format!("handler file \"{}\" not found", h.file))
                        .hint("handler paths are relative to the project root"),
                );
            }
        };
        for r in routes {
            if let Some(h) = r.handler() {
                check(self, h, format!("{}.handler", r.source));
            }
            for (src, l) in &r.layers {
                if let Some(OrOff::On(list)) = &l.before {
                    for h in list {
                        check(self, h, format!("{src}.before"));
                    }
                }
            }
        }
        for p in prefixes {
            if let Some(OrOff::On(list)) = &p.layer.before {
                for h in list {
                    check(self, h, format!("{}.before", p.source));
                }
            }
        }
    }

    fn warnings(&mut self, routes: &[Route], prefixes: &[PrefixEntry]) {
        for r in routes {
            let probe = r.path.replace("{*", "{");
            let mut layer = r.low.clone();
            for p in prefixes {
                if match_prefix(&p.segs, &probe).is_some() {
                    layer.merge_from(&p.layer);
                }
            }
            layer.merge_from(&r.high);
            let eff = layer.finalize(Duration::from_secs(30), 0);
            if let (Some(cache), AuthReq::AnyOf(_)) = (&eff.cache, &eff.auth)
                && !rules::cache_separates_users(cache)
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
        for r in routes {
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
pub(crate) mod tests;
