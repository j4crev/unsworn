//! One request, from matching to response, with the host doing the I/O.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Method, StatusCode, header};
use serde_json::{Map, Value};
use unsworn_proto::{Phase, Request as ProtoRequest};

mod mcp;

use super::auth::{AuthOutcome, KeyCache, authenticate_request};
use super::host::{Body, CallError, Deadline, Host, ProxyRequest, Reply, Response, StaticRequest};
use super::{
    JsonBody, Problem, access, cache_key, check_body, check_content_length, decode_param,
    finish_headers, header_map, normalize_path, preflight_headers, query_pairs, rate_limit_bucket,
    redirect_location, response_head, unmet, validate_inputs,
};
use crate::config::{ResponseValidation, TrailingSlash};
use crate::policy::{CacheRule, Effective, HandlerRef, KeySource, RateLimit as RateLimitRule};
use crate::project::{Action, MatchResult, Project, RespondBody, Route};
use crate::rules;
use crate::store::{
    CachedResponse, MAX_CACHED_BODY, RateLimit, ResponseCache, cache_get, rate_limit,
};
use crate::validate::FieldError;

/// What a host needs to know about a handled request.
pub struct Handled<B> {
    pub response: Response<B>,
    /// The matched route's path, or `(none)`, for logs and metrics.
    pub route: String,
}

/// Routes requests for one loaded project. Hosts build one per project
/// (and per reload) and share it across requests.
pub struct Router {
    project: Arc<Project>,
    keys: Arc<KeyCache>,
    well_known: HashMap<String, Value>,
}

impl Router {
    /// `keys` usually outlives the project, so reloads keep loaded JWT keys.
    pub fn new(project: Arc<Project>, keys: Arc<KeyCache>) -> Router {
        let well_known = project.protected_resource_docs().into_iter().collect();
        Router {
            project,
            keys,
            well_known,
        }
    }

    pub fn project(&self) -> &Arc<Project> {
        &self.project
    }

    /// Handles one request. `client_ip` is the client's address as the host
    /// works it out (see [`super::ClientIp`]).
    pub async fn handle<H: Host>(
        &self,
        host: &H,
        req: http::Request<H::RequestBody>,
        client_ip: IpAddr,
    ) -> Handled<H::ResponseBody> {
        let mut req = normalized(req);
        // What Unsworn tells a proxied app about the caller, and the token
        // an app calls back with, are never the caller's own to send.
        super::proxy::strip_context(req.headers_mut());
        // Whatever the host passed, IPv4 is IPv4 (see `ClientIp::resolve`).
        let client_ip = client_ip.to_canonical();
        let ctx = Ctx {
            host,
            project: &self.project,
            keys: &self.keys,
            client_ip,
            origin: req
                .headers()
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                .map(String::from),
        };
        let path = req.uri().path().to_string();
        if let Some(doc) = self.well_known.get(&path) {
            return Handled {
                response: json_response(StatusCode::OK, doc),
                route: path,
            };
        }
        let (response, route) = route_request(&ctx, req).await;
        Handled { response, route }
    }
}

/// The request with its path normalized (see [`normalize_path`]), so
/// matching, policies, handlers and upstreams all see the same path.
fn normalized<B>(mut req: http::Request<B>) -> http::Request<B> {
    let std::borrow::Cow::Owned(path) = normalize_path(req.uri().path()) else {
        return req;
    };
    let path_and_query = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let mut parts = req.uri().clone().into_parts();
    if let Ok(pq) = http::uri::PathAndQuery::try_from(path_and_query) {
        parts.path_and_query = Some(pq);
        if let Ok(uri) = http::Uri::from_parts(parts) {
            *req.uri_mut() = uri;
        }
    }
    req
}

/// Per-request facts used across steps.
pub(super) struct Ctx<'a, H> {
    host: &'a H,
    project: &'a Project,
    keys: &'a KeyCache,
    client_ip: IpAddr,
    origin: Option<String>,
}

type Resp<H> = Response<<H as Host>::ResponseBody>;

pub(crate) fn empty_response<B>(status: StatusCode) -> Response<B> {
    let mut resp = http::Response::new(Body::Empty);
    *resp.status_mut() = status;
    resp
}

