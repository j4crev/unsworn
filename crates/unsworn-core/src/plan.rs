//! The plan: everything a project enforces, worked out and written down.
//!
//! A route's rules come from several places: `[defaults]`, policy sets,
//! folder policies, prefix entries, the OpenAPI spec and the route itself.
//! The plan is what they add up to, for every route and MCP tool, next to
//! the settings that decide who gets in: auth schemes, the server's limits,
//! the runtimes that run handlers.
//!
//! It has two uses. [`Plan::render`] writes it as text, to commit next to
//! the config, so a pull request shows what it does to the rules. And
//! [`diff`] compares two plans and says which changes let more requests
//! through.
//!
//! Build one from a project compiled with [`crate::compile_symbolic`], so
//! it reads `{env.NAME}` where the config does and is the same on every
//! machine. Secrets and API keys are never written.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde_json::Value;

use crate::auth::{ApiKeys, JwtKeys, Scheme};
use crate::config::{
    ListTools, OnError, Protocol, ResponseValidation, StoreBackend, TrailingSlash,
};
use crate::policy::{AuthReq, Cors, Effective};
use crate::project::{
    Action, PolicyEntry, Project, RespondBody, Route, Seg, intersect, parse_pattern, probe,
};
use crate::units::{format_duration, format_size};
use crate::validate::{BodyRule, LocationRules};

/// The first line of a plan file's body. It changes when the format does.
pub const FORMAT: &str = "plan 1";

/// What kind of thing an entry describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Server,
    Store,
    Auth,
    Runtime,
    Route,
    Tool,
    /// A handler other handlers call by name.
    Function,
    /// A service Unsworn calls for handlers.
    Outbound,
    /// Where spans are sent, and of how many requests.
    Telemetry,
    /// Part of a route's paths with rules of its own.
    Inside,
}

/// One thing the project enforces: a route for one method, an MCP tool, an
/// auth scheme, a runtime, the server's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: Kind,
    /// The method, for a route: `GET`, or `*` for every method.
    pub method: Option<String>,
    /// What the entry is about: a route's path (with its host), a tool's or
    /// scheme's name.
    pub name: String,
    pub facts: Vec<Fact>,
    /// Paths inside a route where other rules apply, such as `/api/{*rest}`
    /// inside a catch-all. Each holds every policy that applies there.
    pub inside: Vec<Entry>,
}

impl Entry {
    /// Whether anyone may call it, on all of its paths or some: a route
    /// or a tool with `auth none`.
    fn open_to_anyone(&self) -> bool {
        let public = |e: &Entry| {
            e.facts
                .iter()
                .any(|f| f.key == "auth" && f.weigh == Weigh::Auth(AuthReq::Public))
        };
        matches!(self.kind, Kind::Route | Kind::Tool)
            && (public(self) || self.inside.iter().any(public))
    }

    fn new(kind: Kind, name: impl Into<String>) -> Entry {
        Entry {
            kind,
            method: None,
            name: name.into(),
            facts: Vec::new(),
            inside: Vec::new(),
        }
    }

    /// The entry's heading, such as `GET /users/{id}` or `auth jwt`.
    pub fn title(&self) -> String {
        self.title_with(self.method.as_deref().unwrap_or_default())
    }

    fn title_with(&self, methods: &str) -> String {
        match self.kind {
            Kind::Server => "server".into(),
            Kind::Store => "store".into(),
            Kind::Auth => format!("auth {}", self.name),
            Kind::Runtime => format!("runtime {}", self.name),
            Kind::Route => format!("{methods} {}", self.name),
            Kind::Tool => format!("tool {}", self.name),
            Kind::Function => format!("function {}", self.name),
            Kind::Outbound => format!("outbound {}", self.name),
            Kind::Telemetry => "telemetry".into(),
            Kind::Inside if methods.is_empty() => format!("at {}", self.name),
            Kind::Inside => format!("at {methods} {}", self.name),
        }
    }

    fn fact(&self, key: &str) -> Option<&Fact> {
        self.facts.iter().find(|f| f.key == key)
    }

    fn push(&mut self, key: impl Into<String>, text: impl Into<String>, weigh: Weigh) {
        self.facts.push(Fact {
            key: key.into(),
            text: text.into(),
            shown: true,
            weigh,
        });
    }

    fn plain(&mut self, key: impl Into<String>, text: impl Into<String>) {
        self.push(key, text, Weigh::Plain);
    }
}

/// One line of an entry: a setting and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub key: String,
    /// The value as the plan writes it. A long schema takes several lines.
    pub text: String,
    /// Whether the plan file has the line. A route's `timeout` and
    /// `max_body` are left out when they're the server's, which the
    /// `server` entry already says.
    pub shown: bool,
    weigh: Weigh,
}

/// How two values of a fact compare: which one lets more through.
#[derive(Debug, Clone, PartialEq)]
enum Weigh {
    /// A change is only a change.
    Plain,
    /// Having it turns requests away, so removing it loosens the rules.
    Guard,
    Auth(AuthReq),
    /// Requests allowed in a period, counted by a key.
    Rate(u32, Duration, String),
    /// A ceiling: a larger one lets more through.
    Limit(u64),
    /// Origins a request may come from. Without the rule, any may.
    Allow(Vec<String>),
    /// Cross-origin access for browsers. Without the rule, there is none.
    Cors(Cors),
    /// Handlers that run first and may reject the request.
    Before(Vec<String>),
    /// Where Unsworn sends data about requests: anywhere new is looser.
    Destination,
    /// The share of requests whose data is sent: a larger one sends more.
    Share(f64),
}

/// Whether a change lets more through (requests in, or data about them
/// out), less, or can't be said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Looser,
    Tighter,
    Changed,
}

