//! The policy merge, checked against a second implementation of its rule.
//!
//! The rule is short: a request gets `[defaults]`, then every statement
//! about it from the widest to the narrowest, so the narrowest one that
//! sets a policy decides it. The code that applies it is spread over the
//! compiler and the request path, where a mistake lets a request through
//! with the wrong policies and nothing looks broken. So this file states
//! the rule again, as plainly as it can be written, over configs made up
//! from a seed, and compares the two for every request it can think of.
//!
//! The reference shares nothing with the real thing but the config text:
//! it works from its own model of what was written, on strings. It doesn't
//! match routes (the router is trusted to say which route serves a
//! request), only what that route's request should end up with.

use std::time::Duration;

use super::tests::MemHost;
use super::*;

/// A small deterministic generator (SplitMix64), so a failure names a seed
/// that reproduces it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// The policies one table sets, as written.
#[derive(Debug, Clone, Default, PartialEq)]
struct Written {
    /// A scheme's name, or `none`.
    auth: Option<String>,
    timeout: Option<u64>,
    max_body: Option<u64>,
    /// Requests a minute, or `None` for `"off"`.
    rate_limit: Option<Option<u32>>,
    /// Seconds, or `None` for `"off"`.
    cache: Option<Option<u64>>,
    /// Handler files, or `None` for `"off"`.
    before: Option<Option<Vec<String>>>,
    sets: Vec<usize>,
}

impl Written {
    fn is_empty(&self) -> bool {
        self == &Written::default()
    }

    fn toml(&self) -> String {
        let mut out = String::new();
        if !self.sets.is_empty() {
            let names: Vec<String> = self.sets.iter().map(|i| format!("\"set{i}\"")).collect();
            out += &format!("use = [{}]\n", names.join(", "));
        }
        if let Some(a) = &self.auth {
            out += &format!("auth = \"{a}\"\n");
        }
        if let Some(t) = self.timeout {
            out += &format!("timeout = \"{t}s\"\n");
        }
        if let Some(m) = self.max_body {
            out += &format!("max_body = \"{m}KB\"\n");
        }
        match &self.rate_limit {
            Some(Some(n)) => out += &format!("rate_limit = {{ requests = {n}, per = \"1m\" }}\n"),
            Some(None) => out += "rate_limit = \"off\"\n",
            None => {}
        }
        match &self.cache {
            Some(Some(n)) => out += &format!("cache = {{ ttl = \"{n}s\" }}\n"),
            Some(None) => out += "cache = \"off\"\n",
            None => {}
        }
        match &self.before {
            Some(Some(list)) => {
                let files: Vec<String> = list.iter().map(|f| format!("\"{f}\"")).collect();
                out += &format!("before = [{}]\n", files.join(", "));
            }
            Some(None) => out += "before = \"off\"\n",
            None => {}
        }
        out
    }
}

#[derive(Debug, Clone)]
struct Statement {
    path: String,
    methods: Option<Vec<String>>,
    written: Written,
    /// A route (it has an action) or a policy-only entry.
    route: bool,
}

#[derive(Debug, Clone, Default)]
struct Model {
    defaults: Written,
    sets: Vec<Written>,
    /// Routes and entries, in the order they're written.
    statements: Vec<Statement>,
}

const FILES: [&str; 3] = ["h1.ts", "h2.ts", "h3.ts"];
const METHODS: [&str; 3] = ["GET", "POST", "PUT"];

fn written(rng: &mut Rng, sets: usize, may_use: bool) -> Written {
    let mut w = Written::default();
    if rng.chance(45) {
        w.auth = Some(rng.pick(&["s1", "s2", "none"]).to_string());
    }
    if rng.chance(25) {
        w.timeout = Some(1 + rng.below(9) as u64);
    }
    if rng.chance(20) {
        w.max_body = Some(1 + rng.below(9) as u64);
    }
    if rng.chance(25) {
        w.rate_limit = Some((!rng.chance(25)).then(|| 1 + rng.below(90) as u32));
    }
    if rng.chance(20) {
        w.cache = Some((!rng.chance(25)).then(|| 1 + rng.below(90) as u64));
    }
    if rng.chance(25) {
        w.before = Some((!rng.chance(25)).then(|| {
            let mut list = vec![rng.pick(&FILES).to_string()];
            if rng.chance(30) {
                let more = rng.pick(&FILES).to_string();
                if !list.contains(&more) {
                    list.push(more);
                }
            }
            list
        }));
    }
    if may_use && sets > 0 && rng.chance(20) {
        w.sets.push(rng.below(sets));
        if rng.chance(30) {
            let more = rng.below(sets);
            if !w.sets.contains(&more) {
                w.sets.push(more);
            }
        }
    }
    w
}