/// A JSON response.
pub fn json_response<B>(status: StatusCode, value: &Value) -> Response<B> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let mut resp = http::Response::new(Body::Bytes(body.into()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// An RFC 9457 problem response.
pub fn problem_response<B>(problem: Problem) -> Response<B> {
    let mut resp = http::Response::new(Body::Bytes(problem.body_bytes().into()));
    *resp.status_mut() = problem.status;
    *resp.headers_mut() = problem.headers;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(Problem::CONTENT_TYPE),
    );
    resp
}

fn problem<B>(status: StatusCode, detail: impl Into<String>) -> Response<B> {
    problem_response(Problem::new(status, detail))
}

fn problem_with<B>(
    status: StatusCode,
    detail: impl Into<String>,
    errors: Option<&[FieldError]>,
) -> Response<B> {
    problem_response(Problem::new(status, detail).with_errors(errors))
}

async fn route_request<H: Host>(
    ctx: &Ctx<'_, H>,
    req: http::Request<H::RequestBody>,
) -> (Resp<H>, String) {
    let project = ctx.project;
    let path = req.uri().path().to_string();
    // An absolute request target (and HTTP/2's `:authority`, which arrives
    // as one) names the host; `Host` only does when there's none (RFC 9112
    // section 3.2.2).
    let host_name = req
        .uri()
        .host()
        .or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
        })
        .map(String::from);
    let method = req.method().as_str().to_string();

    // CORS preflight: match the method the browser is asking about.
    if req.method() == Method::OPTIONS
        && let (Some(origin), Some(want)) = (
            ctx.origin.as_deref(),
            req.headers()
                .get(header::ACCESS_CONTROL_REQUEST_METHOD)
                .and_then(|v| v.to_str().ok()),
        )
        && let MatchResult::Found(m) = project.match_request(host_name.as_deref(), want, &path)
    {
        let eff = project.effective(m.route, want, &path);
        if let Some(cors) = &eff.cors {
            let methods = m
                .route
                .methods
                .clone()
                .unwrap_or_else(|| vec![want.to_string()]);
            let requested = req
                .headers()
                .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
                .and_then(|v| v.to_str().ok());
            let mut resp = empty_response(StatusCode::NO_CONTENT);
            *resp.headers_mut() = preflight_headers(cors, origin, requested, &methods);
            return (resp, m.route.path.clone());
        }
    }

    let mut matched = project.match_request(host_name.as_deref(), &method, &path);
    if matches!(matched, MatchResult::NotFound)
        && project.server.trailing_slash != TrailingSlash::Strict
        && path != "/"
    {
        let toggled = match path.strip_suffix('/') {
            Some(p) => p.to_string(),
            None => format!("{path}/"),
        };
        let alt = project.match_request(host_name.as_deref(), &method, &toggled);
        if let MatchResult::Found(_) = alt {
            if project.server.trailing_slash == TrailingSlash::Redirect {
                let location = super::checks::same_site(match req.uri().query() {
                    Some(q) => format!("{toggled}?{q}"),
                    None => toggled,
                });
                let mut resp = empty_response(StatusCode::PERMANENT_REDIRECT);
                if let Ok(v) = HeaderValue::from_str(&location) {
                    resp.headers_mut().insert(header::LOCATION, v);
                }
                return (resp, path);
            }
            matched = alt;
        }
    }

    match matched {
        MatchResult::NotFound => (
            problem(
                StatusCode::NOT_FOUND,
                format!("no route for {method} {path}"),
            ),
            "(none)".into(),
        ),
        MatchResult::MethodNotAllowed(allowed) => (
            problem_response(
                Problem::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    format!("{path} doesn't accept {method}"),
                )
                .header(header::ALLOW, allowed.join(", ")),
            ),
            "(none)".into(),
        ),
        MatchResult::Found(m) => {
            let route = m.route;
            let label = route.path.clone();
            let eff = project.effective(route, &method, &path);
            let params: Vec<(String, String)> = m
                .params
                .into_iter()
                .map(|(k, v)| (k, decode_param(&v)))
                .collect();
            let timeout = match route.action {
                // Each tool call has its own timeout, applied inside.
                Action::Mcp => mcp::longest_timeout(project, &eff),
                _ => eff.timeout,
            };
            let work = serve_route(ctx, req, route, &eff, params);
            let mut resp = match ctx.host.timeout(timeout, work).await {
                Some(r) => r,
                None => problem(
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("the request took longer than {}s", timeout.as_secs_f64()),
                ),
            };
            finish_headers(&eff, ctx.origin.as_deref(), resp.headers_mut());
            (resp, label)
        }
    }
}

