//! Handling one request, from matching to response.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use serde_json::{Map, Value, json};
use tokio_stream::wrappers::ReceiverStream;
use unsworn_core::MatchResult;
use unsworn_core::auth::{AuthFailure, CredentialSource, authenticate, www_authenticate};
use unsworn_core::config::{ResponseValidation, TrailingSlash};
use unsworn_core::policy::{AuthReq, Effective, HandlerRef, KeySource};
use unsworn_core::project::{Action, RespondBody, Route};
use unsworn_core::rules;
use unsworn_proto::{Phase, Request as ProtoRequest};

use super::proxy;
use super::support::{
    Body, CachedResponse, MAX_CACHED_BODY, empty, full, json_response, problem, problem_with,
};
use super::{Generation, Shared};
use crate::workers::{CallError, Reply, ReplyBody};

/// Per-request facts used across steps.
struct Ctx {
    shared: Arc<Shared>,
    generation: Arc<Generation>,
    request_id: String,
    client_ip: IpAddr,
    tls: bool,
    origin: Option<String>,
}

pub fn handle(
    shared: Arc<Shared>,
    req: Request<Incoming>,
    peer: SocketAddr,
    tls: bool,
) -> BoxFuture<'static, Response<Body>> {
    Box::pin(handle_inner(shared, req, peer, tls))
}

async fn handle_inner(
    shared: Arc<Shared>,
    req: Request<Incoming>,
    peer: SocketAddr,
    tls: bool,
) -> Response<Body> {
    let start = Instant::now();
    let generation = shared.generation.load_full();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 128)
        .map(String::from)
        .unwrap_or_else(|| shared.next_request_id());
    let ctx = Ctx {
        shared: shared.clone(),
        generation: generation.clone(),
        request_id: request_id.clone(),
        client_ip: generation.client_ip.resolve(peer, req.headers()),
        tls,
        origin: req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
    };

    let (mut resp, route_label) = route_request(&ctx, req).await;
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    let status = resp.status().as_u16();
    let elapsed = start.elapsed();
    shared
        .metrics
        .record(method.as_str(), &route_label, status, elapsed);
    tracing::info!(
        target: "access",
        method = %method,
        path = %path,
        status,
        route = %route_label,
        ms = (elapsed.as_secs_f64() * 1_000_000.0).round() / 1000.0,
        ip = %ctx.client_ip,
        id = %request_id,
    );
    resp
}

fn route_request(ctx: &Ctx, req: Request<Incoming>) -> BoxFuture<'_, (Response<Body>, String)> {
    Box::pin(async move {
        let project = &ctx.generation.project;
        let path = req.uri().path().to_string();

        if path == "/_unsworn/health" {
            return (health(ctx), "/_unsworn/health".into());
        }
        if let Some(doc) = ctx.generation.well_known.get(&path) {
            return (json_response(StatusCode::OK, doc), path);
        }

        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| req.uri().host())
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
            && let MatchResult::Found(m) = project.match_request(host.as_deref(), want, &path)
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
                let mut resp = Response::new(empty());
                *resp.status_mut() = StatusCode::NO_CONTENT;
                if let Some(headers) =
                    rules::cors_preflight_headers(cors, origin, requested, &methods)
                {
                    for (k, v) in headers {
                        append(resp.headers_mut(), &k, &v);
                    }
                }
                return (resp, m.route.path.clone());
            }
        }

        let mut matched = project.match_request(host.as_deref(), &method, &path);
        if matches!(matched, MatchResult::NotFound)
            && project.server.trailing_slash != TrailingSlash::Strict
            && path != "/"
        {
            let toggled = match path.strip_suffix('/') {
                Some(p) => p.to_string(),
                None => format!("{path}/"),
            };
            let alt = project.match_request(host.as_deref(), &method, &toggled);
            if let MatchResult::Found(_) = alt {
                if project.server.trailing_slash == TrailingSlash::Redirect {
                    let location = match req.uri().query() {
                        Some(q) => format!("{toggled}?{q}"),
                        None => toggled,
                    };
                    let mut resp = Response::new(empty());
                    *resp.status_mut() = StatusCode::PERMANENT_REDIRECT;
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
            MatchResult::MethodNotAllowed(allowed) => {
                let mut resp = problem(
                    StatusCode::METHOD_NOT_ALLOWED,
                    format!("{path} doesn't accept {method}"),
                );
                if let Ok(v) = HeaderValue::from_str(&allowed.join(", ")) {
                    resp.headers_mut().insert(header::ALLOW, v);
                }
                (resp, "(none)".into())
            }
            MatchResult::Found(m) => {
                let route = m.route;
                let label = route.path.clone();
                let eff = project.effective(route, &method, &path);
                let params: Vec<(String, String)> =
                    m.params.into_iter().map(|(k, v)| (k, decode(&v))).collect();
                let timeout = eff.timeout;
                let work = serve_route(ctx, req, route, &eff, params);
                let mut resp = match tokio::time::timeout(timeout, work).await {
                    Ok(r) => r,
                    Err(_) => problem(
                        StatusCode::GATEWAY_TIMEOUT,
                        format!("the request took longer than {}s", timeout.as_secs_f64()),
                    ),
                };
                finish(ctx, &eff, &mut resp);
                (resp, label)
            }
        }
    })
}

