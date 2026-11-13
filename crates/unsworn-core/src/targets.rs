//! Deployment targets and what each one supports.

use crate::diag::Diagnostic;
use crate::project::{Action, Project};

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
    let mut out = Vec::new();
    if target == "server" {
        return out;
    }
    out.push(Diagnostic::warning(
        "--target",
        format!("the {target} target is planned for {version}; this only checks capabilities"),
    ));
    let stateless_memory = project.store.backend == "memory";
    for route in &project.routes {
        let eff = project.effective(route, "GET", &route.path.replace("{*", "{"));
        if stateless_memory && eff.rate_limit.is_some() {
            out.push(
                Diagnostic::error(
                    format!("{}.rate_limit", route.source),
                    "needs a shared store on this target",
                )
                .hint("set [store] backend to a shared store, or remove rate_limit"),
            );
        }
        if stateless_memory && eff.cache.is_some() {
            out.push(
                Diagnostic::error(
                    format!("{}.cache", route.source),
                    "needs a shared store on this target",
                )
                .hint("set [store] backend to a shared store, or remove cache"),
            );
        }
        if target == "cloudflare"
            && let Action::Handler(h) = &route.action
        {
            let ext = h.file.rsplit('.').next().unwrap_or("");
            if !matches!(ext, "js" | "ts" | "mjs" | "wasm") {
                out.push(
                        Diagnostic::error(
                            format!("{}.handler", route.source),
                            format!("\"{}\" can't run in an edge isolate", h.file),
                        )
                        .hint("only JS/TS and WASM handlers run on edge isolates; use a container target for other languages"),
                    );
            }
        }
    }
    out
}