/// What the checks before the action produce.
pub(super) struct Prepared<H: Host> {
    parts: http::request::Parts,
    template: ProtoRequest,
    raw_params: Vec<(String, String)>,
    raw_body: Option<Bytes>,
    body_bytes: Option<Bytes>,
    streaming_body: Option<H::RequestBody>,
    /// The handler gets `streaming_body` as it arrives.
    stream_body: bool,
    cache_key: Option<String>,
    context: Map<String, Value>,
    /// The verified token's claims, for policy keys like `auth.sub`.
    claims: Option<Map<String, Value>>,
}

impl<H: Host> Ctx<'_, H> {
    fn deadline(&self, timeout: Duration) -> Deadline {
        Deadline(self.host.monotonic() + timeout)
    }

    /// `body` is the request's, for a scheme that checks a signature made
    /// over it.
    async fn authenticate(
        &self,
        headers: &http::HeaderMap,
        eff: &Effective,
        query: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<AuthOutcome, Resp<H>> {
        authenticate_request(
            self.host,
            self.keys,
            self.project,
            headers,
            eff,
            query,
            body,
        )
        .await
        .map_err(problem_response)
    }

    /// Counts a request against a rate limit, on the limit's own counter.
    async fn rate_limit(
        &self,
        limit: &RateLimitRule,
        headers: &http::HeaderMap,
        claims: Option<&Map<String, Value>>,
    ) -> RateLimit {
        let bucket = rate_limit_bucket(limit, self.client_ip, headers, claims);
        let store = self.host.store();
        rate_limit(
            &store,
            &bucket,
            limit.requests,
            limit.per,
            self.project.store.on_error,
        )
        .await
    }

    /// Whether a cached response for `path` was made out of date after it
    /// was stored, by a route that `invalidates` the path.
    async fn invalidated(&self, path: &str, hit: &CachedResponse) -> bool {
        if !self.project.may_be_invalidated(path) {
            return false;
        }
        let segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(decode_param)
            .collect();
        let key = crate::store::path_key(segments.iter().map(String::as_str));
        crate::store::invalidated(&self.host.store(), &key, hit.stored_ms).await
    }

    /// Marks cached responses for these paths as out of date. It's done
    /// before the response goes out, so that whoever made the change
    /// reads it back fresh.
    pub(super) async fn invalidate(&self, path_keys: &[String]) {
        if path_keys.is_empty() {
            return;
        }
        let store = self.host.store();
        let (now, ttl) = (self.host.now_ms(), self.project.invalidation_ttl());
        for key in path_keys {
            if let Err(e) = crate::store::invalidate(&store, key, now, ttl).await {
                tracing::warn!("couldn't invalidate cached responses: {e}");
            }
        }
    }

    /// Reads a response body whole, if it's at most `limit` bytes.
    /// Otherwise gives back an equivalent body.
    async fn collect(
        &self,
        body: Body<H::ResponseBody>,
        limit: usize,
    ) -> Result<Bytes, Body<H::ResponseBody>> {
        match body {
            Body::Empty => Ok(Bytes::new()),
            Body::Bytes(b) if b.len() <= limit => Ok(b),
            Body::Bytes(b) => Err(Body::Bytes(b)),
            Body::Host(b) => self.host.collect(b, limit).await.map_err(Body::Host),
        }
    }
}