/// Header rules and CORS headers, applied to every routed response.
fn finish(ctx: &Ctx, eff: &Effective, resp: &mut Response<Body>) {
    let headers = resp.headers_mut();
    for name in &eff.headers.remove {
        headers.remove(name.as_str());
    }
    for (name, value) in &eff.headers.set {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(n, v);
        }
    }
    if let (Some(cors), Some(origin)) = (&eff.cors, &ctx.origin) {
        for (k, v) in rules::cors_response_headers(cors, origin) {
            if k == "vary" {
                append(headers, &k, &v);
            } else if let (Ok(n), Ok(v)) =
                (HeaderName::try_from(k.as_str()), HeaderValue::from_str(&v))
            {
                headers.insert(n, v);
            }
        }
    }
}

fn append(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        headers.append(n, v);
    }
}

struct KeyLookup<'a>(&'a super::support::KeyCache);

impl KeyLookup<'_> {
    fn get(&self, name: &str) -> Option<Arc<unsworn_core::auth::KeySet>> {
        self.0.get(name)
    }
}

struct Creds<'a> {
    headers: &'a HeaderMap,
    query: &'a [(String, String)],
}

impl CredentialSource for Creds<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the checks before the action produce.
struct Prepared {
    parts: http::request::Parts,
    template: ProtoRequest,
    raw_params: Vec<(String, String)>,
    raw_body: Option<Bytes>,
    body_bytes: Option<Bytes>,
    streaming_body: Option<Body>,
    cache_key: Option<String>,
    context: Map<String, Value>,
}

fn serve_route<'a>(
    ctx: &'a Ctx,
    req: Request<Incoming>,
    route: &'a Route,
    eff: &'a Effective,
    raw_params: Vec<(String, String)>,
) -> BoxFuture<'a, Response<Body>> {
    Box::pin(async move {
        let mut prepared = match prepare(ctx, req, route, eff, raw_params).await {
            Ok(p) => p,
            Err(resp) => return resp,
        };
        let deadline = Instant::now() + eff.timeout;
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
        let cache_key = prepared.cache_key.take();
        let resp = run_action(ctx, route, prepared, deadline).await;
        if let (Some(key), Some(rule)) = (cache_key, eff.cache.as_ref()) {
            let shared = ctx.shared.clone();
            return store_in_cache(shared, key, rule.ttl, resp).await;
        }
        resp
    })
}

