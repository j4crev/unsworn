//! The MCP endpoint: reading JSON-RPC requests, per-tool policies, and
//! either answering them (tools as handlers) or forwarding them to a
//! proxied MCP server.
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
use unsworn_core::config::{OrOff, ResponseValidation};
use unsworn_core::mcp::rpc::{self, Headers, Message, RpcError};
use unsworn_core::mcp::{McpEndpoint, McpTool};
use unsworn_core::policy::{AuthReq, Effective};
use unsworn_core::project::{Proxy, Route};
use unsworn_proto::{Phase, Request as ProtoRequest};

use super::super::proxy;
use super::super::support::{Body, empty, full, json_response, problem};
use super::{
    Ctx, Prepared, authenticate_request, check_rate_limit, collect_up_to, for_handler, header_map,
    rate_limit_unavailable, retry_secs, run_befores, too_many_requests,
};
use crate::store::RateLimit;
use crate::workers::{CallBody, CallError, Reply};

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

    let call = call.map(|(tool, args)| ToolCall {
        mcp,
        tool,
        args,
        msg: &msg,
        trusted,
    });
    match mcp.proxy.clone() {
        Some(upstream) => {
            proxied(
                ctx, &project, eff, upstream, call, parts, body, context, claims,
            )
            .await
        }
        None => hosted(ctx, &project, mcp, &msg, call, parts, context, claims).await,
    }
}