async fn serve_route<H: Host>(
    ctx: &Ctx<'_, H>,
    req: http::Request<H::RequestBody>,
    route: &Route,
    eff: &Effective,
    raw_params: Vec<(String, String)>,
) -> Resp<H> {
    let mut prepared = match prepare(ctx, req, route, eff, raw_params).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if matches!(route.action, Action::Mcp) {
        // On the heap, so that the other routes, which are most of them,
        // don't each carry the room an MCP request needs.
        return Box::pin(mcp::serve(ctx, eff, prepared)).await;
    }
    let deadline = ctx.deadline(eff.timeout);
    let befores = run_befores(
        ctx,
        eff,
        &prepared.template,
        prepared.raw_body.clone(),
        prepared.context.clone(),
        deadline,
    );
    match befores.await {
        Ok(context) => prepared.context = context,
        Err(resp) => return resp,
    }
    // The cache is read after `before` handlers, so one that turns a caller
    // away does so even when a response is cached.
    if let Some(key) = &prepared.cache_key
        && let Some(hit) = cache_get(&ctx.host.store(), key, ctx.host.now_ms()).await
        && !ctx.invalidated(prepared.parts.uri.path(), &hit).await
    {
        let method = &prepared.template.method;
        return cached_response(ctx, &hit, method, &prepared.parts.headers);
    }
    // A `HEAD` request may be answered from the cache, but its response
    // has no body to answer a later `GET` with, so it's never stored.
    let cache_key = prepared
        .cache_key
        .take()
        .filter(|_| prepared.template.method == "GET");
    // The paths this request makes out of date, with its own parameters
    // filled in.
    let outdated: Vec<String> = route
        .invalidates
        .iter()
        .filter_map(|target| {
            let params = &prepared.raw_params;
            target.key(&|name| {
                let found = params.iter().find(|(k, _)| k == name);
                found.map(|(_, v)| v.clone())
            })
        })
        .collect();
    let resp = run_action(ctx, route, prepared, deadline).await;
    if resp.status().as_u16() < 400 {
        ctx.invalidate(&outdated).await;
    }
    if let (Some(key), Some(rule)) = (cache_key, eff.cache.as_ref()) {
        return store_in_cache(ctx, key, rule, resp).await;
    }
    resp
}

