//! Deployment targets and what each one supports.

use crate::diag::Diagnostic;
use crate::project::Project;

/// Known targets and the version that adds them.
pub const TARGETS: &[(&str, &str)] = &[
    ("server", "v1"),
    ("cloudflare", "v2"),
    ("vercel", "v3"),
    ("lambda", "v3"),
    ("netlify", "v3"),
];

/// Checks a project against a target's capabilities.
pub fn check(project: &Project, target: &str) -> Vec<Diagnostic> {
    if !TARGETS.iter().any(|(t, _)| *t == target) {
        let known: Vec<&str> = TARGETS.iter().map(|(t, _)| *t).collect();
        return vec![
            Diagnostic::error("--target", format!("unknown target \"{target}\""))
                .hint(format!("known targets: {}", known.join(", "))),
        ];
    }
    let mut out = match target {
        "server" => return Vec::new(),
        "cloudflare" => cloudflare(project),
        // The server itself runs there, in a container, so what's
        // reported is the little that differs, and nothing after it.
        "lambda" => return lambda(project),
        _ => functions(project, target),
    };
    // Spans are sent from every target; metrics and logs are server-only.
    if let Some(telemetry) = &project.telemetry {
        let server_only = [
            (telemetry.metrics, "[telemetry].metrics"),
            (telemetry.logs, "[telemetry].logs"),
        ];
        for (_, at) in server_only.into_iter().filter(|(on, _)| *on) {
            out.push(Diagnostic::warning(
                at,
                format!("ignored on {target}: only the server sends them"),
            ));
        }
    }
    // Functions, outbound services and shadows run on every target. What
    // differs is said here.
    if let Some(route) = project.routes.iter().find(|r| r.shadow.is_some()) {
        out.push(Diagnostic::warning(
            format!("{}.shadow", route.source.loc()),
            format!(
                "on {target}, a shadow's differences are logged, and there are no figures: `unsworn top` reads a server"
            ),
        ));
    }
    if let Some((name, _)) = project
        .outbound
        .iter()
        .find(|(_, service)| !service.headers.is_empty())
    {
        out.push(
            Diagnostic::warning(
                format!("[outbound.{name}]"),
                format!(
                    "on {target}, handlers run in the same process as Unsworn, so one can read the variables this service's headers come from"
                ),
            )
            .hint("Unsworn still adds the headers and checks the method and path; on a server, handlers can't reach the variables at all"),
        );
    }
    out
}

/// File extensions a JS host can import as a handler module.
const ISOLATE_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "ts", "mts", "cts", "jsx", "tsx"];

/// What runs differently, or not at all, on Cloudflare Workers.
fn cloudflare(project: &Project) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if project.store.backend == crate::config::StoreBackend::Redis {
        out.push(
            Diagnostic::error(
                "[store].backend",
                "Redis isn't used on cloudflare: rate limits use a Durable Object and the cache uses the Cache API",
            )
            .hint("remove [store] for this target; the build sets up Cloudflare's store"),
        );
    }
    ignored_server_settings(
        project,
        "cloudflare",
        "the client IP comes from CF-Connecting-IP",
        "use Workers Logs and analytics",
        &mut out,
    );
    js_handlers_only(project, "cloudflare", "a Worker", &mut out);
    no_unix_upstreams(project, "cloudflare", &mut out);
    // The Cache API keeps a copy in each data center, and a write is
    // handled in one of them.
    if let Some(route) = project.routes.iter().find(|r| !r.invalidates.is_empty()) {
        out.push(
            Diagnostic::warning(
                format!("{}.invalidates", route.source.loc()),
                "on cloudflare, cached responses are kept per data center: a write invalidates the copies where it was handled, and the others live out their `ttl`",
            )
            .hint("keep `ttl` as short as stale data can be tolerated there, or use the server target with one store"),
        );
    }
    out
}