fn direction(old: Option<&Fact>, new: Option<&Fact>) -> Direction {
    use Direction::{Changed, Looser, Tighter};
    let by = |looser: bool, tighter: bool| match (looser, tighter) {
        (true, false) => Looser,
        (false, true) => Tighter,
        _ => Changed,
    };
    match (old.map(|f| &f.weigh), new.map(|f| &f.weigh)) {
        (Some(Weigh::Auth(a)), Some(Weigh::Auth(b))) => by(accepts_all(b, a), accepts_all(a, b)),
        (Some(Weigh::Rate(n1, per1, key1)), Some(Weigh::Rate(n2, per2, key2))) if key1 == key2 => {
            // n2 / per2 against n1 / per1, without dividing.
            let (old, new) = (
                u128::from(*n1) * per2.as_nanos(),
                u128::from(*n2) * per1.as_nanos(),
            );
            by(new > old, new < old)
        }
        (Some(Weigh::Limit(a)), Some(Weigh::Limit(b))) => by(b > a, b < a),
        (Some(Weigh::Share(a)), Some(Weigh::Share(b))) => by(b > a, b < a),
        (Some(Weigh::Destination), Some(Weigh::Destination)) => Looser,
        (Some(Weigh::Allow(a)), Some(Weigh::Allow(b))) => by(superset(b, a), superset(a, b)),
        (Some(Weigh::Cors(a)), Some(Weigh::Cors(b))) => by(cors_wider(b, a), cors_wider(a, b)),
        (None, Some(Weigh::Cors(_))) => Looser,
        (Some(Weigh::Cors(_)), None) => Tighter,
        (old, new) if is_before(old) || is_before(new) => {
            let list = |w: Option<&Weigh>| match w {
                Some(Weigh::Before(list)) => list.clone(),
                _ => Vec::new(),
            };
            let (a, b) = (list(old), list(new));
            by(superset(&a, &b), superset(&b, &a))
        }
        (Some(w), None) if w.guards() => Looser,
        (None, Some(w)) if w.guards() => Tighter,
        _ => Changed,
    }
}

fn is_before(w: Option<&Weigh>) -> bool {
    matches!(w, Some(Weigh::Before(_)))
}

impl Weigh {
    /// Whether having the fact at all keeps some requests out.
    fn guards(&self) -> bool {
        matches!(
            self,
            Weigh::Guard | Weigh::Rate(..) | Weigh::Allow(_) | Weigh::Auth(_)
        )
    }
}

/// `a` has everything in `b`, and they differ.
fn superset(a: &[String], b: &[String]) -> bool {
    a != b && b.iter().all(|x| a.contains(x))
}

/// Whether every credential `old` accepts gets through `new` as well, and
/// the two differ.
fn accepts_all(new: &AuthReq, old: &AuthReq) -> bool {
    if new == old {
        return false;
    }
    match (new, old) {
        (AuthReq::Public, _) => true,
        (AuthReq::AnyOf(_), AuthReq::Public) => false,
        (AuthReq::AnyOf(new), AuthReq::AnyOf(old)) => old.iter().all(|o| {
            new.iter()
                .any(|n| n.scheme == o.scheme && n.scopes.iter().all(|s| o.scopes.contains(s)))
        }),
    }
}

/// Whether `new` lets every site in that `old` did, and they differ in who
/// gets in.
fn cors_wider(new: &Cors, old: &Cors) -> bool {
    let origins = match (&new.origins, &old.origins) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(n), Some(o)) => o.iter().all(|x| n.contains(x)),
    };
    let same = new.origins == old.origins && new.credentials == old.credentials;
    origins && (new.credentials || !old.credentials) && !same
}

/// Everything a project enforces. See the [module](self).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The config file the plan was made from.
    pub config_file: String,
    /// Settings first, then routes by path and method, then MCP tools.
    pub entries: Vec<Entry>,
}

impl Plan {
    /// Works out the plan of a project. Compile it with
    /// [`crate::compile_symbolic`] first.
    pub fn of(project: &Project) -> Plan {
        let mut entries = vec![server(project), store(project)];
        entries.extend(project.schemes.values().map(|s| scheme(s)));
        entries.extend(project.runtimes.values().map(|r| runtime(r)));
        let mut routes = Vec::new();
        let mut tools = Vec::new();
        for route in &project.routes {
            if route.mcp_tool().is_some() {
                tools.extend(tool(project, route));
                continue;
            }
            match &route.methods {
                None => routes.push(route_entry(project, route, "*")),
                Some(methods) => {
                    routes.extend(methods.iter().map(|m| route_entry(project, route, m)));
                }
            }
        }
        routes.sort_by(|a, b| {
            let rank = |e: &Entry| method_rank(e.method.as_deref().unwrap_or_default());
            (&a.name, rank(a), &a.method).cmp(&(&b.name, rank(b), &b.method))
        });
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        entries.extend(routes);
        entries.extend(tools);
        entries.extend(project.functions.values().map(|f| function(project, f)));
        entries.extend(project.outbound.values().map(|o| outbound(project, o)));
        entries.extend(project.telemetry.as_ref().map(telemetry));
        Plan {
            config_file: project.config_file.clone(),
            entries,
        }
    }

    /// The plan as text, for a file kept next to the config.
    ///
    /// Methods of one route that share every rule are written as one block.
    pub fn render(&self) -> String {
        let mut out = format!(
            "# What this project enforces, worked out from {} and everything it\n\
             # brings in: each route's merged policies and checks, the auth schemes,\n\
             # the server's limits. Written by `unsworn plan`; don't edit it. Change\n\
             # the config, run `unsworn plan` again and commit both, so a review shows\n\
             # what a change does to the rules. `unsworn plan --check` fails when\n\
             # this file is out of date.\n\n{FORMAT}\n",
            self.config_file
        );
        for (methods, entry) in grouped(&self.entries) {
            out.push('\n');
            render_entry(&mut out, entry, &entry.title_with(&methods), 0, None);
        }
        out
    }
}