/// Origin, auth, rate limit, validation, cache lookup and body reading.
async fn prepare<H: Host>(
    ctx: &Ctx<'_, H>,
    req: http::Request<H::RequestBody>,
    route: &Route,
    eff: &Effective,
    raw_params: Vec<(String, String)>,
) -> Result<Prepared<H>, Resp<H>> {
    // The body is taken apart from the rest now, since a signed request's
    // has to be read before its caller is known.
    let (parts, incoming) = req.into_parts();
    let mut incoming = Some(incoming);
    let method = parts.method.as_str().to_string();
    let query_pairs = query_pairs(parts.uri.query());
    access(eff, ctx.origin.as_deref(), ctx.client_ip).map_err(problem_response)?;

    // An MCP endpoint counts requests once their headers are checked
    // against the body, so header keys can be trusted.
    let is_mcp = matches!(route.action, Action::Mcp);
    let limit = eff.rate_limit.as_ref().filter(|_| !is_mcp);
    // A limit that doesn't depend on who the caller is runs first, so
    // requests with bad credentials count, and a remote key verifier is
    // never asked more often than the limit allows.
    let before_auth = |l: &&RateLimitRule| !matches!(l.key, KeySource::Claim(_));
    if let Some(limit) = limit.filter(before_auth) {
        rate_limited(ctx.rate_limit(limit, &parts.headers, None).await)?;
    }

    // A webhook's signature is made over its body, so that is read first:
    // after the limits that don't need the caller, and within `max_body`.
    let mut body_bytes: Option<Bytes> = None;
    if ctx.project.signs_body(&eff.auth)
        && let Some(body) = incoming.take()
    {
        check_content_length(&parts.headers, eff.max_body).map_err(problem_response)?;
        body_bytes = Some(
            ctx.host
                .read_body(body, eff.max_body)
                .await
                .map_err(problem_response)?,
        );
    }
    let authenticated = ctx
        .authenticate(&parts.headers, eff, &query_pairs, body_bytes.as_deref())
        .await;
    let (context, claims) = match authenticated {
        Ok(outcome) => outcome,
        Err(resp) => {
            // The limits that wait for the caller's claims (one keyed on a
            // claim, and an MCP endpoint's) count a request that fails
            // authentication against the client's address instead, so
            // guessing credentials is limited too. A verifier outage (`503`)
            // isn't the caller's doing, and doesn't count.
            let rejected = resp.status() != StatusCode::SERVICE_UNAVAILABLE;
            if rejected
                && let Some(limit) = eff
                    .rate_limit
                    .as_ref()
                    .filter(|l| is_mcp || !before_auth(l))
            {
                let headers = crate::mcp::rpc::trusted_headers(&parts.headers, None);
                rate_limited(ctx.rate_limit(limit, &headers, None).await)?;
            }
            return Err(resp);
        }
    };
    if let Some(limit) = limit.filter(|l| !before_auth(l)) {
        rate_limited(ctx.rate_limit(limit, &parts.headers, claims.as_ref()).await)?;
    }

    // Before the request's own values are looked at any further: a caller
    // who may not be here learns nothing about what the route expects.
    if let Some(rule) = unmet(
        &eff.require,
        parts.uri.path(),
        &query_pairs,
        &parts.headers,
        claims.as_ref(),
    ) {
        return Err(problem_response(not_allowed(rule)));
    }

    let (params, query) = validate_inputs(route, &raw_params, &query_pairs, &parts.headers)
        .map_err(problem_response)?;

    let scheme = context
        .get("auth")
        .and_then(|a| a.get("scheme"))
        .and_then(Value::as_str);
    let cache_key = eff.cache.as_ref().and_then(|rule| {
        cache_key(
            rule,
            &ctx.project.credential_headers(&eff.auth),
            route.id,
            &method,
            &parts.uri,
            &parts.headers,
            claims.as_ref(),
            scheme,
            ctx.client_ip,
        )
    });

    // A handler with `stream_body` reads the body as it arrives instead.
    let needs_body = (matches!(route.action, Action::Handler(_)) && !eff.stream_body)
        || route.validation.body.is_some()
        || is_mcp;
    let mut streaming_body = None;
    // Unless it was read to check a signature.
    if let Some(incoming) = incoming {
        check_content_length(&parts.headers, eff.max_body).map_err(problem_response)?;
        if needs_body {
            body_bytes = Some(
                ctx.host
                    .read_body(incoming, eff.max_body)
                    .await
                    .map_err(problem_response)?,
            );
        } else {
            streaming_body = Some(ctx.host.limit_body(incoming, eff.max_body));
        }
    }

    // MCP reads its own JSON-RPC body, with JSON-RPC errors.
    // A body with one reader, the route's handler, can go to it unread. A
    // `before` handler would be a second reader, and two parsers can
    // disagree about a body one of them should have refused.
    let may_leave_unread = ctx.host.takes_unread_json()
        && matches!(route.action, Action::Handler(_))
        && eff.before.is_empty();
    let json_body = if is_mcp {
        JsonBody::No
    } else {
        check_body(
            route,
            &parts.headers,
            body_bytes.as_deref(),
            may_leave_unread,
        )
        .map_err(problem_response)?
    };
    let is_json = json_body != JsonBody::No;
    let raw_body = if is_json {
        None
    } else {
        body_bytes.clone().filter(|b| !b.is_empty())
    };
    // Only a handler reads the copied headers: the route's own or a
    // `before` handler. Static answers and proxied requests skip the copy,
    // and so does MCP, which builds each tool's request itself.
    let for_a_handler = match route.action {
        Action::Handler(_) => true,
        Action::Mcp => false,
        _ => !eff.before.is_empty(),
    };
    let template = ProtoRequest {
        route: route.path.clone(),
        method,
        path: parts.uri.path().to_string(),
        params,
        query,
        raw_query: parts.uri.query().map(String::from),
        headers: if for_a_handler {
            header_map(&parts.headers)
        } else {
            Default::default()
        },
        // The checked body, as the text it arrived as.
        json: body_bytes
            .clone()
            .filter(|_| is_json && for_a_handler)
            .map(|text| match json_body {
                JsonBody::Unread => unsworn_proto::Json::unchecked(text),
                _ => unsworn_proto::Json::raw(text),
            }),
        ..Default::default()
    };
    Ok(Prepared {
        parts,
        template,
        raw_params,
        raw_body,
        body_bytes,
        streaming_body,
        stream_body: eff.stream_body && matches!(route.action, Action::Handler(_)),
        cache_key,
        context,
        claims,
    })
}

/// The `403` for a request that doesn't meet a `require` rule. It names
/// the rule, which is in the config, and neither of the values.
pub(super) fn not_allowed(rule: &crate::policy::Requirement) -> Problem {
    Problem::new(
        StatusCode::FORBIDDEN,
        format!(
            "`{}` and `{}` have to be the same for this request, and they aren't",
            rule.left.describe(),
            rule.right.describe()
        ),
    )
}

