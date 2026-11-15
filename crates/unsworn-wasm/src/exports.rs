//! What the module exports to JavaScript.

use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderValue, Method, StatusCode, Uri, header};
use js_sys::{Object, Promise};
use unsworn_core::pipeline::{KeyCache, Problem, Router, problem_response};
use unsworn_core::store::{Bucket, take_token as core_take_token};
use unsworn_core::validate::Build;
use unsworn_core::{Level, compile_with};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::bridge::{Bridge, JsBody, get, headers_from_js, response_to_js, set};
use crate::files::Files;
use crate::host::{WasmHost, WasmStore};
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
            let host = WasmHost {
                store: WasmStore {
                    bridge: bridge.clone(),
                },
                bridge,
                dev,
                request_id: request_id.clone(),
                tls,
            };
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

/// The Unsworn version this module was built from.
#[wasm_bindgen]
pub fn version() -> String {
    unsworn_core::VERSION.to_string()
}