/// Entries with the methods to title them by: neighbours that are one
/// route's methods and say the same thing become one.
fn grouped(entries: &[Entry]) -> Vec<(String, &Entry)> {
    let mut out: Vec<(String, &Entry)> = Vec::new();
    for e in entries {
        let method = e.method.clone().unwrap_or_default();
        if let Some((methods, last)) = out.last_mut()
            && e.kind == Kind::Route
            && last.kind == Kind::Route
            && last.name == e.name
            && last.facts == e.facts
            && last.inside == e.inside
        {
            methods.push_str(", ");
            methods.push_str(&method);
            continue;
        }
        out.push((method, e));
    }
    out
}

const KEY_WIDTH: usize = 18;

/// Writes an entry under `title`. `base` is the route an entry is part
/// of: such an entry is written as what differs from the route's own rules.
fn render_entry(out: &mut String, entry: &Entry, title: &str, depth: usize, base: Option<&Entry>) {
    let pad = "  ".repeat(depth);
    let _ = writeln!(out, "{pad}{title}");
    for fact in &entry.facts {
        let print = match base {
            None => fact.shown,
            Some(b) => b.fact(&fact.key).is_none_or(|f| f.text != fact.text),
        };
        if print {
            render_fact(out, &pad, &fact.key, &fact.text);
        }
    }
    if let Some(base) = base {
        for fact in &base.facts {
            if is_policy(&fact.key) && entry.fact(&fact.key).is_none() {
                render_fact(out, &pad, &fact.key, "off");
            }
        }
    }
    for inner in &entry.inside {
        render_entry(out, inner, &inner.title(), depth + 1, Some(entry));
    }
}

fn render_fact(out: &mut String, pad: &str, key: &str, text: &str) {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let _ = writeln!(out, "{pad}  {key:<KEY_WIDTH$} {first}");
    for line in lines {
        let _ = writeln!(out, "{pad}  {:<KEY_WIDTH$} {line}", "");
    }
}

/// The policies an [`Effective`] holds, as fact keys.
const POLICY_KEYS: &[&str] = &[
    "auth",
    "origin",
    "ip",
    "cors",
    "rate_limit",
    "cache",
    "before",
    "require",
    "timeout",
    "max_body",
    "stream_body",
    "response_headers",
];

fn is_policy(key: &str) -> bool {
    POLICY_KEYS.contains(&key)
}

fn method_rank(method: &str) -> usize {
    const ORDER: &[&str] = &["*", "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];
    ORDER
        .iter()
        .position(|m| *m == method)
        .unwrap_or(ORDER.len())
}

fn name_of<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(s)) => s,
        _ => String::new(),
    }
}

fn server(project: &Project) -> Entry {
    let s = &project.server;
    let mut e = Entry::new(Kind::Server, "");
    if s.listen_set {
        e.plain("listen", &s.listen);
    }
    e.plain("tls", if s.tls.is_some() { "on" } else { "off" });
    e.push(
        "max_body",
        format_size(s.max_body),
        Weigh::Limit(s.max_body),
    );
    e.plain("timeout", format_duration(s.timeout));
    if s.trailing_slash != TrailingSlash::default() {
        e.plain("trailing_slash", name_of(&s.trailing_slash));
    }
    if !s.trusted_proxies.is_empty() {
        e.plain("trusted_proxies", s.trusted_proxies.join(", "));
    }
    if let Some(admin) = &s.admin_listen {
        e.plain("admin_listen", admin);
    }
    if s.response_validation != ResponseValidation::default() {
        e.plain("response_validation", name_of(&s.response_validation));
    }
    e.plain("max_connections", s.max_connections.to_string());
    e.plain("idle_timeout", format_duration(s.idle_timeout));
    e
}

fn store(project: &Project) -> Entry {
    let s = &project.store;
    let mut e = Entry::new(Kind::Store, "");
    e.plain("backend", name_of(&s.backend));
    if s.backend != StoreBackend::Memory {
        // What a rate limit does when the store can't be reached.
        e.plain(
            "on_error",
            match s.on_error {
                OnError::Open => "let requests through",
                OnError::Closed => "answer 503",
            },
        );
    }
    if !s.prefix.is_empty() {
        e.plain("prefix", &s.prefix);
    }
    e
}

fn scheme(scheme: &Scheme) -> Entry {
    let mut e = Entry::new(Kind::Auth, scheme.name());
    match scheme {
        Scheme::Jwt(j) => {
            e.plain("type", "jwt");
            e.plain("header", &j.header);
            e.plain(
                "keys",
                match &j.keys {
                    JwtKeys::Jwks(url) => format!("from {url}"),
                    JwtKeys::Secret(_) => "a shared secret".into(),
                    JwtKeys::PublicKey(_) => "a public key file".into(),
                },
            );
            let algorithms: Vec<String> = j.algorithms.iter().map(|a| format!("{a:?}")).collect();
            e.plain("algorithms", algorithms.join(", "));
            if let Some(issuer) = &j.issuer {
                e.push("issuer", issuer, Weigh::Guard);
            }
            if !j.audience.is_empty() {
                e.push("audience", j.audience.join(", "), Weigh::Guard);
            }
            e.plain("leeway", format!("{}s", j.leeway));
            if let Some(pr) = &j.protected_resource {
                e.plain("protected_resource", &pr.resource);
                if !pr.authorization_servers.is_empty() {
                    e.plain("authorization_servers", pr.authorization_servers.join(", "));
                }
            }
        }
        Scheme::ApiKey(k) => {
            e.plain("type", "api_key");
            if let Some(h) = &k.header {
                e.plain("header", h);
            }
            if let Some(q) = &k.query {
                e.plain("query", q);
            }
            match &k.keys {
                ApiKeys::Static(_) => e.plain("keys", "listed in the config"),
                ApiKeys::Remote(v) => {
                    e.plain("keys", format!("checked by {}", v.url));
                    e.plain("verifier_timeout", format_duration(v.timeout));
                    if let Some(cache) = v.cache {
                        e.plain("verifier_cache", format_duration(cache));
                    }
                }
            }
        }
        Scheme::Webhook(w) => {
            e.plain("type", "webhook");
            if let Some(provider) = w.signed.name() {
                e.plain("provider", provider);
            }
            e.plain("header", w.signed.header());
            e.plain("signature", "HMAC-SHA256 with a shared secret");
            if w.signed.timed() {
                // A longer one accepts older requests.
                e.push(
                    "tolerance",
                    format_duration(Duration::from_secs(w.tolerance)),
                    Weigh::Limit(w.tolerance),
                );
            }
        }
    }
    e
}