/// The answer to a request over its rate limit, if it is.
fn rate_limited<B>(outcome: RateLimit) -> Result<(), Response<B>> {
    match outcome {
        RateLimit::Allowed => Ok(()),
        RateLimit::Limited { retry_after } => {
            Err(problem_response(super::too_many_requests(retry_after)))
        }
        RateLimit::Unavailable => Err(problem_response(super::rate_limit_unavailable())),
    }
}

/// The answer from a cache entry: the stored response, or `304` when the
/// request's validators show the client already has it.
fn cached_response<H: Host>(
    ctx: &Ctx<'_, H>,
    hit: &CachedResponse,
    method: &str,
    request: &http::HeaderMap,
) -> Resp<H> {
    let stored = |name: &str| {
        let found = hit.headers.iter().find(|(k, _)| k == name);
        found.map(|(_, v)| v.as_str())
    };
    let asked = |name: http::HeaderName| request.get(name).and_then(|v| v.to_str().ok());
    let not_modified = rules::not_modified(
        asked(header::IF_NONE_MATCH),
        asked(header::IF_MODIFIED_SINCE),
        stored("etag"),
        stored("last-modified"),
    );
    let body = if not_modified || method == "HEAD" {
        Body::Empty
    } else {
        Body::Bytes(hit.body.clone())
    };
    let mut resp = http::Response::new(body);
    *resp.status_mut() = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::from_u16(hit.status).unwrap_or(StatusCode::OK)
    };
    for (k, v) in &hit.headers {
        // A `304` has no content for these to describe.
        if not_modified && matches!(k.as_str(), "content-length" | "content-type") {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::try_from(k.as_str()),
            HeaderValue::from_str(v),
        ) {
            resp.headers_mut().append(n, v);
        }
    }
    // Its age on arrival, plus its time here (RFC 9111 section 4.2.3).
    let upstream =
        rules::response_age(&|name| stored(name).map(String::from).into_iter().collect());
    let age = (upstream + hit.age(ctx.host.now_ms())).as_secs();
    resp.headers_mut()
        .insert(header::AGE, HeaderValue::from(age));
    if ctx.host.dev() {
        resp.headers_mut()
            .insert("x-unsworn-cache", HeaderValue::from_static("hit"));
    }
    resp
}

/// Runs `before` handlers. Returns the final context, or a response that
/// ends the request.
async fn run_befores<H: Host>(
    ctx: &Ctx<'_, H>,
    eff: &Effective,
    template: &ProtoRequest,
    raw_body: Option<Bytes>,
    mut context: Map<String, Value>,
    deadline: Deadline,
) -> Result<Map<String, Value>, Resp<H>> {
    for before in &eff.before {
        let request = for_handler(template, before, Phase::Before, &context);
        let body = Body::bytes(raw_body.clone());
        match ctx.host.call(before, request, body, deadline).await {
            Ok(Reply::Continue(more)) => {
                for (k, v) in more {
                    if k != "auth" {
                        context.insert(k, v);
                    }
                }
            }
            Ok(Reply::Response { response, body }) => return Err(to_http(response, body)),
            Err(e) => return Err(call_error(ctx.host.dev(), before, e)),
        }
    }
    Ok(context)
}

