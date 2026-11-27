//! What the module exports to JavaScript.

use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use std::collections::BTreeMap;

use bytes::Bytes;
use http::{HeaderValue, Method, StatusCode, Uri, header};
use js_sys::{Object, Promise};
use serde_json::Value;
use unsworn_core::calls::{self, Failure};
use unsworn_core::pipeline::{
    Body, Deadline, FetchError, Host, KeyCache, Problem, Reply, Router, problem_response,
};
use unsworn_core::store::{Bucket, take_token as core_take_token};
use unsworn_core::validate::Build;
use unsworn_core::{Level, Project, compile_with, outbound};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::bridge::{Bridge, JsBody, bytes_to_js, get, headers_from_js, response_to_js, set};
use crate::files::Files;
use crate::host::{Caller, WasmHost, WasmStore};
use crate::log;

/// A loaded project, ready to handle requests. Build one per isolate.
#[wasm_bindgen]
pub struct Unsworn {
    router: Rc<Router>,
    dev: bool,
}

fn string_entries(value: &JsValue) -> Vec<(String, Option<String>)> {
    let Some(obj) = value.dyn_ref::<Object>() else {
        return Vec::new();
    };
    Object::entries(obj)
        .iter()
        .filter_map(|pair| {
            let key = get(&pair, "0").as_string()?;
            Some((key, get(&pair, "1").as_string()))
        })
        .collect()
}

#[wasm_bindgen]
impl Unsworn {
    /// Loads a project. `files` maps project-relative paths to their text,
    /// or `null` for files that only need to exist (handler modules).
    /// `env` holds the variables `{env.NAME}` reads; values that aren't
    /// strings are ignored. Throws with the diagnostics when the project
    /// has errors, and logs its warnings.
    #[wasm_bindgen(constructor)]
    pub fn new(
        config_file: &str,
        files: &JsValue,
        env: &JsValue,
        dev: bool,
    ) -> Result<Unsworn, JsError> {
        log::init(dev);
        let files = Files {
            files: string_entries(files).into_iter().collect(),
            env: string_entries(env)
                .into_iter()
                .filter_map(|(k, v)| Some((k, v?)))
                .collect(),
        };
        // `unsworn build` compiled this config in full, so each schema's
        // validator can wait for the first request that needs it. Building
        // them all here would add milliseconds per route to every cold
        // start.
        let compiled = compile_with(&files, config_file, Build::OnFirstUse);
        let errors: Vec<String> = compiled
            .diagnostics
            .iter()
            .filter(|d| d.level == Level::Error)
            .map(|d| d.to_string())
            .collect();
        for d in compiled
            .diagnostics
            .iter()
            .filter(|d| d.level != Level::Error)
        {
            tracing::warn!("{d}");
        }
        let project = match compiled.project {
            Some(p) if errors.is_empty() => p,
            _ => {
                return Err(JsError::new(&format!(
                    "the project has errors:\n{}",
                    errors.join("\n")
                )));
            }
        };
        Ok(Unsworn {
            router: Rc::new(Router::new(
                Arc::new(project),
                Arc::new(KeyCache::default()),
            )),
            dev,
        })
    }