fn runtime(r: &crate::project::Runtime) -> Entry {
    let mut e = Entry::new(Kind::Runtime, &r.name);
    if !r.extensions.is_empty() {
        e.plain("extensions", r.extensions.join(", "));
    }
    e.plain("command", &r.command);
    if let Some(build) = &r.build {
        e.plain("build", build);
    }
    e.plain(
        "protocol",
        match r.protocol {
            Protocol::Jsonl => "jsonl",
            Protocol::Framed | Protocol::Binary => "framed",
            Protocol::Http => match r.listen {
                crate::config::Listen::Socket => "http, on a socket",
                crate::config::Listen::Port => "http, on a port",
            },
        },
    );
    match (r.workers.min, r.workers.max) {
        (1, None) => {}
        (min, Some(max)) if min == max => e.plain("workers", min.to_string()),
        (min, Some(max)) => e.plain("workers", format!("{min} to {max}")),
        (min, None) => e.plain("workers", format!("{min} or more")),
    }
    if r.eager {
        e.plain("eager", "yes");
    }
    match &r.calls {
        crate::project::Calls::None => {}
        crate::project::Calls::All => e.plain("calls", "every function and outbound service"),
        crate::project::Calls::Only(names) => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            e.plain("calls", names.join(", "));
        }
    }
    if !r.env.is_empty() {
        // Names only: a value may be a secret someone wrote into the config.
        let names: Vec<&str> = r.env.keys().map(String::as_str).collect();
        e.plain("env", names.join(", "));
    }
    e
}

fn auth_label(auth: &AuthReq) -> String {
    match auth {
        AuthReq::Public => "none".into(),
        AuthReq::AnyOf(alts) => alts
            .iter()
            .map(|a| {
                if a.scopes.is_empty() {
                    a.scheme.clone()
                } else {
                    format!("{} [{}]", a.scheme, a.scopes.join(" "))
                }
            })
            .collect::<Vec<_>>()
            .join(" or "),
    }
}

/// An entry's policy facts, in [`POLICY_KEYS`] order.
fn policies(e: &mut Entry, eff: &Effective, project: &Project) {
    e.push("auth", auth_label(&eff.auth), Weigh::Auth(eff.auth.clone()));
    if let Some(origins) = &eff.origin {
        e.push("origin", origins.join(", "), Weigh::Allow(origins.clone()));
    }
    if let Some(ip) = &eff.ip {
        let mut parts = Vec::new();
        if !ip.allow.is_empty() {
            parts.push(format!("allow {}", ip.allow.join(", ")));
        }
        if !ip.deny.is_empty() {
            parts.push(format!("deny {}", ip.deny.join(", ")));
        }
        e.push("ip", parts.join("; "), Weigh::Guard);
    }
    if let Some(cors) = &eff.cors {
        let mut parts = vec![match &cors.origins {
            None => "any origin".to_string(),
            Some(list) => list.join(", "),
        }];
        if cors.credentials {
            parts.push("with credentials".into());
        }
        if let Some(m) = &cors.methods {
            parts.push(format!("methods {}", m.join(" ")));
        }
        if let Some(h) = &cors.headers {
            parts.push(format!("headers {}", h.join(" ")));
        }
        if !cors.expose.is_empty() {
            parts.push(format!("expose {}", cors.expose.join(" ")));
        }
        if let Some(age) = cors.max_age {
            parts.push(format!("max_age {}", format_duration(age)));
        }
        e.push("cors", parts.join("; "), Weigh::Cors(cors.clone()));
    }
    if let Some(rl) = &eff.rate_limit {
        let key = rl.key.describe();
        e.push(
            "rate_limit",
            format!(
                "{} per {} by {key}, counted for {}",
                rl.requests,
                format_duration(rl.per),
                rl.counter
            ),
            // A limit that moves to another counter is another limit:
            // how the two compare depends on what else counts there.
            Weigh::Rate(rl.requests, rl.per, format!("{key} on {}", rl.counter)),
        );
    }
    if let Some(cache) = &eff.cache {
        let mut parts = vec![format_duration(cache.ttl)];
        if let Some(key) = &cache.key {
            parts.push(format!("by {}", key.describe()));
        }
        if !cache.vary.is_empty() {
            parts.push(format!("vary {}", cache.vary.join(" ")));
        }
        e.plain("cache", parts.join("; "));
    }
    if !eff.before.is_empty() {
        let list: Vec<String> = eff.before.iter().map(|h| h.raw.clone()).collect();
        e.push("before", list.join(", "), Weigh::Before(list));
    }
    if !eff.require.is_empty() {
        let rules: Vec<String> = eff.require.iter().map(|r| r.describe()).collect();
        // Each rule turns requests away, as a `before` handler may.
        e.push("require", rules.join("; "), Weigh::Before(rules));
    }
    e.plain("timeout", format_duration(eff.timeout));
    if let Some(f) = e.facts.last_mut() {
        f.shown = eff.timeout != project.server.timeout;
    }
    e.push(
        "max_body",
        format_size(eff.max_body),
        Weigh::Limit(eff.max_body),
    );
    if let Some(f) = e.facts.last_mut() {
        f.shown = eff.max_body != project.server.max_body;
    }
    if eff.stream_body {
        e.plain("stream_body", "yes");
    }
    let mut headers = Vec::new();
    if !eff.response_headers.set.is_empty() {
        let set: Vec<String> = eff
            .response_headers
            .set
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        headers.push(format!("set {}", set.join(", ")));
    }
    if !eff.response_headers.remove.is_empty() {
        headers.push(format!("remove {}", eff.response_headers.remove.join(", ")));
    }
    if !headers.is_empty() {
        e.plain("response_headers", headers.join("; "));
    }
}