/// Runs the route's action.
async fn run_action<H: Host>(
    ctx: &Ctx<'_, H>,
    route: &Route,
    prepared: Prepared<H>,
    deadline: Deadline,
) -> Resp<H> {
    let Prepared {
        parts,
        template,
        raw_params,
        raw_body,
        body_bytes,
        streaming_body,
        stream_body,
        context,
        ..
    } = prepared;
    let method = template.method.clone();
    match &route.action {
        Action::Respond {
            status,
            body,
            headers,
        } => {
            let mut resp = match body {
                RespondBody::None => empty_response(StatusCode::OK),
                RespondBody::Json(j) => json_response(StatusCode::OK, j),
                RespondBody::Text(t) => {
                    let mut r = http::Response::new(Body::Bytes(Bytes::from(t.clone())));
                    r.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    r
                }
            };
            *resp.status_mut() = StatusCode::from_u16(*status).unwrap_or(StatusCode::OK);
            for (k, v) in headers {
                if let (Ok(n), Ok(v)) = (
                    http::HeaderName::try_from(k.as_str()),
                    HeaderValue::from_str(v),
                ) {
                    resp.headers_mut().insert(n, v);
                }
            }
            resp
        }
        Action::Redirect { to, status } => {
            let location = redirect_location(to, &raw_params);
            let mut resp = empty_response(
                StatusCode::from_u16(*status).unwrap_or(StatusCode::PERMANENT_REDIRECT),
            );
            if let Ok(v) = HeaderValue::from_str(&location) {
                resp.headers_mut().insert(header::LOCATION, v);
            }
            resp
        }
        Action::Static {
            dir,
            spa_fallback,
            index,
            cache_control,
        } => {
            let rest = route
                .catch_all
                .as_ref()
                .and_then(|name| raw_params.iter().find(|(k, _)| k == name))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            // Dotfiles in a served directory are rarely meant for visitors:
            // `.env`, `.git`, `.htpasswd`. `.well-known` is the exception.
            if super::checks::hidden_path(&rest) {
                return problem(StatusCode::NOT_FOUND, format!("/{rest} not found"));
            }
            ctx.host
                .serve_static(StaticRequest {
                    dir: dir.clone(),
                    rest,
                    index: index.clone(),
                    spa_fallback: spa_fallback.clone(),
                    cache_control: cache_control.clone(),
                    method,
                    headers: parts.headers,
                })
                .await
        }
        Action::Proxy(p) => {
            let body = match (body_bytes, streaming_body) {
                (Some(b), _) => Body::Bytes(b),
                (None, Some(stream)) => Body::Host(stream),
                (None, None) => Body::Empty,
            };
            // The app learns who the caller is, as a handler does.
            let mut parts = parts;
            super::proxy::forward_context(&mut parts.headers, &context, p);
            ctx.host
                .proxy(ProxyRequest {
                    proxy: p.clone(),
                    request: http::Request::from_parts(parts, body),
                    client_ip: ctx.client_ip,
                    context,
                    deadline,
                })
                .await
        }
        Action::Handler(h) => {
            // The last use of the request, so it moves instead of copying
            // its headers and JSON body again.
            let mut request = template;
            request.handler = h.raw.clone();
            request.phase = Phase::Handle;
            request.context = context;
            // A copy for the route's shadow handler, if this is a request
            // to copy. A streamed body can only be read once.
            let shadow = route
                .shadow
                .as_ref()
                .filter(|s| !stream_body && s.take())
                .map(|s| {
                    let mut copy = request.clone();
                    copy.handler = s.handler.raw.clone();
                    (s.handler.clone(), copy, raw_body.clone())
                });
            let began = ctx.host.monotonic();
            let body = match streaming_body {
                Some(stream) if stream_body => Body::Host(stream),
                _ => Body::bytes(raw_body),
            };
            match ctx.host.call(h, request, body, deadline).await {
                Ok(Reply::Response { response, body }) => {
                    if let Some((handler, request, raw_body)) = shadow {
                        ctx.host.shadow(super::shadow::ShadowCall {
                            route: route.path.clone(),
                            method: request.method.clone(),
                            handler,
                            request,
                            body: raw_body,
                            deadline,
                            primary: super::shadow::Answer::of(&response, &body),
                            primary_time: ctx.host.monotonic().saturating_sub(began),
                        });
                    }
                    let mode = ctx.project.server.response_validation;
                    let rule = route.validation.response.as_ref();
                    if mode != ResponseValidation::Off
                        && let (Some(rule), Some(json)) = (rule, &response.json)
                        && let Err(errors) = rule.check(&json.to_value(), "response")
                    {
                        tracing::warn!(route = %route.path, ?errors, "response doesn't match its schema");
                        if mode == ResponseValidation::Enforce {
                            return problem_with(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "the handler's response doesn't match its schema",
                                ctx.host.dev().then_some(errors.as_slice()),
                            );
                        }
                    }
                    to_http(response, body)
                }
                Ok(Reply::Continue(_)) => problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the handler sent `continue`, which only before handlers may send",
                ),
                Err(e) => call_error(ctx.host.dev(), h, e),
            }
        }
        // `serve_route` hands MCP requests to `mcp::serve` instead.
        Action::Mcp => problem(StatusCode::INTERNAL_SERVER_ERROR, "unexpected MCP request"),
    }
}

fn for_handler(
    template: &ProtoRequest,
    handler: &HandlerRef,
    phase: Phase,
    context: &Map<String, Value>,
) -> ProtoRequest {
    let mut r = template.clone();
    r.handler = handler.raw.clone();
    r.phase = phase;
    r.context = context.clone();
    r
}