fn methods(rng: &mut Rng) -> Option<Vec<String>> {
    if rng.chance(45) {
        return None;
    }
    let mut list = vec![rng.pick(&METHODS).to_string()];
    if rng.chance(35) {
        let more = rng.pick(&METHODS).to_string();
        if !list.contains(&more) {
            list.push(more);
        }
    }
    Some(list)
}

/// Up to `max` segments, each a literal or a parameter.
fn segments(rng: &mut Rng, max: usize) -> Vec<String> {
    (0..rng.below(max + 1))
        .map(|i| match rng.below(5) {
            0 | 1 => format!("{{p{i}}}"),
            2 => "a".to_string(),
            3 => "b".to_string(),
            _ => "c".to_string(),
        })
        .collect()
}

fn model(rng: &mut Rng) -> Model {
    let mut m = Model::default();
    for _ in 0..rng.below(3) {
        m.sets.push(written(rng, 0, false));
    }
    if rng.chance(70) {
        m.defaults = written(rng, m.sets.len(), true);
    }
    for _ in 0..1 + rng.below(6) {
        let mut segs = segments(rng, 3);
        if rng.chance(30) {
            segs.push("{*rest}".to_string());
        }
        let written = match rng.chance(55) {
            true => written(rng, m.sets.len(), true),
            false => Written::default(),
        };
        let route = Statement {
            path: format!("/{}", segs.join("/")),
            methods: methods(rng),
            written,
            route: true,
        };
        // Two routes can't serve the same method and path.
        let same = |a: &Statement, b: &Statement| {
            let shape = |s: &Statement| -> Vec<String> {
                let name = |p: &&str| match p.starts_with('{') {
                    true => p.chars().take(2).collect(),
                    false => p.to_string(),
                };
                parts(&s.path).iter().map(name).collect()
            };
            let methods = match (&a.methods, &b.methods) {
                (Some(x), Some(y)) => x.iter().any(|m| y.contains(m)),
                _ => true,
            };
            methods && shape(a) == shape(b)
        };
        if !m.statements.iter().any(|other| same(other, &route)) {
            m.statements.push(route);
        }
    }
    let routes = m.statements.clone();
    for _ in 0..rng.below(7) {
        // A prefix, the path of one of the routes, or a path inside one.
        let path = match rng.below(3) {
            0 => {
                let mut segs = segments(rng, 2);
                segs.push("{*rest}".to_string());
                format!("/{}", segs.join("/"))
            }
            1 => rng.pick(&routes).path.clone(),
            _ => {
                let base = rng.pick(&routes).path.clone();
                let extra = segments(rng, 2);
                match base.strip_suffix("{*rest}") {
                    Some(prefix) if !extra.is_empty() => format!("{prefix}{}", extra.join("/")),
                    _ => base,
                }
            }
        };
        let mut w = written(rng, m.sets.len(), true);
        if w.is_empty() {
            w.timeout = Some(3);
        }
        m.statements.push(Statement {
            path,
            methods: methods(rng),
            written: w,
            route: false,
        });
    }
    // In the file, routes and entries come mixed.
    for i in (1..m.statements.len()).rev() {
        m.statements.swap(i, rng.below(i + 1));
    }
    m
}

fn config(m: &Model) -> String {
    let mut out = String::from(
        "[runtimes.ts]\nextensions = [\".ts\"]\ncommand = \"bun run worker.ts\"\n\n\
         [auth.s1]\ntype = \"jwt\"\nsecret = \"one\"\n\n[auth.s2]\ntype = \"jwt\"\nsecret = \"two\"\n\n",
    );
    for (i, set) in m.sets.iter().enumerate() {
        out += &format!("[policies.set{i}]\n{}\n", set.toml());
    }
    out += &format!("[defaults]\n{}\n", m.defaults.toml());
    for s in &m.statements {
        out += &format!("[[route]]\npath = \"{}\"\n", s.path);
        if let Some(list) = &s.methods {
            let quoted: Vec<String> = list.iter().map(|m| format!("\"{m}\"")).collect();
            out += &format!("method = [{}]\n", quoted.join(", "));
        }
        if s.route {
            out += "respond = { status = 200 }\n";
        }
        out += &s.written.toml();
        out += "\n";
    }
    out
}

// ---- The rule, written a second time ----

/// What a request ends up with.
#[derive(Debug, Default, PartialEq)]
struct Outcome {
    auth: String,
    timeout: u64,
    max_body: u64,
    rate_limit: Option<u32>,
    cache: Option<u64>,
    before: Vec<String>,
}