/// A schema on one line when it's short, and spread out when it isn't, so
/// a change to a long one shows as the lines that changed.
fn schema_text(schema: &Value) -> String {
    let compact = schema.to_string();
    if compact.len() <= 72 {
        compact
    } else {
        serde_json::to_string_pretty(schema).unwrap_or(compact)
    }
}

fn location(e: &mut Entry, rules: &Option<LocationRules>, prefix: &str, always: bool) {
    for spec in rules.iter().flat_map(|r| &r.specs) {
        let need = if spec.required && !always {
            "required "
        } else {
            ""
        };
        e.push(
            format!("{prefix}.{}", spec.name),
            format!("{need}{}", schema_text(&spec.schema)),
            Weigh::Guard,
        );
    }
}

fn body(e: &mut Entry, key: &str, rule: &Option<BodyRule>) {
    if let Some(rule) = rule {
        let need = if rule.required { "" } else { "optional " };
        e.push(
            key,
            format!("{need}{}", schema_text(&rule.schema)),
            Weigh::Guard,
        );
    }
}

fn action(e: &mut Entry, project: &Project, route: &Route) {
    match &route.action {
        Action::Handler(h) => e.plain("handler", format!("{} ({})", h.raw, h.runtime)),
        Action::Proxy(p) => proxy(e, p),
        Action::Respond {
            status,
            body,
            headers,
        } => {
            let body = match body {
                RespondBody::None => String::new(),
                RespondBody::Json(v) => format!(" json {}", schema_text(v)),
                RespondBody::Text(t) => format!(" text {}", Value::from(t.as_str())),
            };
            e.plain("respond", format!("{status}{body}"));
            if !headers.is_empty() {
                let list: Vec<String> = headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                e.plain("respond_headers", list.join(", "));
            }
        }
        Action::Redirect { to, status } => e.plain("redirect", format!("{status} to {to}")),
        Action::Static {
            dir,
            spa_fallback,
            index,
            cache_control,
        } => {
            let mut parts = vec![dir.clone(), format!("index {index}")];
            if let Some(f) = spa_fallback {
                parts.push(format!("spa_fallback {f}"));
            }
            if let Some(c) = cache_control {
                parts.push(format!("cache_control {c}"));
            }
            e.plain("static", parts.join("; "));
        }
        Action::Mcp => {
            let Some(mcp) = &project.mcp else { return };
            match &mcp.proxy {
                Some(p) => {
                    e.plain("mcp", "forwarded to an MCP server");
                    proxy(e, p);
                }
                None => e.plain("mcp", "tools run as handlers"),
            }
            e.plain(
                "list_tools",
                match mcp.list_tools {
                    ListTools::Permitted => "those the caller's token allows",
                    ListTools::All => "all",
                },
            );
        }
    }
}

fn proxy(e: &mut Entry, p: &crate::project::Proxy) {
    let mut parts = vec![match &p.runtime {
        Some(name) => format!("runtime {name}"),
        None => p.upstreams.join(", "),
    }];
    if let Some(s) = &p.strip_prefix {
        parts.push(format!("strip_prefix {s}"));
    }
    if let Some(h) = &p.sticky_header {
        parts.push(format!("sticky by {h}"));
    }
    if p.preserve_host {
        parts.push("preserve_host".into());
    }
    if !p.context {
        parts.push("no context sent".into());
    }
    e.plain("proxy", parts.join("; "));
}

fn route_entry(project: &Project, route: &Route, method: &str) -> Entry {
    let name = match &route.host {
        Some(host) => format!("{host}{}", route.path),
        None => route.path.clone(),
    };
    let mut e = Entry::new(Kind::Route, name);
    e.method = Some(method.to_string());
    action(&mut e, project, route);
    if let Some(t) = project.route_tool(route.id) {
        e.plain("mcp_tool", tool_summary(t));
    }
    if let Some(s) = &route.shadow {
        let share = match s.every {
            1 => String::new(),
            n => format!("; 1 request in {n}"),
        };
        e.plain(
            "shadow",
            format!("{} ({}){share}", s.handler.raw, s.handler.runtime),
        );
    }
    if !route.invalidates.is_empty() {
        let paths: Vec<&str> = route.invalidates.iter().map(|i| i.path.as_str()).collect();
        e.plain("invalidates", paths.join(", "));
    }
    let own = parse_pattern(&route.path);
    policies(
        &mut e,
        &project.effective(route, method, &probe(&own)),
        project,
    );
    location(&mut e, &route.validation.params, "params", true);
    location(&mut e, &route.validation.query, "query", false);
    location(&mut e, &route.validation.headers, "headers", false);
    body(&mut e, "body", &route.validation.body);
    body(&mut e, "response", &route.validation.response);
    e.inside = inside(project, route, method, &own, &e);
    e
}