    /// Every handler reference the project calls, `file#name` as written.
    #[wasm_bindgen(getter)]
    pub fn handlers(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .router
            .project()
            .handlers_by_runtime()
            .into_values()
            .flatten()
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Where `[store]` says rate limits and cached responses live, for
    /// hosts that follow it: `{ backend, url, token, prefix }`. A host with
    /// a store of its own, such as Cloudflare, ignores it.
    #[wasm_bindgen(getter, unchecked_return_type = "StoreSettings")]
    pub fn store(&self) -> Object {
        let config = &self.router.project().store;
        let out = Object::new();
        let backend = match config.backend {
            unsworn_core::config::StoreBackend::Memory => "memory",
            unsworn_core::config::StoreBackend::Redis => "redis",
        };
        let text = |v: &Option<String>| v.as_deref().map_or(JsValue::NULL, JsValue::from_str);
        set(&out, "backend", &JsValue::from_str(backend));
        set(&out, "url", &text(&config.url));
        set(&out, "token", &text(&config.token));
        set(&out, "prefix", &JsValue::from_str(&config.prefix));
        out
    }

    /// Handles a request (`IncomingRequest`), resolving a
    /// `HandledResponse`.
    pub fn handle(&self, request: JsValue, bridge: Bridge) -> Promise {
        let router = self.router.clone();
        let dev = self.dev;
        future_to_promise(async move {
            let request_id = get(&request, "requestId").as_string().unwrap_or_default();
            let (req, client_ip, tls) = match request_from_js(&request) {
                Ok(r) => r,
                Err(detail) => {
                    let resp = problem_response(Problem::new(StatusCode::BAD_REQUEST, detail));
                    let out = response_to_js(resp);
                    set(&out, "route", &JsValue::from_str("(none)"));
                    return Ok(out.into());
                }
            };
            let host = host(router.project(), bridge, dev, request_id.clone(), tls);
            let mut handled = router.handle(&host, req, client_ip).await;
            if let Ok(v) = HeaderValue::from_str(&request_id) {
                handled.response.headers_mut().insert("x-request-id", v);
            }
            let out = response_to_js(handled.response);
            set(&out, "route", &JsValue::from_str(&handled.route));
            Ok(out.into())
        })
    }
}

#[wasm_bindgen]
impl Unsworn {
    /// Runs a function for a handler: its `ctx.call`. `caller` is what the
    /// bridge's `call` was given for the handler, and `input` is JSON
    /// text. `traceparent` and `tracestate` are the handler's own, when it
    /// names a span for the function to be called under. Resolves a
    /// `FunctionResult`, and never rejects.
    #[wasm_bindgen(js_name = callFunction, unchecked_return_type = "Promise<FunctionResult>")]
    pub fn call_function(
        &self,
        caller: &str,
        name: &str,
        input: &str,
        bridge: Bridge,
        traceparent: Option<String>,
        tracestate: Option<String>,
    ) -> Promise {
        let project = self.router.project().clone();
        let dev = self.dev;
        let own = calls::given_trace(traceparent.as_deref(), tracestate.as_deref());
        let caller = Caller::decode(caller).map(|mut caller| {
            caller.call = caller.call.with_trace(own);
            caller
        });
        let (name, input) = (name.to_string(), serde_json::from_str::<Value>(input));
        future_to_promise(async move {
            let outcome = match (caller, input) {
                (Some(caller), Ok(input)) => {
                    run_function(&project, caller, &name, &input, bridge, dev).await
                }
                (None, _) => Err(no_caller()),
                (_, Err(e)) => Err(Failure::new(400, format!("the input isn't JSON: {e}"))),
            };
            let out = Object::new();
            let (status, json, error) = match outcome {
                Ok((status, json)) => (status, json, None),
                Err(failure) => (failure.status, failure.json, Some(failure.error)),
            };
            set(&out, "status", &JsValue::from(status));
            if !json.is_null()
                && let Ok(value) = js_sys::JSON::parse(&json.to_string())
            {
                set(&out, "json", &value);
            }
            if let Some(error) = error {
                set(&out, "error", &JsValue::from_str(&error));
            }
            Ok(out.into())
        })
    }