/// Origin, auth, rate limit, validation, cache lookup and body reading.
fn prepare<'a>(
    ctx: &'a Ctx,
    req: Request<Incoming>,
    route: &'a Route,
    eff: &'a Effective,
    raw_params: Vec<(String, String)>,
) -> BoxFuture<'a, Result<Prepared, Response<Body>>> {
    Box::pin(async move {
        let method = req.method().as_str().to_string();
        let query_pairs: Vec<(String, String)> = req
            .uri()
            .query()
            .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
            .unwrap_or_default();

        if let (Some(allow), Some(origin)) = (&eff.origin, &ctx.origin)
            && !allow.iter().any(|a| a == origin)
        {
            return Err(problem(
                StatusCode::FORBIDDEN,
                format!("requests from origin {origin} aren't allowed"),
            ));
        }

        if let Some(rules) = &eff.ip
            && !ip_allowed(rules, ctx.client_ip)
        {
            return Err(problem(
                StatusCode::FORBIDDEN,
                "requests from this address aren't allowed",
            ));
        }

        let (context, claims) = authenticate_request(ctx, &req, eff, &query_pairs).await?;

        if let Some(limit) = &eff.rate_limit {
            let ip = ctx.client_ip;
            let key = key_value(&limit.key, ip, req.headers(), claims.as_ref())
                .unwrap_or_else(|| ip.to_string());
            let bucket = format!("{}|{}|{key}", route.id, limit.key.describe());
            if let Err(wait) = ctx
                .shared
                .store
                .rate_limit(&bucket, limit.requests, limit.per)
            {
                let mut resp = problem(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
                let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
                resp.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(secs));
                return Err(resp);
            }
        }

        let (params, query) = validate_inputs(route, &raw_params, &query_pairs, req.headers())?;

        let cache_key = match &eff.cache {
            Some(rule) => compute_cache_key(
                rule,
                route.id,
                &method,
                req.uri(),
                req.headers(),
                claims.as_ref(),
                ctx.client_ip,
            ),
            None => None,
        };
        if let Some(key) = &cache_key
            && let Some(hit) = ctx.shared.store.cache_get(key)
        {
            return Err(cached_response(&hit, &method, ctx.shared.dev));
        }

        let needs_body =
            matches!(route.action, Action::Handler(_)) || route.validation.body.is_some();
        let (parts, incoming) = req.into_parts();
        if let Some(len) = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            && len > eff.max_body
        {
            return Err(problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the body is larger than {} bytes", eff.max_body),
            ));
        }
        let mut body_bytes: Option<Bytes> = None;
        let mut streaming_body: Option<Body> = None;
        if needs_body {
            body_bytes = Some(read_body(incoming, eff.max_body).await?);
        } else {
            streaming_body = Some(Limited::new(incoming, eff.max_body as usize).boxed_unsync());
        }

        let json_body = check_body(route, &parts.headers, body_bytes.as_ref())?;
        let raw_body = if json_body.is_some() {
            None
        } else {
            body_bytes.clone().filter(|b| !b.is_empty())
        };
        let template = ProtoRequest {
            id: String::new(),
            phase: Phase::Handle,
            handler: String::new(),
            route: route.path.clone(),
            method,
            path: parts.uri.path().to_string(),
            params,
            query,
            raw_query: parts.uri.query().map(String::from),
            headers: header_map(&parts.headers),
            context: Map::new(),
            json: json_body,
            ..Default::default()
        };
        Ok(Prepared {
            parts,
            template,
            raw_params,
            raw_body,
            body_bytes,
            streaming_body,
            cache_key,
            context,
        })
    })
}

type AuthOutcome = (Map<String, Value>, Option<Map<String, Value>>);

fn authenticate_request<'a>(
    ctx: &'a Ctx,
    req: &'a Request<Incoming>,
    eff: &'a Effective,
    query_pairs: &'a [(String, String)],
) -> BoxFuture<'a, Result<AuthOutcome, Response<Body>>> {
    Box::pin(async move {
        let mut context = Map::new();
        let AuthReq::AnyOf(alts) = &eff.auth else {
            return Ok((context, None));
        };
        let kid = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' ').map(|(_, t)| t.to_string()))
            .and_then(|t| jsonwebtoken_kid(&t));
        let mut schemes = Vec::new();
        for alt in alts {
            if let Some(s) = ctx.generation.project.schemes.get(&alt.scheme) {
                schemes.push(s.clone());
            }
        }
        for scheme in schemes {
            ctx.shared
                .keys
                .ensure(ctx.shared.client.clone(), scheme, kid.clone())
                .await;
        }
        let creds = Creds {
            headers: req.headers(),
            query: query_pairs,
        };
        let keys = KeyLookup(&ctx.shared.keys);
        let lookup = |n: &str| keys.get(n);
        let outcome = authenticate(
            &eff.auth,
            &ctx.generation.project.schemes,
            &lookup,
            &creds,
            now_secs(),
        );
        match outcome {
            Ok(Some(ok)) => {
                let mut auth = ok.claims.clone();
                auth.insert("scheme".into(), json!(ok.scheme));
                context.insert("auth".into(), Value::Object(auth));
                Ok((context, Some(ok.claims)))
            }
            Ok(None) => Ok((context, None)),
            Err(failure) => Err(auth_failure(ctx, &failure)),
        }
    })
}