fn tool_summary(tool: &crate::mcp::McpTool) -> String {
    match &tool.description {
        Some(d) => format!("{}: {d}", tool.name),
        None => tool.name.clone(),
    }
}

/// The parts of a route's paths where an entry for a longer path, or for
/// some methods only, changes the rules.
fn inside(project: &Project, route: &Route, method: &str, own: &[Seg], base: &Entry) -> Vec<Entry> {
    let mut found: BTreeMap<(String, String), Vec<Seg>> = BTreeMap::new();
    for entry in &project.entries {
        let Some(shared) = intersect(own, &entry.segs) else {
            continue;
        };
        let whole = same_paths(&shared, own);
        for m in entry_methods(entry, method) {
            if whole && m == method {
                continue; // already in the route's own rules
            }
            // A narrower route may serve these requests, with rules of
            // its own.
            if !project.serves(route, &m, &probe(&shared)) {
                continue;
            }
            let shown = if m == method { String::new() } else { m };
            found.insert((pattern(&shared), shown), shared.clone());
        }
    }
    let mut out = Vec::new();
    for ((path, shown), segs) in found {
        let m = if shown.is_empty() { method } else { &shown };
        let mut e = Entry::new(Kind::Inside, path);
        e.method = (!shown.is_empty()).then(|| shown.clone());
        policies(&mut e, &project.effective(route, m, &probe(&segs)), project);
        let policy_facts = |e: &Entry| -> Vec<(String, String)> {
            e.facts
                .iter()
                .filter(|f| is_policy(&f.key))
                .map(|f| (f.key.clone(), f.text.clone()))
                .collect()
        };
        if policy_facts(&e) != policy_facts(base) {
            out.push(e);
        }
    }
    out
}

/// The methods a policy entry applies to, among those a rule for `method`
/// (`*` for all) is about.
fn entry_methods(entry: &PolicyEntry, method: &str) -> Vec<String> {
    match &entry.methods {
        None => vec![method.to_string()],
        Some(list) if method == "*" => list.clone(),
        Some(list) => {
            let covers = list
                .iter()
                .any(|m| m == method || (method == "HEAD" && m == "GET"));
            if covers {
                vec![method.to_string()]
            } else {
                Vec::new()
            }
        }
    }
}

/// Whether two patterns match the same paths.
fn same_paths(a: &[Seg], b: &[Seg]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|pair| match pair {
            (Seg::Lit(x), Seg::Lit(y)) => x == y,
            (Seg::Param(_), Seg::Param(_)) | (Seg::CatchAll(_), Seg::CatchAll(_)) => true,
            _ => false,
        })
}

fn pattern(segs: &[Seg]) -> String {
    let parts: Vec<String> = segs
        .iter()
        .map(|s| match s {
            Seg::Lit(l) => l.clone(),
            Seg::Param(n) => format!("{{{n}}}"),
            Seg::CatchAll(n) => format!("{{*{n}}}"),
        })
        .collect();
    format!("/{}", parts.join("/"))
}

/// The entry for a `[[mcp.tool]]`, from its hidden route.
fn tool(project: &Project, route: &Route) -> Option<Entry> {
    let mcp = project.mcp.as_ref()?;
    let tool = mcp.tool(route.mcp_tool()?)?;
    let mut e = Entry::new(Kind::Tool, &tool.name);
    if let Some(d) = &tool.description {
        e.plain("description", d);
    }
    match &route.action {
        Action::Handler(_) => action(&mut e, project, route),
        _ => e.plain("forwarded", "to the MCP server"),
    }
    let eff = project.route_effective(route);
    policies(&mut e, &eff, project);
    // A call passes the endpoint's auth before the tool's own.
    if tool.own.auth.is_some() {
        let endpoint = project.route_effective(&project.routes[mcp.route]);
        if let Some(f) = e.facts.iter_mut().find(|f| f.key == "auth") {
            f.text = format!(
                "{}, then {}",
                auth_label(&endpoint.auth),
                auth_label(&eff.auth)
            );
        }
    }
    if tool.input_rule.is_some() {
        e.push("input", schema_text(&tool.input_schema), Weigh::Guard);
    } else {
        e.plain(
            "input",
            format!("unchecked {}", schema_text(&tool.input_schema)),
        );
    }
    if let Some(output) = &tool.output_schema {
        e.plain("output", schema_text(output));
    }
    if let Some(a) = &tool.annotations {
        e.plain("annotations", schema_text(a));
    }
    Some(e)
}

fn function(project: &Project, f: &crate::project::Function) -> Entry {
    let mut e = Entry::new(Kind::Function, &f.name);
    e.plain(
        "handler",
        format!("{} ({})", f.handler.raw, f.handler.runtime),
    );
    e.plain("timeout", format_duration(f.timeout));
    if let Some(fact) = e.facts.last_mut() {
        fact.shown = f.timeout != project.server.timeout;
    }
    body(&mut e, "input", &f.input);
    if let Some(output) = &f.output {
        e.plain("output", schema_text(&output.schema));
    }
    e
}

fn outbound(project: &Project, o: &crate::project::Outbound) -> Entry {
    let mut e = Entry::new(Kind::Outbound, &o.name);
    e.plain("url", &o.url);
    if !o.headers.is_empty() {
        // Names only: the values are the credentials.
        let names: Vec<&str> = o.headers.iter().map(|(name, _)| name.as_str()).collect();
        e.plain("adds", names.join(", "));
    }
    // Leaving either out lets a handler use any.
    if let Some(methods) = &o.methods {
        e.push("methods", methods.join(", "), Weigh::Allow(methods.clone()));
    }
    if let Some(paths) = &o.paths {
        e.push("paths", paths.join(", "), Weigh::Allow(paths.clone()));
    }
    e.plain("timeout", format_duration(o.timeout));
    if let Some(fact) = e.facts.last_mut() {
        fact.shown = o.timeout != project.server.timeout;
    }
    e.push(
        "max_response",
        format_size(o.max_response),
        Weigh::Limit(o.max_response),
    );
    e
}