    /// Makes a handler's request to an outbound service: its `ctx.fetch`.
    /// `headers` is an object of the handler's headers, and `body` a
    /// `Uint8Array` or `null`. Resolves a `ServiceResponse`, and never
    /// rejects.
    #[wasm_bindgen(js_name = fetchService, unchecked_return_type = "Promise<ServiceResponse>")]
    pub fn fetch_service(
        &self,
        caller: &str,
        service: &str,
        method: &str,
        path: &str,
        headers: &JsValue,
        body: JsValue,
        bridge: Bridge,
    ) -> Promise {
        let project = self.router.project().clone();
        let dev = self.dev;
        let caller = Caller::decode(caller);
        let (service, method, path) = (service.to_string(), method.to_string(), path.to_string());
        let sent: BTreeMap<String, String> = string_entries(headers)
            .into_iter()
            .filter_map(|(name, value)| Some((name, value?)))
            .collect();
        let body = body
            .dyn_ref::<js_sys::Uint8Array>()
            .map(|bytes| Bytes::from(bytes.to_vec()))
            .unwrap_or_default();
        future_to_promise(async move {
            let asked = Asked {
                service: &service,
                method: &method,
                path: &path,
                headers: &sent,
                body,
            };
            let outcome = match caller {
                Some(caller) => fetch_service(&project, caller, asked, bridge, dev).await,
                None => {
                    let failure = no_caller();
                    Err((failure.status, failure.error))
                }
            };
            let out = Object::new();
            let headers = Object::new();
            match outcome {
                Ok(response) => {
                    set(&out, "status", &JsValue::from(response.status().as_u16()));
                    for (name, value) in outbound::response_headers(response.headers()) {
                        // `fetch` decoded the body, so these no longer
                        // describe it.
                        if name != "content-encoding" && name != "content-length" {
                            set(&headers, &name, &JsValue::from_str(&value));
                        }
                    }
                    let body = response.into_body();
                    let body = match body.is_empty() {
                        true => JsValue::NULL,
                        false => bytes_to_js(&body).into(),
                    };
                    set(&out, "body", &body);
                }
                Err((status, error)) => {
                    set(&out, "status", &JsValue::from(status));
                    set(&out, "body", &JsValue::NULL);
                    set(&out, "error", &JsValue::from_str(&error));
                }
            }
            set(&out, "headers", &headers);
            Ok(out.into())
        })
    }
}

fn host(project: &Project, bridge: Bridge, dev: bool, request_id: String, tls: bool) -> WasmHost {
    WasmHost {
        store: WasmStore {
            bridge: bridge.clone(),
        },
        bridge,
        dev,
        request_id,
        tls,
        calls: !project.functions.is_empty() || !project.outbound.is_empty(),
    }
}

/// A `ctx.call` or `ctx.fetch` that came without the record of the call
/// it was made from.
fn no_caller() -> Failure {
    Failure::new(
        409,
        "a call is made for a request in flight, and this one has no record of its request",
    )
}

/// Runs a function as the server does (see `unsworn_core::calls`), with
/// the function's handler called in this isolate.
async fn run_function(
    project: &Project,
    caller: Caller,
    name: &str,
    input: &Value,
    bridge: Bridge,
    dev: bool,
) -> Result<(u16, Value), Failure> {
    let (function, request) = calls::prepare(&project.functions, name, input, caller.call)?;
    let host = host(project, bridge, dev, caller.request_id, false);
    // The function has its own time limit, and no more time than the
    // request it's called for.
    let deadline = caller
        .deadline
        .min(Deadline(host.monotonic() + function.timeout));
    let reply = host
        .call(&function.handler, request, Body::Empty, deadline)
        .await;
    let (response, body) = match reply {
        Ok(Reply::Response { response, body }) => (response, body),
        Ok(Reply::Continue(_)) => return Err(calls::continued()),
        Err(e) => return Err(calls::failed(e)),
    };
    let answer = match body {
        Body::Empty => calls::Answer::Empty,
        Body::Bytes(bytes) => calls::Answer::Bytes(bytes),
        // Bytes and nothing arrive as such: what's left is a stream.
        Body::Host(_) => calls::Answer::Stream,
    };
    calls::finish(
        &function,
        project.server.response_validation,
        response,
        answer,
    )
}

/// What a handler's `ctx.fetch` asked for.
struct Asked<'a> {
    service: &'a str,
    method: &'a str,
    path: &'a str,
    headers: &'a BTreeMap<String, String>,
    body: Bytes,
}