type Inputs = (Map<String, Value>, Map<String, Value>);

fn validate_inputs(
    route: &Route,
    raw_params: &[(String, String)],
    query_pairs: &[(String, String)],
    headers: &HeaderMap,
) -> Result<Inputs, Response<Body>> {
    let mut params = Map::new();
    match &route.validation.params {
        Some(rules) => match rules.apply(raw_params) {
            Ok(p) => params = p,
            Err(errors) => {
                return Err(problem_with(
                    StatusCode::BAD_REQUEST,
                    "invalid path parameters",
                    Some(&errors),
                ));
            }
        },
        None => {
            for (k, v) in raw_params {
                params.insert(k.clone(), Value::String(v.clone()));
            }
        }
    }
    let mut query = Map::new();
    match &route.validation.query {
        Some(rules) => match rules.apply(query_pairs) {
            Ok(q) => query = q,
            Err(errors) => {
                return Err(problem_with(
                    StatusCode::BAD_REQUEST,
                    "invalid query parameters",
                    Some(&errors),
                ));
            }
        },
        None => {
            for (k, v) in query_pairs {
                query.entry(k.clone()).or_insert(Value::String(v.clone()));
            }
        }
    }
    if let Some(rules) = &route.validation.headers {
        let pairs: Vec<(String, String)> = headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_string(), v.to_string()))
            })
            .collect();
        if let Err(errors) = rules.apply(&pairs) {
            return Err(problem_with(
                StatusCode::BAD_REQUEST,
                "invalid headers",
                Some(&errors),
            ));
        }
    }
    Ok((params, query))
}

fn cached_response(hit: &CachedResponse, method: &str, dev: bool) -> Response<Body> {
    let mut resp = Response::new(if method == "HEAD" {
        empty()
    } else {
        full(hit.body.clone())
    });
    *resp.status_mut() = StatusCode::from_u16(hit.status).unwrap_or(StatusCode::OK);
    for (k, v) in &hit.headers {
        append_raw(resp.headers_mut(), k, v.clone());
    }
    resp.headers_mut().insert(
        header::AGE,
        HeaderValue::from(hit.stored.elapsed().as_secs()),
    );
    if dev {
        resp.headers_mut()
            .insert("x-unsworn-cache", HeaderValue::from_static("hit"));
    }
    resp
}

async fn read_body(incoming: Incoming, max: u64) -> Result<Bytes, Response<Body>> {
    match Limited::new(incoming, max as usize).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) => Err(if e.is::<http_body_util::LengthLimitError>() {
            problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the body is larger than {max} bytes"),
            )
        } else {
            problem(StatusCode::BAD_REQUEST, "couldn't read the request body")
        }),
    }
}

/// Parses a JSON body and validates it against the route's body schema.
fn check_body(
    route: &Route,
    headers: &HeaderMap,
    body: Option<&Bytes>,
) -> Result<Option<Value>, Response<Body>> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_json = content_type.starts_with("application/json") || content_type.contains("+json");
    let empty_body = body.is_none_or(|b| b.is_empty());
    let mut json_body = None;
    if let Some(bytes) = body
        && !empty_body
        && is_json
    {
        match serde_json::from_slice::<Value>(bytes) {
            Ok(v) => json_body = Some(v),
            Err(e) => {
                return Err(problem(
                    StatusCode::BAD_REQUEST,
                    format!("the body isn't valid JSON: {e}"),
                ));
            }
        }
    }
    if let Some(rule) = &route.validation.body {
        if empty_body {
            if rule.required {
                return Err(problem(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "a JSON body is required",
                ));
            }
        } else if !is_json {
            return Err(problem(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "the body must be JSON (Content-Type: application/json)",
            ));
        } else if let Some(v) = &json_body
            && let Err(errors) = rule.check(v, "body")
        {
            return Err(problem_with(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid body",
                Some(&errors),
            ));
        }
    }
    Ok(json_body)
}