/// The policies as they stand part-way through the merge.
#[derive(Default)]
struct Merging {
    auth: Option<String>,
    timeout: Option<u64>,
    max_body: Option<u64>,
    rate_limit: Option<Option<u32>>,
    cache: Option<Option<u64>>,
    before: Option<Option<Vec<String>>>,
}

impl Merging {
    /// One table's own keys on top: each key it sets replaces what was
    /// there, except `before`, whose lists add up until an `"off"`.
    fn keys(&mut self, w: &Written) {
        if w.auth.is_some() {
            self.auth = w.auth.clone();
        }
        if w.timeout.is_some() {
            self.timeout = w.timeout;
        }
        if w.max_body.is_some() {
            self.max_body = w.max_body;
        }
        if w.rate_limit.is_some() {
            self.rate_limit = w.rate_limit;
        }
        if w.cache.is_some() {
            self.cache = w.cache;
        }
        match (&mut self.before, &w.before) {
            (_, None) => {}
            (slot, Some(None)) => *slot = Some(None),
            (Some(Some(have)), Some(Some(more))) => {
                for file in more {
                    if !have.contains(file) {
                        have.push(file.clone());
                    }
                }
            }
            (slot, Some(Some(more))) => *slot = Some(Some(more.clone())),
        }
    }
}

/// A table is its sets, in the order `use` lists them, then its own keys.
/// What that adds up to is then applied as one layer.
fn table(w: &Written, sets: &[Written]) -> Written {
    let mut merged = Merging::default();
    for &i in &w.sets {
        merged.keys(&sets[i]);
    }
    merged.keys(w);
    Written {
        auth: merged.auth,
        timeout: merged.timeout,
        max_body: merged.max_body,
        rate_limit: merged.rate_limit,
        cache: merged.cache,
        before: merged.before,
        sets: Vec::new(),
    }
}

fn parts(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn is_prefix(pattern: &str) -> bool {
    parts(pattern).last().is_some_and(|s| s.starts_with("{*"))
}

/// The segments of a pattern that a path's segments are compared with.
fn fixed_parts(pattern: &str) -> Vec<&str> {
    let mut p = parts(pattern);
    if is_prefix(pattern) {
        p.pop();
    }
    p
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let (want, have) = (fixed_parts(pattern), parts(path));
    let length = match is_prefix(pattern) {
        true => have.len() >= want.len(),
        false => have.len() == want.len(),
    };
    length
        && want
            .iter()
            .zip(&have)
            .all(|(w, h)| w.starts_with('{') || w == h)
}

fn method_matches(methods: &Option<Vec<String>>, method: &str) -> bool {
    match methods {
        None => true,
        Some(list) => list
            .iter()
            .any(|m| m == method || (m == "GET" && method == "HEAD")),
    }
}

/// How narrow a statement is. Compared in this order: a full path is
/// narrower than a prefix; then more segments; then a literal where the
/// other has a parameter, at the first segment where they differ; then
/// fewer methods.
fn narrowness(pattern: &str, methods: &Option<Vec<String>>) -> (bool, usize, Vec<bool>, usize) {
    let fixed = fixed_parts(pattern);
    let count = match methods {
        None => usize::MAX,
        Some(list) => {
            let head = list.iter().any(|m| m == "GET") && !list.iter().any(|m| m == "HEAD");
            list.len() + head as usize
        }
    };
    (
        !is_prefix(pattern),
        fixed.len(),
        fixed.iter().map(|s| !s.starts_with('{')).collect(),
        usize::MAX - count,
    )
}

/// What a request for `method` and `path`, served by the route at
/// `m.statements[route]`, ends up with.
fn expected(m: &Model, route: usize, method: &str, path: &str) -> Outcome {
    let own = &m.statements[route];
    let own_rank = narrowness(&own.path, &own.methods);
    // Every statement about this request: the policy entries that match
    // it, and the route's own policies.
    let mut about: Vec<(_, usize, &Statement)> = m
        .statements
        .iter()
        .enumerate()
        .filter(|(i, s)| match s.route {
            true => *i == route,
            false => path_matches(&s.path, path) && method_matches(&s.methods, method),
        })
        .map(|(i, s)| match s.route {
            // Among statements just as narrow, the route's own come last.
            true => (own_rank.clone(), usize::MAX, s),
            false => (narrowness(&s.path, &s.methods), i, s),
        })
        .collect();
    about.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));

    let mut merged = Merging::default();
    merged.keys(&table(&m.defaults, &m.sets));
    for (_, _, s) in about {
        merged.keys(&table(&s.written, &m.sets));
    }
    Outcome {
        auth: merged.auth.unwrap_or_else(|| "none".to_string()),
        timeout: merged.timeout.unwrap_or(30),
        max_body: merged.max_body.map_or(1024 * 1024, |kb| kb * 1024),
        rate_limit: merged.rate_limit.flatten(),
        cache: merged.cache.flatten(),
        before: merged.before.flatten().unwrap_or_default(),
    }
}