/// What runs differently, or not at all, in a serverless function: one
/// Node function runs the whole project, and nothing outlives a request
/// except what's in the store.
fn functions(project: &Project, target: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let store = &project.store;
    match store.backend {
        crate::config::StoreBackend::Redis if !store.is_rest() => out.push(
            Diagnostic::error(
                "[store].url",
                format!("functions on {target} can't keep a Redis connection open, so they reach Redis through a REST API"),
            )
            .hint("set `url` to the REST API's https:// URL and `token` to its token (Upstash, or anything that speaks its protocol)"),
        ),
        crate::config::StoreBackend::Redis => {}
        crate::config::StoreBackend::Memory => {
            let hint = "set [store] backend = \"redis\" with a REST API's `url` and `token`, or remove it";
            for route in &project.routes {
                let eff = project.route_effective(route);
                if eff.rate_limit.is_some() {
                    out.push(
                        Diagnostic::error(
                            format!("{}.rate_limit", route.source.loc()),
                            "needs a shared store on this target: each instance of the function would count on its own",
                        )
                        .hint(hint),
                    );
                }
                if eff.cache.is_some() {
                    out.push(
                        Diagnostic::error(
                            format!("{}.cache", route.source.loc()),
                            "needs a shared store on this target: each instance of the function would fill its own cache",
                        )
                        .hint(hint),
                    );
                }
            }
        }
    }
    ignored_server_settings(
        project,
        target,
        "the client IP comes from the platform",
        "use the platform's logs and metrics",
        &mut out,
    );
    js_handlers_only(project, target, "a Node function", &mut out);
    no_unix_upstreams(project, target, &mut out);
    out
}

/// What differs on AWS Lambda, where the `unsworn` binary runs in a
/// container image behind AWS's Lambda Web Adapter. Handlers in any
/// language, HTTP runtimes and Unix-socket upstreams all work as they do
/// on a server: it is one.
fn lambda(project: &Project) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let store = &project.store;
    match store.backend {
        crate::config::StoreBackend::Redis if store.is_rest() => out.push(
            Diagnostic::error(
                "[store].url",
                "on lambda the server speaks Redis itself, as it does anywhere: a REST API's URL is for the function targets",
            )
            .hint("set `url` to a redis:// or rediss:// URL the function can reach, and drop `token`"),
        ),
        crate::config::StoreBackend::Redis => {}
        crate::config::StoreBackend::Memory => {
            let hint = "set [store] backend = \"redis\" with a redis:// or rediss:// `url`, or remove it";
            for route in &project.routes {
                let eff = project.route_effective(route);
                if eff.rate_limit.is_some() {
                    out.push(
                        Diagnostic::error(
                            format!("{}.rate_limit", route.source.loc()),
                            "needs a shared store on this target: each instance of the function would count on its own",
                        )
                        .hint(hint),
                    );
                }
                if eff.cache.is_some() {
                    out.push(
                        Diagnostic::error(
                            format!("{}.cache", route.source.loc()),
                            "needs a shared store on this target: each instance of the function would fill its own cache",
                        )
                        .hint(hint),
                    );
                }
            }
        }
    }
    let server = &project.server;
    if server.tls.is_some() {
        out.push(Diagnostic::warning(
            "[server.tls]",
            "not needed on lambda: the function's URL or API terminates TLS, and the adapter reaches the server over plain HTTP",
        ));
    }
    // Every request arrives from the adapter, beside the server.
    let loopback = server
        .trusted_proxies
        .iter()
        .any(|p| p == "all" || p.starts_with("127."));
    if !loopback {
        out.push(
            Diagnostic::warning(
                "[server].trusted_proxies",
                "on lambda every request arrives from the adapter at 127.0.0.1, so that's the address `ip` rules, rate limits by `ip` and the access log see",
            )
            .hint("set trusted_proxies = [\"127.0.0.1\"] to read the client's address from X-Forwarded-For, which Lambda sets"),
        );
    }
    out
}

/// `[server]` settings that only mean something to the binary.
fn ignored_server_settings(
    project: &Project,
    target: &str,
    client_ip: &str,
    instead: &str,
    out: &mut Vec<Diagnostic>,
) {
    let server = &project.server;
    if server.tls.is_some() {
        out.push(Diagnostic::warning(
            "[server.tls]",
            format!("ignored on {target}, which terminates TLS"),
        ));
    }
    if server.admin_listen.is_some() {
        out.push(Diagnostic::warning(
            "[server].admin_listen",
            format!("ignored on {target}: there's no metrics or admin endpoint; {instead}"),
        ));
    }
    if !server.trusted_proxies.is_empty() {
        out.push(Diagnostic::warning(
            "[server].trusted_proxies",
            format!("ignored on {target}: {client_ip}"),
        ));
    }
}

