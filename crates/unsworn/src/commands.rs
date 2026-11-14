//! `check`, `routes`, `openapi` and `init`.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use unsworn_core::config::OrOff;
use unsworn_core::policy::{AuthReq, Layer};
use unsworn_core::{Project, Route, export, targets};

use crate::load;
use crate::workers::{PoolOptions, Pools, SocketDir};

fn load_or_fail(root: &Path, config: &str) -> Result<Project, String> {
    let compiled = load::load(root, config);
    let errors = load::print_diagnostics(&compiled.diagnostics);
    compiled.project.ok_or_else(|| format!("{errors} error(s)"))
}

pub async fn check(
    root: &Path,
    config: &str,
    deep: bool,
    target: Option<&str>,
) -> Result<(), String> {
    let compiled = load::load(root, config);
    let mut errors = load::print_diagnostics(&compiled.diagnostics);
    let Some(project) = compiled.project else {
        return Err(format!("{errors} error(s)"));
    };
    if let Some(t) = target {
        let diags = targets::check(&project, t);
        errors += load::print_diagnostics(&diags);
    }
    if deep {
        let sockets = SocketDir::create().map_err(|e| e.to_string())?;
        let pools = Pools::new(
            &project,
            PoolOptions {
                root: root.to_path_buf(),
                dev: true,
                build: true,
                socket_dir: sockets.path().to_path_buf(),
                log_tap: None,
            },
        );
        for (runtime, missing, err) in pools.start_all().await {
            if let Some(e) = err {
                eprintln!("✗ [runtimes.{runtime}]: {e}");
                errors += 1;
                continue;
            }
            for (handler, reason) in &missing {
                eprintln!("✗ {handler}: {reason}");
                errors += 1;
            }
            if pools.get(&runtime).is_some_and(|p| !p.verify_supported()) {
                eprintln!(
                    "! [runtimes.{runtime}]: its workers don't support `verify`, so handler names weren't checked"
                );
            }
        }
        pools.shutdown(Duration::from_secs(1)).await;
    }
    if errors > 0 {
        return Err(format!("{errors} error(s)"));
    }
    let tools = project.mcp.as_ref().map_or(0, |m| m.tools.len());
    let n = project.routes.len() - tools;
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if project.mcp.is_some() {
        println!(
            "✓ {n} route{} and {tools} MCP tool{} OK",
            plural(n),
            plural(tools)
        );
    } else {
        println!("✓ {n} route{} OK", plural(n));
    }
    Ok(())
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
                    format!("{}[{}]", a.scheme, a.scopes.join(" "))
                }
            })
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn layer_summary(layer: &Layer) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(a) = &layer.auth {
        out.push(format!("auth={}", auth_label(a)));
    }
    let mut onoff = |name: &str, set: bool, off: bool| {
        if off {
            out.push(format!("{name}=off"));
        } else if set {
            out.push(name.to_string());
        }
    };
    onoff(
        "cors",
        matches!(layer.cors, Some(OrOff::On(_))),
        matches!(layer.cors, Some(OrOff::Off)),
    );
    onoff(
        "origin",
        matches!(layer.origin, Some(OrOff::On(_))),
        matches!(layer.origin, Some(OrOff::Off)),
    );
    onoff(
        "ip",
        matches!(layer.ip, Some(OrOff::On(_))),
        matches!(layer.ip, Some(OrOff::Off)),
    );
    onoff(
        "rate_limit",
        matches!(layer.rate_limit, Some(OrOff::On(_))),
        matches!(layer.rate_limit, Some(OrOff::Off)),
    );
    onoff(
        "cache",
        matches!(layer.cache, Some(OrOff::On(_))),
        matches!(layer.cache, Some(OrOff::Off)),
    );
    onoff(
        "headers",
        matches!(layer.headers, Some(OrOff::On(_))),
        matches!(layer.headers, Some(OrOff::Off)),
    );
    if let Some(t) = layer.timeout {
        out.push(format!(
            "timeout={}",
            unsworn_core::units::format_duration(t)
        ));
    }
    if let Some(b) = layer.max_body {
        out.push(format!("max_body={b}"));
    }
    if let Some(s) = layer.stream_body {
        out.push(format!("stream_body={s}"));
    }
    match &layer.before {
        Some(OrOff::On(list)) => out.push(format!(
            "before=[{}]",
            list.iter()
                .map(|h| h.raw.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Some(OrOff::Off) => out.push("before=off".into()),
        None => {}
    }
    out
}

/// The auth a request needs. A tool call is checked against the endpoint's
/// auth first, then against the tool's own.
fn route_auth(project: &Project, route: &Route, auth: &AuthReq) -> String {
    let own = route
        .mcp_tool()
        .zip(project.mcp.as_ref())
        .and_then(|(name, mcp)| Some((mcp, mcp.tool(name)?)));
    match own {
        Some((mcp, tool)) if tool.own.auth.is_some() => {
            let endpoint = project.route_effective(&project.routes[mcp.route]);
            format!("{}, then {}", auth_label(&endpoint.auth), auth_label(auth))
        }
        _ => auth_label(auth),
    }
}

/// `TOOL` for an MCP tool's hidden route, otherwise the methods.
fn route_method(r: &Route) -> String {
    match r.mcp_tool() {
        Some(_) => "TOOL".into(),
        None => r.method_label(),
    }
}

/// The tool name for an MCP tool's hidden route, otherwise the path.
fn route_path(r: &Route) -> String {
    match r.mcp_tool() {
        Some(name) => name.to_string(),
        None => r.path.clone(),
    }
}

pub fn routes(root: &Path, config: &str, policies: bool, as_json: bool) -> Result<(), String> {
    let project = load_or_fail(root, config)?;
    if as_json {
        let list: Vec<Value> = project
            .routes
            .iter()
            .map(|r| {
                let eff = project.route_effective(r);
                json!({
                    "method": route_method(r),
                    "path": route_path(r),
                    "tool": r.mcp_tool(),
                    "host": r.host,
                    "action": r.action.describe(),
                    "source": r.source.to_string(),
                    "policies": eff,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&list).unwrap_or_default()
        );
        return Ok(());
    }
    let rows: Vec<[String; 4]> = project
        .routes
        .iter()
        .map(|r| {
            [
                route_method(r),
                match &r.host {
                    Some(h) => format!("{h}{}", r.path),
                    None => route_path(r),
                },
                r.action.describe(),
                r.source.to_string(),
            ]
        })
        .collect();
    let header = ["METHOD", "PATH", "ACTION", "SOURCE"];
    let mut widths = header.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: [&str; 4]| {
        format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            cells[0],
            cells[1],
            cells[2],
            cells[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        )
    };
    println!("{}", line(header));
    for (row, route) in rows.iter().zip(&project.routes) {
        println!("{}", line([&row[0], &row[1], &row[2], &row[3]]).trim_end());
        if policies {
            let eff = project.route_effective(route);
            println!("    auth: {}", route_auth(&project, route, &eff.auth));
            for (source, layer) in &route.layers {
                let summary = layer_summary(layer);
                if !summary.is_empty() {
                    println!("    from {source}: {}", summary.join(", "));
                }
            }
            for p in project.prefixes_for(route) {
                let summary = layer_summary(&p.layer);
                if !summary.is_empty() {
                    println!("    from {} ({}): {}", p.source, p.path, summary.join(", "));
                }
            }
        }
    }
    if !project.prefixes.is_empty() && !policies {
        println!();
        println!("Policy prefixes:");
        for p in &project.prefixes {
            println!(
                "  {}  {}  {}",
                p.path,
                layer_summary(&p.layer).join(", "),
                p.source
            );
        }
    }
    Ok(())
}

pub fn openapi(root: &Path, config: &str, output: Option<&Path>) -> Result<(), String> {
    let project = load_or_fail(root, config)?;
    let doc = export::openapi(&project);
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    match output {
        Some(path) => {
            std::fs::write(path, text + "\n")
                .map_err(|e| format!("can't write {}: {e}", path.display()))?;
            eprintln!("✓ wrote {}", path.display());
        }
        None => println!("{text}"),
    }
    Ok(())
}

const STARTER: &str = r#"#:schema https://raw.githubusercontent.com/j4crev/unsworn/main/schema/unsworn.schema.json
# Unsworn config. Docs: https://github.com/j4crev/unsworn/blob/main/docs/config.md

[server]
listen = "0.0.0.0:8080"

# File-based routes: routes/users.[id].get.ts serves GET /users/{id}.
# [routes]
# dir = "routes"

# How to run handlers written in TypeScript (with the Bun SDK).
# [runtimes.ts]
# extensions = [".ts"]
# command = "bun run node_modules/@unswornjs/sdk/src/worker.ts"
# protocol = "framed"

[[route]]
path = "/health"
respond = { status = 200, json = { ok = true } }

# Put Unsworn in front of an existing app:
# [[route]]
# path = "/{*rest}"
# proxy = "http://127.0.0.1:3000"
"#;

pub fn init(root: &Path) -> Result<(), String> {
    for name in load::CONFIG_NAMES {
        if root.join(name).exists() {
            return Err(format!("{name} already exists"));
        }
    }
    let path = root.join("unsworn.toml");
    std::fs::write(&path, STARTER).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    println!("✓ created unsworn.toml");
    println!("  run `unsworn dev` and open http://localhost:8080/health");
    Ok(())
}
