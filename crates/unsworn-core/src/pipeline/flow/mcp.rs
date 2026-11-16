//! The MCP endpoint: reading JSON-RPC requests, per-tool policies, and
//! either answering them (tools as handlers) or forwarding them to a
//! proxied MCP server.
//!
//! The `[mcp]` policies that don't depend on the body (origin, IP rules,
//! auth) already ran in `prepare`. Everything else needs the message: the
//! `[mcp]` rate limit runs once its headers are checked, and a call to a
//! configured tool then gets the tool's own policies. See `docs/mcp.md`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use serde_json::{Map, Value, json};
use unsworn_proto::{Phase, Request as ProtoRequest};

use super::{
    Ctx, Prepared, Resp, empty_response, json_response, problem, rate_limited, run_befores,
};
use crate::config::{OrOff, ResponseValidation};
use crate::mcp::rpc::{self, Headers, Message, RpcError};
use crate::mcp::{McpEndpoint, McpTool};
use crate::pipeline::host::{Body, CallError, Host, ProxyRequest, Reply};
use crate::pipeline::{header_map, query_pairs, retry_secs};
use crate::policy::{AuthReq, Effective};
use crate::project::{Project, Proxy, Route};
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
pub(super) async fn serve<H: Host>(
    ctx: &Ctx<'_, H>,
    route: &Route,
    eff: &Effective,
    prepared: Prepared<H>,
) -> Resp<H> {
    let project = ctx.project;
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
    let headers = Headers::from_http(&parts.headers);
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
    let trusted = rpc::trusted_headers(&parts.headers, call.as_ref().map(|(t, _)| *t));

    if let Some(limit) = &eff.rate_limit
        && let Err(resp) = rate_limited(
            ctx.rate_limit(route.id, limit, &trusted, claims.as_ref())
                .await,
        )
    {
        return resp;
    }

    let call = call.map(|(tool, args)| ToolCall {
        mcp,
        tool,
        args,
        msg: &msg,
        trusted,
    });
    match mcp.proxy.clone() {
        Some(upstream) => proxied(ctx, eff, upstream, call, parts, body, context, claims).await,
        None => hosted(ctx, mcp, &msg, call, parts, context, claims).await,
    }
}

/// Forwards a request to the MCP server behind the proxy, after a
/// configured tool's own policies.
#[allow(clippy::too_many_arguments)]
async fn proxied<H: Host>(
    ctx: &Ctx<'_, H>,
    eff: &Effective,
    upstream: Arc<Proxy>,
    call: Option<ToolCall<'_>>,
    parts: http::request::Parts,
    body: Bytes,
    context: Map<String, Value>,
    claims: Option<Map<String, Value>>,
) -> Resp<H> {
    let Some(call) = call else {
        let forward = forward(ctx, upstream, parts, body);
        return match ctx.host.timeout(eff.timeout, forward).await {
            Some(resp) => resp,
            None => problem(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "the request took longer than {}s",
                    eff.timeout.as_secs_f64()
                ),
            ),
        };
    };
    let checked = match call.check(ctx, &parts, context, claims).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let timeout = checked.timeout;
    match ctx
        .host
        .timeout(timeout, forward(ctx, upstream, parts, body))
        .await
    {
        Some(resp) => resp,
        None => call.error(timed_out(timeout)),
    }
}