/// What the real merge gives.
fn actual(eff: &Effective) -> Outcome {
    Outcome {
        auth: match &eff.auth {
            AuthReq::Public => "none".to_string(),
            AuthReq::AnyOf(alts) => alts[0].scheme.clone(),
        },
        timeout: eff.timeout.as_secs(),
        max_body: eff.max_body,
        rate_limit: eff.rate_limit.as_ref().map(|r| r.requests),
        cache: eff.cache.as_ref().map(|c| c.ttl.as_secs()),
        before: eff.before.iter().map(|h| h.raw.clone()).collect(),
    }
}

/// Every path of up to three segments from a few names, one of which no
/// pattern has as a literal.
fn requests() -> Vec<String> {
    let names = ["a", "b", "c", "x"];
    let mut out = vec!["/".to_string()];
    for a in names {
        out.push(format!("/{a}"));
        for b in names {
            out.push(format!("/{a}/{b}"));
            for c in names {
                out.push(format!("/{a}/{b}/{c}"));
            }
        }
    }
    out
}

/// Compiles one generated config and compares every request. `None` when
/// the config doesn't compile, which the generator's choices sometimes
/// cause (two routes for one path, an entry no route matches, two
/// statements for the same requests setting one policy).
fn compare(seed: u64) -> Option<usize> {
    let m = model(&mut Rng(seed));
    let text = config(&m);
    let mut files = vec![("unsworn.toml", text.as_str())];
    files.extend(FILES.iter().map(|f| (*f, "")));
    let host = MemHost::with(&files);
    let project = compile(&host, "unsworn.toml").project?;

    // The route a compiled route was written as: routes keep the order
    // they were written in.
    let written: Vec<usize> = m
        .statements
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.route.then_some(i))
        .collect();
    let mut compared = 0;
    for path in requests() {
        for method in ["GET", "HEAD", "POST", "PUT", "DELETE"] {
            let MatchResult::Found(found) = project.match_request(None, method, &path) else {
                continue;
            };
            let got = actual(&project.effective(found.route, method, &path));
            let want = expected(&m, written[found.route.id], method, &path);
            assert_eq!(
                got,
                want,
                "seed {seed}: {method} {path}, served by {} {}\n\n{text}",
                found.route.method_label(),
                found.route.path,
            );
            compared += 1;
        }
    }
    Some(compared)
}

#[test]
fn the_merge_gives_what_the_rule_says() {
    let (mut compiled, mut compared) = (0, 0);
    let seeds = 4000;
    for seed in 0..seeds {
        if let Some(n) = compare(seed) {
            compiled += 1;
            compared += n;
        }
    }
    // The comparison only means something if enough configs get that far.
    // About half do: the rest mostly have two statements for the same
    // requests setting one policy, which is an error.
    assert!(
        compiled > 1500 && compared > 150_000,
        "only {compiled} of {seeds} generated configs compiled, and {compared} requests were compared"
    );
}

/// The reference has to be able to fail: a rule that agreed with anything
/// would prove nothing.
#[test]
fn the_reference_tells_narrower_from_wider() {
    let entry = |path: &str, methods: Option<&[&str]>, auth: &str| Statement {
        path: path.to_string(),
        methods: methods.map(|m| m.iter().map(|m| m.to_string()).collect()),
        written: Written {
            auth: Some(auth.to_string()),
            ..Default::default()
        },
        route: false,
    };
    let mut route = entry("/{*rest}", None, "s1");
    route.route = true;
    let m = Model {
        statements: vec![
            entry("/a/b", None, "s2"),
            route,
            entry("/a/{*rest}", None, "none"),
            entry("/a/{*rest}", Some(&["GET"]), "s2"),
        ],
        ..Default::default()
    };
    let auth = |method: &str, path: &str| expected(&m, 1, method, path).auth;
    assert_eq!(auth("POST", "/x"), "s1");
    assert_eq!(auth("POST", "/a/x"), "none");
    assert_eq!(auth("GET", "/a/x"), "s2");
    assert_eq!(auth("HEAD", "/a/x"), "s2");
    assert_eq!(auth("POST", "/a/b"), "s2");
    assert_eq!(
        expected(&m, 1, "GET", "/x").timeout,
        Duration::from_secs(30).as_secs()
    );
}