fn telemetry(t: &crate::project::Telemetry) -> Entry {
    let mut e = Entry::new(Kind::Telemetry, "");
    e.push("outbound", &t.outbound.name, Weigh::Destination);
    if let Some(name) = &t.service_name {
        e.plain("service_name", name);
    }
    e.push("sample", t.sample.to_string(), Weigh::Share(t.sample));
    let sends: Vec<String> = [(true, "traces"), (t.metrics, "metrics"), (t.logs, "logs")]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, what)| what.to_string())
        .collect();
    // Each kind of data added is more that leaves the server.
    e.push("sends", sends.join(", "), Weigh::Allow(sends.clone()));
    e
}

/// One changed line of an entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// The part of the route it applies to, such as `at /api/{*rest}`.
    /// `None` for the entry's own rules.
    pub at: Option<String>,
    pub key: String,
    pub old: Option<String>,
    pub new: Option<String>,
    pub direction: Direction,
}

/// What happened to one entry between two plans.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    /// The entry is new. It's kept whole, to show what was added.
    Added(Entry),
    Removed,
    Changed(Vec<Change>),
}

/// The difference between two plans.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Diff {
    /// Each changed entry's title and what happened to it, in plan order.
    /// One route's methods that changed alike share an item.
    pub items: Vec<(String, Delta)>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn changes(&self) -> impl Iterator<Item = &Change> {
        self.items.iter().flat_map(|(_, d)| match d {
            Delta::Changed(list) => list.as_slice(),
            _ => &[],
        })
    }

    /// How many changes let more through: rules that got looser, new routes
    /// and tools that anyone may call, and `[telemetry]` being added.
    pub fn looser(&self) -> usize {
        let exporting = self
            .items
            .iter()
            .filter(|(_, d)| matches!(d, Delta::Added(e) if e.kind == Kind::Telemetry))
            .count();
        self.loosened() + self.opened() + exporting
    }

    /// How many rules got looser.
    fn loosened(&self) -> usize {
        self.changes()
            .filter(|c| c.direction == Direction::Looser)
            .count()
    }

    /// How many added routes and tools anyone may call, on all of their
    /// paths or some. A route that was never there let nothing through, so
    /// a public one is looser than what came before it.
    pub fn opened(&self) -> usize {
        self.items
            .iter()
            .filter(|(_, d)| matches!(d, Delta::Added(e) if e.open_to_anyone()))
            .count()
    }

    /// How many changes let fewer requests through.
    pub fn tighter(&self) -> usize {
        self.changes()
            .filter(|c| c.direction == Direction::Tighter)
            .count()
    }

    /// The difference as text. A `!` marks each change that lets more
    /// through.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "The rules are the same.\n".into();
        }
        let mut out = String::new();
        let (mut added, mut removed, mut changed) = (0, 0, 0);
        for (title, delta) in &self.items {
            match delta {
                Delta::Added(entry) => {
                    added += 1;
                    let mut block = String::new();
                    render_entry(&mut block, entry, title, 0, None);
                    let guarded = matches!(entry.kind, Kind::Route | Kind::Tool);
                    for (i, line) in block.lines().enumerate() {
                        let mark = if i == 0 { "+ " } else { "  " };
                        let rule = line.trim_start();
                        let open = guarded
                            && rule.split_whitespace().collect::<Vec<_>>() == ["auth", "none"];
                        let note = if open {
                            Some("anyone may call it")
                        } else if entry.kind == Kind::Telemetry && rule.starts_with("outbound ") {
                            Some("spans are sent there")
                        } else {
                            None
                        };
                        if let Some(note) = note {
                            // The `!` goes where a looser change has it,
                            // in the margin.
                            let indent = &line[..line.len() - rule.len()];
                            let indent = &indent[..indent.len().saturating_sub(2)];
                            let _ = writeln!(out, "{mark}! {indent}{rule}  (looser: {note})");
                        } else {
                            let _ = writeln!(out, "{mark}{line}");
                        }
                    }
                }
                Delta::Removed => {
                    removed += 1;
                    let _ = writeln!(out, "- {title}");
                }
                Delta::Changed(changes) => {
                    changed += changes.len();
                    let _ = writeln!(out, "~ {title}");
                    let mut at: Option<&String> = None;
                    for c in changes {
                        if c.at.as_ref() != at {
                            at = c.at.as_ref();
                            if let Some(at) = at {
                                let _ = writeln!(out, "    {at}");
                            }
                        }
                        render_change(&mut out, c);
                    }
                }
            }
        }
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        let mut parts = Vec::new();
        if changed > 0 {
            let mut s = plural(changed, "change");
            let mut marks = Vec::new();
            if self.loosened() > 0 {
                marks.push(format!("{} looser", self.loosened()));
            }
            if self.tighter() > 0 {
                marks.push(format!("{} tighter", self.tighter()));
            }
            if !marks.is_empty() {
                let _ = write!(s, " ({})", marks.join(", "));
            }
            parts.push(s);
        }
        if added > 0 {
            parts.push(match self.opened() {
                0 => format!("{added} added"),
                n => format!("{added} added ({n} open to anyone)"),
            });
        }
        if removed > 0 {
            parts.push(format!("{removed} removed"));
        }
        let _ = writeln!(out, "\n{}", parts.join(", "));
        out
    }
}