/// Runs `before` handlers. Returns the final context, or a response that
/// ends the request.
fn run_befores<'a>(
    ctx: &'a Ctx,
    eff: &'a Effective,
    template: &'a ProtoRequest,
    raw_body: Option<Bytes>,
    mut context: Map<String, Value>,
    deadline: Instant,
) -> BoxFuture<'a, Result<Map<String, Value>, Response<Body>>> {
    Box::pin(async move {
        for before in &eff.before {
            let Some(pool) = ctx.generation.pools.get(&before.runtime).cloned() else {
                return Err(problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "runtime not found",
                ));
            };
            let request = for_handler(template, before, Phase::Before, &context);
            match pool.call(request, raw_body.clone(), deadline).await {
                Ok(Reply::Continue(more)) => {
                    for (k, v) in more {
                        if k != "auth" {
                            context.insert(k, v);
                        }
                    }
                }
                Ok(Reply::Response { response, body }) => return Err(to_http(response, body)),
                Err(e) => return Err(call_error(ctx, before, e)),
            }
        }
        Ok(context)
    })
}

/// Runs the route's action.
fn run_action<'a>(
    ctx: &'a Ctx,
    route: &'a Route,
    prepared: Prepared,
    deadline: Instant,
) -> BoxFuture<'a, Response<Body>> {
    Box::pin(async move {
        let Prepared {
            parts,
            template,
            raw_params,
            raw_body,
            body_bytes,
            streaming_body,
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
                    RespondBody::None => Response::new(empty()),
                    RespondBody::Json(j) => json_response(StatusCode::OK, j),
                    RespondBody::Text(t) => {
                        let mut r = Response::new(full(t.clone()));
                        r.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/plain; charset=utf-8"),
                        );
                        r
                    }
                };
                *resp.status_mut() = StatusCode::from_u16(*status).unwrap_or(StatusCode::OK);
                for (k, v) in headers {
                    if let (Ok(n), Ok(v)) =
                        (HeaderName::try_from(k.as_str()), HeaderValue::from_str(v))
                    {
                        resp.headers_mut().insert(n, v);
                    }
                }
                resp
            }
            Action::Redirect { to, status } => {
                let mut location = to.clone();
                for (k, v) in &raw_params {
                    location = location
                        .replace(&format!("{{*{k}}}"), v)
                        .replace(&format!("{{{k}}}"), v);
                }
                let mut resp = Response::new(empty());
                *resp.status_mut() =
                    StatusCode::from_u16(*status).unwrap_or(StatusCode::PERMANENT_REDIRECT);
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
                super::static_files::serve(super::static_files::StaticRequest {
                    base: ctx.shared.root.join(dir),
                    rest,
                    index: index.clone(),
                    spa_fallback: spa_fallback.clone(),
                    cache_control: cache_control.clone(),
                    method,
                    headers: parts.headers.clone(),
                })
                .await
            }
            Action::Proxy(p) => {
                let body = match body_bytes {
                    Some(b) => full(b),
                    None => streaming_body.unwrap_or_else(empty),
                };
                let req = Request::from_parts(parts, body);
                proxy::forward(
                    ctx.shared.client.clone(),
                    p.clone(),
                    req,
                    proxy::ForwardInfo {
                        client_ip: ctx.client_ip,
                        tls: ctx.tls,
                        request_id: ctx.request_id.clone(),
                    },
                )
                .await
            }
            Action::Handler(h) => {
                let Some(pool) = ctx.generation.pools.get(&h.runtime).cloned() else {
                    return problem(StatusCode::INTERNAL_SERVER_ERROR, "runtime not found");
                };
                let request = for_handler(&template, h, Phase::Handle, &context);
                let result = pool.call(request, raw_body, deadline).await;
                match result {
                    Ok(Reply::Response { response, body }) => {
                        let mode = ctx.generation.project.server.response_validation;
                        let rule = route.validation.response.as_ref();
                        if mode != ResponseValidation::Off
                            && let (Some(rule), Some(json)) = (rule, &response.json)
                            && let Err(errors) = rule.check(json, "response")
                        {
                            tracing::warn!(route = %route.path, ?errors, "response doesn't match its schema");
                            if mode == ResponseValidation::Enforce {
                                return problem_with(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "the handler's response doesn't match its schema",
                                    ctx.shared.dev.then_some(errors.as_slice()),
                                );
                            }
                        }
                        to_http(response, body)
                    }
                    Ok(Reply::Continue(_)) => problem(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "the handler sent `continue`, which only before handlers may send",
                    ),
                    Err(e) => call_error(ctx, h, e),
                }
            }
        }
    })
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