/// Forwards a request to the MCP server behind the proxy, after a
/// configured tool's own policies.
#[allow(clippy::too_many_arguments)]
async fn proxied(
    ctx: &Ctx,
    project: &Project,
    eff: &Effective,
    upstream: Arc<Proxy>,
    call: Option<ToolCall<'_>>,
    parts: http::request::Parts,
    body: Bytes,
    context: Map<String, Value>,
    claims: Option<Map<String, Value>>,
) -> Response<Body> {
    let Some(call) = call else {
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
    let checked = match call.check(ctx, project, &parts, context, claims).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let timeout = checked.timeout;
    match tokio::time::timeout(timeout, forward(ctx, upstream, parts, body)).await {
        Ok(resp) => resp,
        Err(_) => call.error(timed_out(timeout)),
    }
}

/// Answers a request itself: Unsworn is the MCP server, and each tool is a
/// handler.
#[allow(clippy::too_many_arguments)]
async fn hosted(
    ctx: &Ctx,
    project: &Project,
    mcp: &McpEndpoint,
    msg: &Message,
    call: Option<ToolCall<'_>>,
    parts: http::request::Parts,
    context: Map<String, Value>,
    claims: Option<Map<String, Value>>,
) -> Response<Body> {
    let Some(id) = &msg.id else {
        // No client notifications need an answer in MCP 2026-07-28.
        let mut resp = Response::new(empty());
        *resp.status_mut() = StatusCode::ACCEPTED;
        return resp;
    };
    let ok = |result| json_response(StatusCode::OK, &rpc::result(id, result, Some(&mcp.server)));
    match msg.method.as_str() {
        "server/discover" => ok(rpc::discover(mcp)),
        "tools/list" => {
            if msg.params.get("cursor").is_some() {
                // The whole list fits in one page, so no cursor is valid.
                return rpc_error(Some(id), &RpcError::invalid_params("unknown cursor"));
            }
            let visible = visible_tools(ctx, project, mcp, &parts).await;
            ok(rpc::list_tools(mcp, |t| visible.contains(&t.name)))
        }
        "tools/call" => match call {
            Some(call) => match call.check(ctx, project, &parts, context, claims).await {
                Ok(checked) => call.run(ctx, project, checked).await,
                Err(resp) => resp,
            },
            None => {
                let name = msg.tool_call().map(|(n, _)| n).unwrap_or_default();
                rpc_error(
                    Some(id),
                    &RpcError::invalid_params(format!("unknown tool: {name}")),
                )
            }
        },
        other => rpc_error(Some(id), &RpcError::method_not_found(other)),
    }
}

/// The names of the tools `tools/list` shows this caller: with
/// `list_tools = "permitted"`, only tools whose own `auth` the caller's
/// credentials satisfy.
async fn visible_tools(
    ctx: &Ctx,
    project: &Project,
    mcp: &McpEndpoint,
    parts: &http::request::Parts,
) -> Vec<String> {
    if !rpc::list_varies_by_caller(mcp) {
        return mcp.tools.iter().map(|t| t.name.clone()).collect();
    }
    let query = query_pairs(parts);
    // Tools often share an `auth`; check each distinct one once.
    let mut verdicts: Vec<(AuthReq, bool)> = Vec::new();
    let mut out = Vec::new();
    for tool in &mcp.tools {
        let allowed = match &tool.own.auth {
            None => true,
            Some(auth) => match verdicts.iter().find(|(a, _)| a == auth) {
                Some((_, v)) => *v,
                None => {
                    let eff = project.tool_effective(tool);
                    let v = authenticate_request(ctx, &parts.headers, &eff, &query)
                        .await
                        .is_ok();
                    verdicts.push((auth.clone(), v));
                    v
                }
            },
        };
        if allowed {
            out.push(tool.name.clone());
        }
    }
    out
}

fn query_pairs(parts: &http::request::Parts) -> Vec<(String, String)> {
    parts
        .uri
        .query()
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default()
}

fn timed_out(timeout: Duration) -> String {
    format!("The tool didn't finish within {}s.", timeout.as_secs_f64())
}

/// What a call needs once the tool's policies have passed.
struct Checked {
    timeout: Duration,
    /// Auth claims, `mcp`, and whatever `before` handlers added.
    context: Map<String, Value>,
    template: ProtoRequest,
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
    /// Applies the tool's own policies. Returns what the call needs, or the
    /// response that ends it.
    async fn check(
        &self,
        ctx: &Ctx,
        project: &Project,
        parts: &http::request::Parts,
        mut context: Map<String, Value>,
        mut claims: Option<Map<String, Value>>,
    ) -> Result<Checked, Response<Body>> {
        let tool = self.tool;
        let eff = project.tool_effective(tool);
        let route = &project.routes[tool.route];

        // A missing scope is an HTTP error, so clients can ask for more access.
        if tool.own.auth.is_some() {
            let query = query_pairs(parts);
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
        let mut mcp = json!({ "tool": tool.name, "protocol_version": self.msg.protocol_version });
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
        if !eff.before.is_empty() {
            let deadline = Instant::now() + eff.timeout;
            match run_befores(ctx, &eff, &template, None, context, deadline).await {
                Ok(c) => context = c,
                Err(resp) => return Err(self.error(rejection_text(resp).await)),
            }
        }
        Ok(Checked {
            timeout: eff.timeout,
            context,
            template,
        })
    }

    /// Calls the tool's handler and turns its reply into a tool result.
    async fn run(&self, ctx: &Ctx, project: &Project, checked: Checked) -> Response<Body> {
        let route = &project.routes[self.tool.route];
        let Some(handler) = route.handler() else {
            return self.error("The tool has no handler.");
        };
        let Some(pool) = ctx.generation.pools.get(&handler.runtime).cloned() else {
            return self.error("The tool's runtime isn't available.");
        };
        let request = for_handler(&checked.template, handler, Phase::Handle, &checked.context);
        let deadline = Instant::now() + checked.timeout;
        let reply = pool
            .call(request, CallBody::Empty, deadline, ctx.request_id.clone())
            .await;
        let response = match reply {
            Ok(Reply::Response { response, .. }) => response,
            Ok(Reply::Continue(_)) => {
                tracing::error!(handler = %handler.raw, "a tool handler sent `continue`, which only before handlers may send");
                return self.error("The tool failed.");
            }
            Err(CallError::Timeout) => return self.error(timed_out(checked.timeout)),
            Err(e) => {
                tracing::error!(handler = %handler.raw, tool = %self.tool.name, "{e}");
                return self.error(if ctx.shared.dev {
                    format!("The tool failed: {e}")
                } else {
                    "The tool failed.".to_string()
                });
            }
        };
        let ok = (200..300).contains(&response.status);
        let mode = project.server.response_validation;
        if ok
            && mode != ResponseValidation::Off
            && let Some(rule) = &route.validation.response
        {
            let problem = match &response.json {
                None => Some("it declares `output` but returned no JSON".to_string()),
                Some(j) => rule.check(j, "result").err().map(|errors| {
                    errors
                        .iter()
                        .map(|e| format!("{}: {}", e.location, e.message))
                        .collect::<Vec<_>>()
                        .join("; ")
                }),
            };
            if let Some(p) = problem {
                tracing::warn!(tool = %self.tool.name, "tool result doesn't match its output schema: {p}");
                if mode == ResponseValidation::Enforce {
                    return self.error("The tool's result doesn't match its output schema.");
                }
            }
        }
        let result = rpc::call_result(rpc::Reply {
            status: response.status,
            json: response.json.as_ref(),
            text: response.text.as_deref(),
            mcp_content: response.mcp_content.as_ref(),
            other_body: response.body_len.is_some() || response.stream,
        });
        let id = self.msg.id.clone().unwrap_or(Value::Null);
        json_response(
            StatusCode::OK,
            &rpc::result(&id, result, Some(&self.mcp.server)),
        )
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
