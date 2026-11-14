//! The MCP endpoint: reading JSON-RPC requests, per-tool policies, and
//! forwarding to a proxied MCP server.
//!
//! The `[mcp]` policies that don't depend on the body (origin, IP rules,
//! auth) already ran in `prepare`. Everything else needs the message: the
//! `[mcp]` rate limit runs once its headers are checked, and a call to a
//! configured tool then gets the tool's own policies. See `docs/mcp.md`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use serde_json::{Map, Value, json};
use unsworn_core::Project;
use unsworn_core::config::OrOff;
use unsworn_core::mcp::rpc::{self, Headers, Message, RpcError};
use unsworn_core::mcp::{McpEndpoint, McpTool};
use unsworn_core::policy::Effective;
use unsworn_core::project::{Proxy, Route};
use unsworn_proto::Request as ProtoRequest;

use super::super::proxy;
use super::super::support::{Body, full, json_response, problem};
use super::{
    Ctx, Prepared, authenticate_request, check_rate_limit, collect_up_to, header_map,
    rate_limit_unavailable, retry_secs, run_befores, too_many_requests,
};
use crate::store::RateLimit;

/// The time a request to the endpoint may take: the longest of the
/// endpoint's timeout and every tool's.
pub(super) fn longest_timeout(project: &Project, eff: &Effective) -> Duration {
    project
        .mcp
        .iter()
        .flat_map(|m| &m.tools)
        .map(|t| project.tool_effective(t).timeout)
        .fold(eff.timeout, Duration::max)
}

/// Handles one request to the MCP endpoint.
pub(super) async fn serve(
    ctx: &Ctx,
    route: &Route,
    eff: &Effective,
    prepared: Prepared,
) -> Response<Body> {
    let project = ctx.generation.project.clone();
    let Some(mcp) = project.mcp.as_ref() else {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, "no [mcp] endpoint");
    };
    let Prepared {
        parts,
        body_bytes,
        context,
        claims,
        ..
    } = prepared;
    let body = body_bytes.unwrap_or_default();
    let headers = mcp_headers(&parts.headers);
    let msg = match rpc::read(&body, &headers) {
        Ok(m) => m,
        Err(r) => return rpc_error(r.id.as_ref(), &r.error),
    };
    let call = msg
        .tool_call()
        .and_then(|(name, args)| Some((mcp.tool(name)?, args)));
    if let Some((tool, args)) = &call
        && let Err(e) = rpc::check_param_headers(tool, args, &headers)
    {
        return rpc_error(msg.id.as_ref(), &e);
    }
    let trusted = trusted_headers(&parts.headers, call.as_ref().map(|(t, _)| *t));

    if let Some(limit) = &eff.rate_limit {
        match check_rate_limit(ctx, route.id, limit, &trusted, claims.as_ref()).await {
            RateLimit::Allowed => {}
            RateLimit::Limited { retry_after } => return too_many_requests(retry_after),
            RateLimit::Unavailable => return rate_limit_unavailable(),
        }
    }

    let Some(upstream) = mcp.proxy.clone() else {
        // `make_generation` refuses hosted tools until they're served.
        return problem(
            StatusCode::NOT_IMPLEMENTED,
            "serving MCP tools as handlers isn't available yet",
        );
    };
    let Some((tool, args)) = call else {
        return match tokio::time::timeout(eff.timeout, forward(ctx, upstream, parts, body)).await {
            Ok(resp) => resp,
            Err(_) => problem(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "the request took longer than {}s",
                    eff.timeout.as_secs_f64()
                ),
            ),
        };
    };
    let call = ToolCall {
        mcp,
        tool,
        args,
        msg: &msg,
        trusted,
    };
    match call.check(ctx, &project, &parts, context, claims).await {
        Ok(timeout) => {
            match tokio::time::timeout(timeout, forward(ctx, upstream, parts, body)).await {
                Ok(resp) => resp,
                Err(_) => call.error(format!(
                    "The tool didn't finish within {}s.",
                    timeout.as_secs_f64()
                )),
            }
        }
        Err(resp) => resp,
    }
}

/// A `tools/call` for a tool in `[mcp]`.
struct ToolCall<'a> {
    mcp: &'a McpEndpoint,
    tool: &'a McpTool,
    args: Value,
    msg: &'a Message,
    /// The request headers, without `Mcp-Param-*` headers that weren't
    /// checked against the arguments.
    trusted: HeaderMap,
}