fn render_change(out: &mut String, c: &Change) {
    let mark = if c.direction == Direction::Looser {
        '!'
    } else {
        ' '
    };
    let pad = if c.at.is_some() { "  " } else { "" };
    let note = match c.direction {
        Direction::Looser => "  (looser)",
        Direction::Tighter => "  (tighter)",
        Direction::Changed => "",
    };
    let long = |t: &Option<String>| t.as_ref().is_some_and(|t| t.contains('\n'));
    if long(&c.old) || long(&c.new) {
        let _ = writeln!(out, "  {mark} {pad}{}{note}", c.key);
        for (sign, text) in [('-', &c.old), ('+', &c.new)] {
            for line in text.iter().flat_map(|t| t.lines()) {
                let _ = writeln!(out, "      {pad}{sign} {line}");
            }
        }
        return;
    }
    let side = |t: &Option<String>| t.clone().unwrap_or_else(|| "(not set)".into());
    let _ = writeln!(
        out,
        "  {mark} {pad}{:<KEY_WIDTH$} {} → {}{note}",
        c.key,
        side(&c.old),
        side(&c.new)
    );
}

/// The changes from `old` to `new`.
pub fn diff(old: &Plan, new: &Plan) -> Diff {
    let id = |e: &Entry| (e.kind, e.name.clone(), e.method.clone());
    let before: BTreeMap<_, &Entry> = old.entries.iter().map(|e| (id(e), e)).collect();
    let after: BTreeMap<_, &Entry> = new.entries.iter().map(|e| (id(e), e)).collect();

    // In the new plan's order, with what's gone where the old plan had it.
    let mut found: Vec<(&Entry, Delta)> = Vec::new();
    for e in &new.entries {
        match before.get(&id(e)) {
            None => found.push((e, Delta::Added(e.clone()))),
            Some(old) => {
                let changes = entry_changes(old, e);
                if !changes.is_empty() {
                    found.push((e, Delta::Changed(changes)));
                }
            }
        }
    }
    for e in &old.entries {
        if !after.contains_key(&id(e)) {
            found.push((e, Delta::Removed));
        }
    }
    found.sort_by_key(|(e, _)| {
        (
            e.kind,
            e.name.clone(),
            method_rank(e.method.as_deref().unwrap_or_default()),
        )
    });

    // Methods of one route that changed alike read as one item.
    let mut items: Vec<(String, Delta)> = Vec::new();
    // The entry the last item is for, and its methods so far.
    let mut last: Option<(&Entry, String)> = None;
    for (e, delta) in found {
        let method = e.method.clone().unwrap_or_default();
        if let Some((prev, methods)) = &mut last
            && let Some(item) = items.last_mut()
            && e.kind == Kind::Route
            && prev.kind == Kind::Route
            && prev.name == e.name
            && same_delta(&item.1, &delta, prev, e)
        {
            methods.push_str(", ");
            methods.push_str(&method);
            item.0 = e.title_with(methods);
            continue;
        }
        items.push((e.title(), delta));
        last = Some((e, method));
    }
    Diff { items }
}

fn same_delta(a: &Delta, b: &Delta, a_entry: &Entry, b_entry: &Entry) -> bool {
    match (a, b) {
        (Delta::Removed, Delta::Removed) => true,
        (Delta::Changed(x), Delta::Changed(y)) => x == y,
        (Delta::Added(_), Delta::Added(_)) => {
            a_entry.facts == b_entry.facts && a_entry.inside == b_entry.inside
        }
        _ => false,
    }
}

fn entry_changes(old: &Entry, new: &Entry) -> Vec<Change> {
    let mut out = fact_changes(None, &old.facts, &new.facts, &[]);
    let own = out.clone();
    // A part of a route that one plan doesn't single out had the route's
    // own rules there.
    let mut titles: Vec<String> = Vec::new();
    for inner in old.inside.iter().chain(&new.inside) {
        let title = inner.title();
        if !titles.contains(&title) {
            titles.push(title);
        }
    }
    let policy_facts = |e: &Entry| -> Vec<Fact> {
        e.facts
            .iter()
            .filter(|f| is_policy(&f.key))
            .cloned()
            .collect()
    };
    for title in titles {
        let side = |e: &Entry| {
            e.inside
                .iter()
                .find(|i| i.title() == title)
                .map_or_else(|| policy_facts(e), |i| i.facts.clone())
        };
        out.extend(fact_changes(Some(&title), &side(old), &side(new), &own));
    }
    out
}

/// The facts that differ. `own` holds the route's own changes, so a part
/// of the route doesn't repeat one it only inherits.
fn fact_changes(at: Option<&String>, old: &[Fact], new: &[Fact], own: &[Change]) -> Vec<Change> {
    let mut keys: Vec<&String> = Vec::new();
    for f in old.iter().chain(new) {
        if !keys.contains(&&f.key) {
            keys.push(&f.key);
        }
    }
    let mut out = Vec::new();
    for key in keys {
        let a = old.iter().find(|f| &f.key == key);
        let b = new.iter().find(|f| &f.key == key);
        let text = |f: Option<&Fact>| f.map(|f| f.text.clone());
        if text(a) == text(b) {
            continue;
        }
        // Both only repeat the server's value, which its entry reports.
        if a.is_some_and(|f| !f.shown) && b.is_some_and(|f| !f.shown) {
            continue;
        }
        let change = Change {
            at: at.cloned(),
            key: key.clone(),
            old: text(a),
            new: text(b),
            direction: direction(a, b),
        };
        let inherited = own
            .iter()
            .any(|c| c.key == change.key && c.old == change.old && c.new == change.new);
        if !inherited {
            out.push(change);
        }
    }
    out
}

#[cfg(test)]
mod tests;