fn chunk_to_frame(item: Result<Bytes, String>) -> Result<Frame<Bytes>, super::support::BoxError> {
    match item {
        Ok(bytes) => Ok(Frame::data(bytes)),
        Err(e) => Err(e.into()),
    }
}

async fn store_in_cache(
    shared: Arc<Shared>,
    key: String,
    ttl: Duration,
    resp: Response<Body>,
) -> Response<Body> {
    let status = resp.status().as_u16();
    let storable = rules::cache_storable(status, &|name| {
        resp.headers()
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok().map(String::from))
            .collect()
    });
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
    let (bytes, complete) = collect_up_to(body, MAX_CACHED_BODY).await;
    if !complete {
        // Too large or failed midway: serve what we have without caching.
        return Response::from_parts(parts, full(bytes));
    }
    shared.store.cache_put(
        key,
        CachedResponse {
            status,
            headers: parts
                .headers
                .iter()
                .filter(|(k, _)| *k != header::DATE)
                .map(|(k, v)| (k.as_str().to_string(), v.clone()))
                .collect(),
            body: bytes.clone(),
            stored: Instant::now(),
            ttl,
        },
    );
    let mut resp = Response::from_parts(parts, full(bytes));
    if shared.dev {
        resp.headers_mut()
            .insert("x-unsworn-cache", HeaderValue::from_static("miss"));
    }
    resp
}

/// Reads a body up to `limit` bytes. Returns the bytes and whether the
/// whole body was read.
async fn collect_up_to(mut body: Body, limit: usize) -> (Bytes, bool) {
    let mut out = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return (out.freeze(), false);
        };
        if let Ok(data) = frame.into_data() {
            if out.len() + data.len() > limit {
                return (out.freeze(), false);
            }
            out.extend_from_slice(&data);
        }
    }
    (out.freeze(), true)
}

fn append_raw(headers: &mut HeaderMap, name: &str, value: HeaderValue) {
    if let Ok(n) = HeaderName::try_from(name) {
        headers.append(n, value);
    }
}

fn header_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in headers {
        let Ok(v) = v.to_str() else { continue };
        let sep = if k == header::COOKIE { "; " } else { ", " };
        out.entry(k.as_str().to_string())
            .and_modify(|e| {
                e.push_str(sep);
                e.push_str(v);
            })
            .or_insert_with(|| v.to_string());
    }
    out
}

fn ip_matches(entry: &str, ip: IpAddr) -> bool {
    if let Ok(net) = entry.parse::<ipnet::IpNet>() {
        return net.contains(&ip);
    }
    entry.parse::<IpAddr>().is_ok_and(|e| e == ip)
}

fn ip_allowed(rules: &unsworn_core::policy::IpRules, ip: IpAddr) -> bool {
    if rules.deny.iter().any(|d| ip_matches(d, ip)) {
        return false;
    }
    rules.allow.is_empty() || rules.allow.iter().any(|a| ip_matches(a, ip))
}

fn compute_cache_key(
    rule: &unsworn_core::policy::CacheRule,
    route_id: usize,
    method: &str,
    uri: &http::Uri,
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
    client_ip: IpAddr,
) -> Option<String> {
    if !rules::cache_method(method) {
        return None;
    }
    if claims.is_some() && !rules::cache_separates_users(rule) {
        return None;
    }
    let mut vary = Vec::new();
    for h in &rule.vary {
        let value = headers
            .get(h.as_str())
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        vary.push((h.clone(), value));
    }
    let key = rule
        .key
        .as_ref()
        .map(|k| key_value(k, client_ip, headers, claims).unwrap_or_default());
    Some(rules::cache_key(
        route_id,
        method,
        uri.path(),
        uri.query(),
        &vary,
        key.as_deref(),
    ))
}