impl ToolCall<'_> {
    /// Applies the tool's own policies. Returns the call's timeout, or the
    /// response that ends the call.
    async fn check(
        &self,
        ctx: &Ctx,
        project: &Project,
        parts: &http::request::Parts,
        mut context: Map<String, Value>,
        mut claims: Option<Map<String, Value>>,
    ) -> Result<Duration, Response<Body>> {
        let tool = self.tool;
        let eff = project.tool_effective(tool);
        let route = &project.routes[tool.route];

        // A missing scope is an HTTP error, so clients can ask for more access.
        if tool.own.auth.is_some() {
            let query: Vec<(String, String)> = parts
                .uri
                .query()
                .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
                .unwrap_or_default();
            let (more, tool_claims) =
                authenticate_request(ctx, &parts.headers, &eff, &query).await?;
            if let Some(auth) = more.get("auth") {
                context.insert("auth".into(), auth.clone());
            }
            if tool_claims.is_some() {
                claims = tool_claims;
            }
        }

        // The rest are reported to the model as tool errors.
        if let Some(OrOff::On(limit)) = &tool.own.rate_limit {
            match check_rate_limit(ctx, tool.route, limit, &self.trusted, claims.as_ref()).await {
                RateLimit::Allowed => {}
                RateLimit::Limited { retry_after } => {
                    return Err(self.error(format!(
                        "Rate limit exceeded for {}. Try again in {} seconds.",
                        tool.name,
                        retry_secs(retry_after)
                    )));
                }
                RateLimit::Unavailable => {
                    return Err(self.error(
                        "Rate limiting is unavailable, so the tool wasn't called. Try again later.",
                    ));
                }
            }
        }
        if let Some(rule) = &route.validation.body
            && let Err(errors) = rule.check(&self.args, "arguments")
        {
            return Err(self.error(rpc::invalid_arguments(&errors)));
        }
        if !eff.before.is_empty() {
            let mut mcp =
                json!({ "tool": tool.name, "protocol_version": self.msg.protocol_version });
            if let Some(client) = &self.msg.client {
                mcp["client"] = client.clone();
            }
            context.insert("mcp".into(), mcp);
            let template = ProtoRequest {
                route: route.path.clone(),
                method: "POST".into(),
                path: parts.uri.path().to_string(),
                raw_query: parts.uri.query().map(String::from),
                headers: header_map(&self.trusted),
                json: Some(self.args.clone()),
                ..Default::default()
            };
            let deadline = Instant::now() + eff.timeout;
            if let Err(resp) = run_befores(ctx, &eff, &template, None, context, deadline).await {
                return Err(self.error(rejection_text(resp).await));
            }
        }
        Ok(eff.timeout)
    }

    /// A tool result with `isError: true`, answered by Unsworn.
    fn error(&self, text: impl Into<String>) -> Response<Body> {
        let id = self.msg.id.clone().unwrap_or(Value::Null);
        // A proxied server's identity isn't ours to report.
        let server = self.mcp.hosted().then_some(&self.mcp.server);
        json_response(
            StatusCode::OK,
            &rpc::result(&id, rpc::tool_error(text), server),
        )
    }
}

fn rpc_error(id: Option<&Value>, error: &RpcError) -> Response<Body> {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST);
    json_response(status, &error.to_json(id))
}

/// The MCP request metadata headers.
fn mcp_headers(headers: &HeaderMap) -> Headers {
    let mut out = Headers::default();
    let mut text = |name: &str, label: &str| match headers.get(name).map(|v| v.to_str()) {
        None => None,
        Some(Ok(v)) => Some(v.to_string()),
        Some(Err(_)) => {
            out.invalid.push(label.to_string());
            None
        }
    };
    let protocol_version = text("mcp-protocol-version", "MCP-Protocol-Version");
    let method = text("mcp-method", "Mcp-Method");
    let name = text("mcp-name", "Mcp-Name");
    out.protocol_version = protocol_version;
    out.method = method;
    out.name = name;
    for (k, v) in headers {
        if k.as_str().starts_with("mcp-param-") {
            match v.to_str() {
                Ok(v) => out.params.push((k.as_str().to_string(), v.to_string())),
                Err(_) => out.invalid.push(k.as_str().to_string()),
            }
        }
    }
    out
}

/// The headers policies may use as keys: `Mcp-Param-*` headers only when
/// they were checked against this call's arguments.
fn trusted_headers(headers: &HeaderMap, tool: Option<&McpTool>) -> HeaderMap {
    let mut out = headers.clone();
    let unchecked: Vec<_> = headers
        .keys()
        .filter(|k| {
            k.as_str().starts_with("mcp-param-")
                && !tool.is_some_and(|t| t.param_headers.iter().any(|p| p.header == k.as_str()))
        })
        .cloned()
        .collect();
    for k in unchecked {
        out.remove(k);
    }
    out
}

fn forward(
    ctx: &Ctx,
    upstream: Arc<Proxy>,
    parts: http::request::Parts,
    body: Bytes,
) -> futures_util::future::BoxFuture<'static, Response<Body>> {
    proxy::forward(
        ctx.shared.client.clone(),
        upstream,
        Request::from_parts(parts, full(body)),
        proxy::ForwardInfo {
            client_ip: ctx.client_ip,
            tls: ctx.tls,
            request_id: ctx.request_id.clone(),
        },
    )
}

/// What a `before` handler's rejection says, for the model.
async fn rejection_text(resp: Response<Body>) -> String {
    let status = resp.status();
    let (bytes, _) = collect_up_to(resp.into_body(), 16 * 1024).await;
    let reason = match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => ["detail", "message", "error"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| v.to_string()),
        Err(_) => String::from_utf8_lossy(&bytes).trim().to_string(),
    };
    let label = format!(
        "{} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    if reason.is_empty() {
        format!("The call was rejected ({}).", label.trim())
    } else {
        format!("The call was rejected ({}): {reason}", label.trim())
    }
}