/// Makes a handler's request to a service as the server does (see
/// `unsworn_core::outbound`), with the request sent by the bridge. `Err`
/// is the status and reason there's no answer.
async fn fetch_service(
    project: &Project,
    caller: Caller,
    asked: Asked<'_>,
    bridge: Bridge,
    dev: bool,
) -> Result<http::Response<Bytes>, (u16, String)> {
    let service = outbound::service(&project.outbound, asked.service)?;
    let planned = outbound::plan(
        service,
        asked.method,
        asked.path,
        asked.headers,
        caller.call.trace.as_ref(),
    )?;
    let mut request = http::Request::builder()
        .method(planned.method.as_str())
        .uri(planned.url);
    for (name, value) in &planned.headers {
        request = request.header(name, value);
    }
    let request = request
        .body(asked.body)
        .map_err(|e| (400, format!("the request can't be made: {e}")))?;

    let host = host(project, bridge, dev, caller.request_id, false);
    let started = host.monotonic();
    // The service's time limit, and no more than the handler's own request
    // has left.
    let timeout = service
        .timeout
        .min(caller.deadline.0.saturating_sub(started));
    let limit = usize::try_from(service.max_response).unwrap_or(usize::MAX);
    let outcome = host.send(request, timeout, limit, true).await;
    // What a handler reached, and when: the query is left out, since
    // that's where a token may be.
    let path = asked.path.split('?').next().unwrap_or_default();
    let (name, method, id) = (&service.name, &planned.method, &host.request_id);
    let failed = |status: u16, error: String| {
        tracing::warn!("outbound service={name} method={method} path={path} id={id}: {error}");
        (status, error)
    };
    match outcome {
        Ok(response) => {
            let ms = host.monotonic().saturating_sub(started).as_secs_f64() * 1000.0;
            let status = response.status().as_u16();
            tracing::info!(
                "outbound service={name} method={method} path={path} status={status} ms={ms:.0} id={id}"
            );
            Ok(response)
        }
        Err(FetchError::Timeout) => Err(failed(504, "the service took too long to answer".into())),
        Err(FetchError::Connect(e)) => Err(failed(502, format!("couldn't reach the service: {e}"))),
        // The bridge reads up to the limit, and says so when there's more.
        Err(FetchError::Read(e)) if e.contains("is larger than") => {
            Err(failed(502, outbound::too_large(service)))
        }
        Err(FetchError::Read(e)) => Err(failed(502, format!("the response couldn't be read: {e}"))),
    }
}

/// The request, the client's IP and whether it came over HTTPS.
fn request_from_js(value: &JsValue) -> Result<(http::Request<JsBody>, IpAddr, bool), String> {
    let method = get(value, "method").as_string().unwrap_or_default();
    let method = Method::from_bytes(method.as_bytes()).map_err(|_| "bad method".to_string())?;
    let url = get(value, "url").as_string().unwrap_or_default();
    let url: Uri = url.parse().map_err(|_| format!("bad URL {url}"))?;
    let tls = url.scheme_str() == Some("https");
    let mut headers = headers_from_js(&get(value, "headers"));
    if !headers.contains_key(header::HOST)
        && let Some(v) = url
            .authority()
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    {
        headers.insert(header::HOST, v);
    }
    let path = url
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let mut req = http::Request::new(JsBody(get(value, "body")));
    *req.method_mut() = method;
    *req.uri_mut() = path.parse().map_err(|_| format!("bad path {path}"))?;
    *req.headers_mut() = headers;
    let ip = get(value, "ip")
        .as_string()
        .and_then(|s| s.parse().ok())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    Ok((req, ip, tls))
}

/// One step of the token bucket every store runs (see
/// `unsworn_core::store::take_token`), for a JS store such as a Durable
/// Object. `tokens` and `lastMs` are the saved state, or `undefined` for a
/// new bucket. Returns `[tokens, lastMs, waitMs]`; `waitMs` is `-1` when
/// the request is allowed.
#[wasm_bindgen(js_name = takeToken)]
pub fn take_token(
    tokens: Option<f64>,
    last_ms: Option<f64>,
    now_ms: f64,
    requests: u32,
    per_ms: f64,
) -> Vec<f64> {
    let ms = |v: f64| Duration::from_secs_f64(v.max(0.0) / 1000.0);
    let bucket = match (tokens, last_ms) {
        (Some(tokens), Some(last)) => Some(Bucket {
            tokens,
            last: ms(last),
        }),
        _ => None,
    };
    let (b, wait) = core_take_token(bucket, ms(now_ms), requests.max(1), ms(per_ms.max(1.0)));
    vec![
        b.tokens,
        b.last.as_secs_f64() * 1000.0,
        wait.map_or(-1.0, |w| w.as_secs_f64() * 1000.0),
    ]
}

/// The token bucket as a Redis script, the one every Unsworn host runs
/// (see `unsworn_core::store::REDIS_RATE_LIMIT`): `EVAL script 1
/// {prefix}rl:{key} requests perMs` returns the milliseconds to wait, or 0.
#[wasm_bindgen(js_name = redisRateLimitScript)]
pub fn redis_rate_limit_script() -> String {
    unsworn_core::store::REDIS_RATE_LIMIT.to_string()
}

/// The Unsworn version this module was built from.
#[wasm_bindgen]
pub fn version() -> String {
    unsworn_core::VERSION.to_string()
}