/// Fills the cache from a response, if it may be stored.
async fn store_in_cache<H: Host>(
    ctx: &Ctx<'_, H>,
    key: String,
    rule: &CacheRule,
    resp: Resp<H>,
) -> Resp<H> {
    let ttl = rule.ttl;
    let status = resp.status().as_u16();
    let (storable, lifetime) = {
        let header = |name: &str| -> Vec<String> {
            resp.headers()
                .get_all(name)
                .iter()
                .filter_map(|v| v.to_str().ok().map(String::from))
                .collect()
        };
        (
            rules::cache_storable(status, &rule.vary, &header),
            rules::cache_lifetime(ttl, ctx.host.now_ms(), &header),
        )
    };
    // The response says how long it stays fresh; `ttl` is the fallback and
    // the ceiling. One that's stale on arrival isn't worth keeping.
    let Some(ttl) = lifetime else {
        return resp;
    };
    let small = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_none_or(|l| l <= MAX_CACHED_BODY);
    if !storable || !small {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let bytes = match ctx.collect(body, MAX_CACHED_BODY).await {
        Ok(bytes) => bytes,
        // Too large, or failed midway: sent as it is, without caching.
        Err(body) => return http::Response::from_parts(parts, body),
    };
    let entry = CachedResponse {
        status,
        headers: parts
            .headers
            .iter()
            .filter(|(k, _)| *k != header::DATE)
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect(),
        body: bytes.clone(),
        stored_ms: ctx.host.now_ms(),
        ttl,
    };
    let store = ctx.host.store();
    ctx.host
        .background(async move {
            let _ = store.put(&key, entry).await;
        })
        .await;
    let mut resp = http::Response::from_parts(parts, Body::Bytes(bytes));
    if ctx.host.dev() {
        resp.headers_mut()
            .insert("x-unsworn-cache", HeaderValue::from_static("miss"));
    }
    resp
}

fn call_error<B>(dev: bool, handler: &HandlerRef, e: CallError) -> Response<B> {
    use super::host::BodyError;
    if let CallError::BadJson(detail) = e {
        return problem(StatusCode::BAD_REQUEST, detail);
    }
    if matches!(e, CallError::Body(_) | CallError::RawBody) {
        tracing::info!(handler = %handler.raw, "{e}");
    } else {
        tracing::error!(handler = %handler.raw, "{e}");
    }
    let (status, public) = match &e {
        CallError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the handler's runtime is unavailable",
        ),
        CallError::MissingHandler(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the handler wasn't found",
        ),
        CallError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "the handler timed out"),
        CallError::WorkerFailed(_) => (StatusCode::BAD_GATEWAY, "the handler's worker failed"),
        CallError::Handler(_) => (StatusCode::INTERNAL_SERVER_ERROR, "the handler failed"),
        CallError::Body(BodyError::TooLarge) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "the body is larger than this route allows",
        ),
        CallError::Body(BodyError::Idle) => (
            StatusCode::REQUEST_TIMEOUT,
            "the request body stopped arriving",
        ),
        CallError::Body(BodyError::Read) => {
            (StatusCode::BAD_REQUEST, "couldn't read the request body")
        }
        // Answered above, with its own detail.
        CallError::BadJson(_) => (StatusCode::BAD_REQUEST, "the body isn't valid JSON"),
        CallError::RawBody => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the body must be JSON (Content-Type: application/json)",
        ),
    };
    if dev {
        problem(status, format!("{e} ({})", handler.raw))
    } else {
        problem(status, public)
    }
}

/// Converts a handler's response message into an HTTP response. An empty
/// body is filled from the message's `json` or `text`.
fn to_http<B>(response: unsworn_proto::Response, body: Body<B>) -> Response<B> {
    let (status, headers) = match response_head(&response) {
        Ok(head) => head,
        Err(p) => return problem_response(p),
    };
    let body = match (body, &response.json, &response.text) {
        (Body::Empty, Some(json), _) => Body::Bytes(json.to_bytes()),
        (Body::Empty, None, Some(text)) => Body::Bytes(Bytes::from(text.clone())),
        (body, _, _) => body,
    };
    let mut resp = http::Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}