/// Handlers are called in the same isolate or process as the core, so only
/// JavaScript and TypeScript ones can run. `place` is what that is here.
fn js_handlers_only(project: &Project, target: &str, place: &str, out: &mut Vec<Diagnostic>) {
    for refs in project.handlers_by_runtime().values() {
        for raw in refs {
            let (file, _) = crate::policy::HandlerRef::split(raw);
            let ext = file.rsplit('.').next().unwrap_or("");
            if ext == "wasm" {
                out.push(
                    Diagnostic::error(
                        format!("handler {raw}"),
                        format!("WASM handlers can't be called directly on {target} yet"),
                    )
                    .hint("export a JS function that instantiates the module and calls it"),
                );
            } else if !ISOLATE_EXTENSIONS.contains(&ext) {
                out.push(
                    Diagnostic::error(
                        format!("handler {raw}"),
                        format!("\"{file}\" can't run in {place}"),
                    )
                    .hint(format!("only JavaScript and TypeScript handlers run on {target}; use the server target for other languages")),
                );
            }
        }
    }
}

/// A proxy's upstream on a Unix socket is on the machine the binary runs
/// on, which these targets don't have.
fn no_unix_upstreams(project: &Project, target: &str, out: &mut Vec<Diagnostic>) {
    let unix = |proxy: &crate::project::Proxy| {
        proxy
            .upstreams
            .iter()
            .any(|u| u.starts_with(crate::project::UNIX_UPSTREAM))
    };
    let hint = "use the upstream's http:// or https:// URL";
    // An `http` runtime is a server the binary starts beside itself.
    for (name, runtime) in &project.runtimes {
        if runtime.protocol == crate::config::Protocol::Http {
            out.push(
                Diagnostic::error(
                    format!("[runtimes.{name}]"),
                    format!("an `http` runtime is a process the server starts, and {target} can't run one"),
                )
                .hint("deploy its server on its own and `proxy` to its URL, or use the server target"),
            );
        }
    }
    for route in &project.routes {
        if let crate::project::Action::Proxy(proxy) = &route.action
            && unix(proxy)
        {
            out.push(
                Diagnostic::error(
                    format!("{}.proxy", route.source.loc()),
                    format!("a Unix socket can't be reached from {target}"),
                )
                .hint(hint),
            );
        }
    }
    if let Some(proxy) = project.mcp.as_ref().and_then(|mcp| mcp.proxy.as_ref())
        && unix(proxy)
    {
        out.push(
            Diagnostic::error(
                "[mcp].proxy",
                format!("a Unix socket can't be reached from {target}"),
            )
            .hint(hint),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Host;

    struct Mem(Vec<(&'static str, String)>);

    impl Host for Mem {
        fn read(&self, path: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, c)| c.clone())
        }
        fn exists(&self, path: &str) -> bool {
            self.0.iter().any(|(p, _)| *p == path)
        }
        fn list(&self, _dir: &str) -> Result<Vec<String>, String> {
            Err("no".into())
        }
        fn env(&self, _key: &str) -> Option<String> {
            None
        }
    }

    fn project(config: &str) -> Project {
        let host = Mem(vec![
            ("unsworn.toml", format!("{RUNTIMES}{config}")),
            ("src/a.ts", String::new()),
            ("src/b.py", String::new()),
        ]);
        let c = crate::compile(&host, "unsworn.toml");
        c.project.unwrap_or_else(|| panic!("{:?}", c.diagnostics))
    }

    const RUNTIMES: &str = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"

[runtimes.py]
extensions = [".py"]
command = "python worker.py"
"#;

    #[test]
    fn cloudflare_runs_js_handlers_and_brings_its_store() {
        let p = project(
            r#"
[[route]]
path = "/a"
handler = "src/a.ts#a"
rate_limit = { requests = 1, per = "1s" }
cache = { ttl = "1s" }
"#,
        );
        assert!(
            check(&p, "cloudflare").is_empty(),
            "{:?}",
            check(&p, "cloudflare")
        );
        // Functions need a shared store for the same project.
        let v = check(&p, "vercel");
        assert_eq!(v.iter().filter(|d| d.is_error()).count(), 2);
    }

    #[test]
    fn functions_reach_redis_over_rest() {
        let route = r#"
[[route]]
path = "/a"
handler = "src/a.ts#a"
rate_limit = { requests = 1, per = "1s" }
"#;
        let rest = project(&format!(
            "[store]\nbackend = \"redis\"\nurl = \"https://x.upstash.io\"\ntoken = \"t\"\n{route}"
        ));
        assert_eq!(check(&rest, "vercel"), vec![]);

        let tcp = project(&format!(
            "[store]\nbackend = \"redis\"\nurl = \"rediss://cache:6380\"\n{route}"
        ));
        let errors: Vec<_> = check(&tcp, "vercel")
            .into_iter()
            .filter(|d| d.is_error())
            .map(|d| d.location)
            .collect();
        assert_eq!(errors, ["[store].url"]);

        assert_eq!(check(&rest, "netlify"), vec![]);

        // Lambda runs the server itself, which speaks Redis and runs any
        // language. What it asks for is a store instances share, and to
        // be told the adapter is where requests arrive from.
        let errors = |p: &Project| -> Vec<String> {
            let found = check(p, "lambda").into_iter().filter(|d| d.is_error());
            found.map(|d| d.location).collect()
        };
        assert_eq!(errors(&rest), ["[store].url"]);
        assert_eq!(errors(&tcp), Vec::<String>::new());
        let python = project("[[route]]\npath = \"/b\"\nhandler = \"src/b.py\"\n");
        assert_eq!(errors(&python), Vec::<String>::new());
        let memory = project(route);
        assert_eq!(errors(&memory).len(), 1);
        let warned = |p: &Project| {
            check(p, "lambda")
                .iter()
                .any(|d| d.location == "[server].trusted_proxies")
        };
        assert!(warned(&tcp));
        let trusting = project(&format!(
            "[server]\ntrusted_proxies = [\"127.0.0.1\"]\n\n[store]\nbackend = \"redis\"\nurl = \"rediss://cache:6380\"\n{route}"
        ));
        assert_eq!(check(&trusting, "lambda"), vec![]);

        let python = project("[[route]]\npath = \"/b\"\nhandler = \"src/b.py\"\n");
        assert!(check(&python, "vercel").iter().any(|d| d.is_error()));
    }

    #[test]
    fn only_the_server_sends_metrics_and_logs() {
        let p = project(
            r#"
[outbound.otel]
url = "https://otlp.example.com"

[telemetry]
outbound = "otel"
metrics = true
logs = true
"#,
        );
        for target in ["cloudflare", "vercel", "netlify"] {
            let warnings: Vec<_> = check(&p, target)
                .into_iter()
                .map(|d| (d.is_error(), d.location))
                .collect();
            let expected = ["[telemetry].metrics", "[telemetry].logs"]
                .map(|at| (false, at.to_string()));
            assert_eq!(warnings, expected, "{target}");
        }
        assert_eq!(check(&p, "server"), vec![]);
        // Lambda runs the server.
        assert!(
            check(&p, "lambda")
                .iter()
                .all(|d| !d.location.starts_with("[telemetry]"))
        );
    }

    #[test]
    fn cloudflare_rejects_other_languages_and_redis() {
        let p = project(
            r#"
[store]
backend = "redis"
url = "redis://localhost"

[server]
trusted_proxies = ["10.0.0.0/8"]

[[route]]
path = "/b"
handler = "src/b.py"
before = ["src/a.ts#check"]
"#,
        );
        let d = check(&p, "cloudflare");
        let errors: Vec<_> = d
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.location.as_str())
            .collect();
        assert_eq!(errors, ["[store].backend", "handler src/b.py"]);
        assert!(
            d.iter()
                .any(|d| !d.is_error() && d.location == "[server].trusted_proxies")
        );
        assert_eq!(check(&p, "server"), vec![]);
        assert!(check(&p, "nope")[0].is_error());

        let unix = project("[[route]]\npath = \"/{*rest}\"\nproxy = \"unix:/run/app.sock\"\n");
        assert_eq!(check(&unix, "server"), vec![]);
        for target in ["cloudflare", "vercel"] {
            let errors: Vec<_> = check(&unix, target)
                .into_iter()
                .filter(|d| d.is_error())
                .map(|d| d.message)
                .collect();
            assert_eq!(
                errors,
                [format!("a Unix socket can't be reached from {target}")]
            );
        }
    }
}