/// Answers a request itself: Unsworn is the MCP server, and each tool is a
/// handler.
async fn hosted<H: Host>(
    ctx: &Ctx<'_, H>,
    mcp: &McpEndpoint,
    msg: &Message,
    call: Option<ToolCall<'_>>,
    parts: http::request::Parts,
    context: Map<String, Value>,
    claims: Option<Map<String, Value>>,
) -> Resp<H> {
    let Some(id) = &msg.id else {
        // No client notifications need an answer in MCP 2026-07-28.
        return empty_response(StatusCode::ACCEPTED);
    };
    let ok = |result| json_response(StatusCode::OK, &rpc::result(id, result, Some(&mcp.server)));
    match msg.method.as_str() {
        "server/discover" => ok(rpc::discover(mcp)),
        "tools/list" => {
            if msg.params.get("cursor").is_some() {
                // The whole list fits in one page, so no cursor is valid.
                return rpc_error(Some(id), &RpcError::invalid_params("unknown cursor"));
            }
            let visible = visible_tools(ctx, mcp, &parts).await;
            ok(rpc::list_tools(mcp, |t| visible.contains(&t.name)))
        }
        "tools/call" => match call {
            Some(call) => match call.check(ctx, &parts, context, claims).await {
                Ok(checked) => call.run(ctx, checked).await,
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
async fn visible_tools<H: Host>(
    ctx: &Ctx<'_, H>,
    mcp: &McpEndpoint,
    parts: &http::request::Parts,
) -> Vec<String> {
    if !rpc::list_varies_by_caller(mcp) {
        return mcp.tools.iter().map(|t| t.name.clone()).collect();
    }
    let query = query_pairs(parts.uri.query());
    // Tools often share an `auth`; check each distinct one once.
    let mut verdicts: Vec<(AuthReq, bool)> = Vec::new();
    let mut out = Vec::new();
    for tool in &mcp.tools {
        let allowed = match &tool.own.auth {
            None => true,
            Some(auth) => match verdicts.iter().find(|(a, _)| a == auth) {
                Some((_, v)) => *v,
                None => {
                    let eff = ctx.project.tool_effective(tool);
                    let v = ctx.authenticate(&parts.headers, &eff, &query).await.is_ok();
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
    async fn check<H: Host>(
        &self,
        ctx: &Ctx<'_, H>,
        parts: &http::request::Parts,
        mut context: Map<String, Value>,
        mut claims: Option<Map<String, Value>>,
    ) -> Result<Checked, Resp<H>> {
        let tool = self.tool;
        let eff = ctx.project.tool_effective(tool);
        let route = &ctx.project.routes[tool.route];

        // A missing scope is an HTTP error, so clients can ask for more access.
        if tool.own.auth.is_some() {
            let query = query_pairs(parts.uri.query());
            let (more, tool_claims) = match ctx.authenticate(&parts.headers, &eff, &query).await {
                Ok(outcome) => outcome,
                Err(resp) => {
                    // A call that fails the tool's `auth` still counts
                    // against its rate limit, by the client's address,
                    // unless it failed because a verifier is down.
                    if resp.status() != StatusCode::SERVICE_UNAVAILABLE
                        && let Some(OrOff::On(limit)) = &tool.own.rate_limit
                        && let RateLimit::Limited { retry_after } =
                            ctx.rate_limit(tool.route, limit, &self.trusted, None).await
                    {
                        return Err(super::problem_response(crate::pipeline::too_many_requests(
                            retry_after,
                        )));
                    }
                    return Err(resp);
                }
            };
            if let Some(auth) = more.get("auth") {
                context.insert("auth".into(), auth.clone());
            }
            if tool_claims.is_some() {
                claims = tool_claims;
            }
        }

        // The rest are reported to the model as tool errors.
        if let Some(OrOff::On(limit)) = &tool.own.rate_limit {
            match ctx
                .rate_limit(tool.route, limit, &self.trusted, claims.as_ref())
                .await
            {
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
        if let Some(rule) = &tool.input_rule
            && let Err(errors) = rule.check(&self.args, "arguments")
        {
            return Err(self.error(rpc::invalid_arguments(&errors)));
        }
        let mut mcp = json!({ "tool": tool.name, "protocol_version": self.msg.protocol_version });
        if let Some(client) = &self.msg.client {
            mcp["client"] = client.clone();
        }
        context.insert("mcp".into(), mcp);
        let template = match &tool.binding {
            // A route offered as a tool: its handler gets the same request a
            // REST client would send.
            Some(b) => {
                let bound = b.bind(&self.args);
                ProtoRequest {
                    route: route.path.clone(),
                    method: b.method.clone(),
                    path: bound.path,
                    params: bound.params,
                    query: bound.query,
                    raw_query: bound.raw_query,
                    headers: header_map(&self.trusted),
                    json: bound.json.map(Into::into),
                    ..Default::default()
                }
            }
            None => ProtoRequest {
                route: route.path.clone(),
                method: "POST".into(),
                path: parts.uri.path().to_string(),
                raw_query: parts.uri.query().map(String::from),
                headers: header_map(&self.trusted),
                json: Some(self.args.clone().into()),
                ..Default::default()
            },
        };
        if !eff.before.is_empty() {
            let deadline = ctx.deadline(eff.timeout);
            match run_befores(ctx, &eff, &template, None, context, deadline).await {
                Ok(c) => context = c,
                Err(resp) => return Err(self.error(rejection_text(ctx, resp).await)),
            }
        }
        Ok(Checked {
            timeout: eff.timeout,
            context,
            template,
        })
    }

    /// Calls the tool's handler and turns its reply into a tool result.
    async fn run<H: Host>(&self, ctx: &Ctx<'_, H>, checked: Checked) -> Resp<H> {
        let route = &ctx.project.routes[self.tool.route];
        let Some(handler) = route.handler() else {
            return self.error("The tool has no handler.");
        };
        let Checked {
            timeout,
            context,
            template: mut request,
        } = checked;
        request.handler = handler.raw.clone();
        request.phase = Phase::Handle;
        request.context = context;
        let deadline = ctx.deadline(timeout);
        let reply = ctx.host.call(handler, request, Body::Empty, deadline).await;
        let response = match reply {
            Ok(Reply::Response { response, .. }) => response,
            Ok(Reply::Continue(_)) => {
                tracing::error!(handler = %handler.raw, "a tool handler sent `continue`, which only before handlers may send");
                return self.error("The tool failed.");
            }
            Err(CallError::Timeout) => return self.error(timed_out(timeout)),
            Err(e) => {
                tracing::error!(handler = %handler.raw, tool = %self.tool.name, "{e}");
                return self.error(if ctx.host.dev() {
                    format!("The tool failed: {e}")
                } else {
                    "The tool failed.".to_string()
                });
            }
        };
        let ok = (200..300).contains(&response.status);
        let json = response.json.as_ref().map(unsworn_proto::Json::to_value);
        let mode = ctx.project.server.response_validation;
        if ok
            && mode != ResponseValidation::Off
            && let Some(rule) = &route.validation.response
        {
            let problem = match &json {
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
            json: json.as_ref(),
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
    fn error<B>(&self, text: impl Into<String>) -> crate::pipeline::host::Response<B> {
        let id = self.msg.id.clone().unwrap_or(Value::Null);
        // A proxied server's identity isn't ours to report.
        let server = self.mcp.hosted().then_some(&self.mcp.server);
        json_response(
            StatusCode::OK,
            &rpc::result(&id, rpc::tool_error(text), server),
        )
    }
}

fn rpc_error<B>(id: Option<&Value>, error: &RpcError) -> crate::pipeline::host::Response<B> {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST);
    json_response(status, &error.to_json(id))
}

async fn forward<H: Host>(
    ctx: &Ctx<'_, H>,
    upstream: Arc<Proxy>,
    parts: http::request::Parts,
    body: Bytes,
) -> Resp<H> {
    ctx.host
        .proxy(ProxyRequest {
            proxy: upstream,
            request: http::Request::from_parts(parts, Body::bytes(Some(body))),
            client_ip: ctx.client_ip,
        })
        .await
}

/// What a `before` handler's rejection says, for the model.
async fn rejection_text<H: Host>(ctx: &Ctx<'_, H>, resp: Resp<H>) -> String {
    let status = resp.status();
    // A body too long to be a message is left out.
    let bytes = ctx
        .collect(resp.into_body(), 16 * 1024)
        .await
        .unwrap_or_default();
    rpc::rejection_text(status, &bytes)
}
