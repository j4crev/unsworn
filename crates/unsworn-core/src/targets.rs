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
    let Some((_, version)) = TARGETS.iter().find(|(t, _)| *t == target) else {
        let known: Vec<&str> = TARGETS.iter().map(|(t, _)| *t).collect();
        return vec![
            Diagnostic::error("--target", format!("unknown target \"{target}\""))
                .hint(format!("known targets: {}", known.join(", "))),
        ];
    };
    match target {
        "server" => Vec::new(),
        "cloudflare" => cloudflare(project),
        _ => planned(project, target, version),
    }
}

/// File extensions a Worker can import as a handler module.
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
    let server = &project.server;
    if server.tls.is_some() {
        out.push(Diagnostic::warning(
            "[server.tls]",
            "ignored on cloudflare, which terminates TLS",
        ));
    }
    if server.admin_listen.is_some() {
        out.push(Diagnostic::warning(
            "[server].admin_listen",
            "ignored on cloudflare: there's no metrics or admin endpoint; use Workers Logs and analytics",
        ));
    }
    if !server.trusted_proxies.is_empty() {
        out.push(Diagnostic::warning(
            "[server].trusted_proxies",
            "ignored on cloudflare: the client IP comes from CF-Connecting-IP",
        ));
    }
    for refs in project.handlers_by_runtime().values() {
        for raw in refs {
            let (file, _) = crate::policy::HandlerRef::split(raw);
            let ext = file.rsplit('.').next().unwrap_or("");
            if ext == "wasm" {
                out.push(
                    Diagnostic::error(
                        format!("handler {raw}"),
                        "WASM handlers can't be called directly on cloudflare yet",
                    )
                    .hint("export a JS function that instantiates the module and calls it"),
                );
            } else if !ISOLATE_EXTENSIONS.contains(&ext) {
                out.push(
                    Diagnostic::error(
                        format!("handler {raw}"),
                        format!("\"{file}\" can't run in a Worker"),
                    )
                    .hint("only JavaScript and TypeScript handlers run on cloudflare; use the server target for other languages"),
                );
            }
        }
    }
    out
}

/// Targets that aren't built yet: what's known not to work there.
fn planned(project: &Project, target: &str, version: &str) -> Vec<Diagnostic> {
    let mut out = vec![Diagnostic::warning(
        "--target",
        format!("the {target} target is planned for {version}; this only checks capabilities"),
    )];
    let stateless_memory = project.store.backend == crate::config::StoreBackend::Memory;
    for route in &project.routes {
        let eff = project.route_effective(route);
        if stateless_memory && eff.rate_limit.is_some() {
            out.push(
                Diagnostic::error(
                    format!("{}.rate_limit", route.source.loc()),
                    "needs a shared store on this target",
                )
                .hint("set [store] backend to a shared store, or remove rate_limit"),
            );
        }
        if stateless_memory && eff.cache.is_some() {
            out.push(
                Diagnostic::error(
                    format!("{}.cache", route.source.loc()),
                    "needs a shared store on this target",
                )
                .hint("set [store] backend to a shared store, or remove cache"),
            );
        }
    }
    out
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
        // Planned targets still need a shared store for the same project.
        let v = check(&p, "vercel");
        assert_eq!(v.iter().filter(|d| d.is_error()).count(), 2);
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
    }
}