fn key_value(
    key: &KeySource,
    client_ip: IpAddr,
    headers: &HeaderMap,
    claims: Option<&Map<String, Value>>,
) -> Option<String> {
    match key {
        KeySource::Ip => Some(client_ip.to_string()),
        KeySource::Header(h) => headers
            .get(h.as_str())
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        KeySource::Claim(c) => claims.and_then(|m| m.get(c)).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
    }
}

fn jsonwebtoken_kid(token: &str) -> Option<String> {
    use base64_lite::decode_segment;
    let header = token.split('.').next()?;
    let bytes = decode_segment(header)?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("kid").and_then(Value::as_str).map(String::from)
}

/// Minimal base64url decoding for reading a JWT header's `kid`.
mod base64_lite {
    pub fn decode_segment(s: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len() * 3 / 4);
        let mut buf = 0u32;
        let mut bits = 0;
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'-' | b'+' => 62,
                b'_' | b'/' => 63,
                b'=' => break,
                _ => return None,
            } as u32;
            buf = (buf << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
                buf &= (1 << bits) - 1;
            }
        }
        Some(out)
    }
}

fn auth_failure(ctx: &Ctx, failure: &AuthFailure) -> Response<Body> {
    let (status, detail) = match failure {
        AuthFailure::Unauthorized { reason, .. } => (StatusCode::UNAUTHORIZED, reason.clone()),
        AuthFailure::InsufficientScope { scopes, .. } => (
            StatusCode::FORBIDDEN,
            format!("this requires the scopes: {}", scopes.join(" ")),
        ),
    };
    let mut resp = problem(status, detail);
    if let Some(h) = www_authenticate(failure, &ctx.generation.project.schemes)
        && let Ok(v) = HeaderValue::from_str(&h)
    {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

fn call_error(ctx: &Ctx, handler: &HandlerRef, e: CallError) -> Response<Body> {
    tracing::error!(handler = %handler.raw, "{e}");
    let dev = ctx.shared.dev;
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
    };
    if dev {
        problem(status, format!("{e} ({})", handler.raw))
    } else {
        problem(status, public)
    }
}

/// Converts a handler's response message into an HTTP response.
fn to_http(response: unsworn_proto::Response, body: ReplyBody) -> Response<Body> {
    let status = match StatusCode::from_u16(response.status) {
        Ok(s) => s,
        Err(_) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the handler returned an invalid status {}", response.status),
            );
        }
    };
    let http_body = match body {
        ReplyBody::Empty => empty(),
        ReplyBody::Bytes(b) => full(b),
        ReplyBody::Stream(rx) => {
            StreamBody::new(ReceiverStream::new(rx).map(chunk_to_frame)).boxed_unsync()
        }
    };
    let mut resp = Response::new(http_body);
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    for (name, values) in &response.headers {
        let Ok(n) = HeaderName::try_from(name.as_str()) else {
            tracing::warn!(header = %name, "handler returned an invalid header name");
            continue;
        };
        for v in values.iter() {
            if let Ok(v) = HeaderValue::from_str(v) {
                headers.append(n.clone(), v);
            }
        }
    }
    if !headers.contains_key(header::CONTENT_TYPE) {
        let ct = if response.json.is_some() {
            Some("application/json")
        } else if response.text.is_some() {
            Some("text/plain; charset=utf-8")
        } else if response.stream {
            None
        } else if response.body_len.is_some() {
            Some("application/octet-stream")
        } else {
            None
        };
        if let Some(ct) = ct {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
        }
    }
    resp
}

fn health(ctx: &Ctx) -> Response<Body> {
    let runtimes = ctx.generation.pools.health();
    let degraded = runtimes
        .iter()
        .any(|(_, started, alive, crashes)| (*started && *alive == 0) || *crashes >= 5);
    let body = json!({
        "status": if degraded { "degraded" } else { "ok" },
        "version": unsworn_core::VERSION,
        "runtimes": runtimes.iter().map(|(name, started, alive, crashes)| json!({
            "name": name,
            "started": started,
            "workers": alive,
            "recent_crashes": crashes,
        })).collect::<Vec<_>>(),
    });
    json_response(
        if degraded {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        },
        &body,
    )
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}
