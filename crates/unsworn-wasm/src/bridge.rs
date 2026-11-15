//! The JavaScript side of a host: the functions the wasm module calls for
//! I/O, and conversions between JS values and `http` types.
//!
//! A JS host (the Cloudflare Worker entry, a serverless shim) passes a
//! [`Bridge`] object with each request. Values cross as plain objects:
//! headers as `[name, value]` pairs, bodies as `null`, a `Uint8Array` or a
//! `ReadableStream`.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use js_sys::{Array, Object, Promise, Reflect, Uint8Array};
use unsworn_core::pipeline as core;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen(typescript_custom_section)]
const BRIDGE_TS: &str = r#"
/** A body crossing the boundary: none, bytes, or a stream. */
export type BodyValue = null | undefined | Uint8Array | ReadableStream<Uint8Array>;

/** Headers as `[name, value]` pairs, repeated names allowed. */
export type HeaderPairs = [string, string][];

export interface HttpRequestValue {
  method: string;
  /** An absolute URL. */
  url: string;
  headers: HeaderPairs;
  body: BodyValue;
}

export interface HttpResponseValue {
  status: number;
  headers: HeaderPairs;
  body: BodyValue;
}

/** An incoming request for `Unsworn.handle`. */
export interface IncomingRequest extends HttpRequestValue {
  /** The client's IP address. */
  ip: string;
  requestId: string;
}

/** A handled request: the response, plus the matched route for logs. */
export interface HandledResponse extends HttpResponseValue {
  route: string;
}

/** What a handler call produced. `response` is a protocol `response` message. */
export type CallResult =
  | { type: "response"; response: Record<string, unknown>; body?: BodyValue }
  | { type: "continue"; context: Record<string, unknown> }
  | { type: "error"; message: string }
  | { type: "missing"; message: string }
  | { type: "timeout" }
  | { type: "body"; error: "too_large" | "idle" | "read" };

/** The I/O a JavaScript host does for the core. */
export interface Bridge {
  /** Calls a handler. `request` is a protocol `request` message as JSON. Must settle within `timeoutMs`, resolving `{ type: "timeout" }` if the handler doesn't. */
  call(request: string, body: BodyValue, timeoutMs: number): Promise<CallResult>;
  /** Reads a stream whole. Resolves `null` past `max` bytes; rejects when it can't be read. */
  readBody(body: ReadableStream<Uint8Array>, max: number): Promise<Uint8Array | null>;
  /** A stream that errors once more than `max` bytes pass through. */
  limitBody(body: ReadableStream<Uint8Array>, max: number): ReadableStream<Uint8Array>;
  /** Reads a stream whole if it's at most `limit` bytes; otherwise a stream with the same data. Never rejects. */
  collect(body: ReadableStream<Uint8Array>, limit: number): Promise<{ bytes: Uint8Array } | { body: ReadableStream<Uint8Array> }>;
  /** Sends a request for the core (a JWKS document, an API-key check), reading up to `limit` bytes of the answer. */
  fetch(request: HttpRequestValue, timeoutMs: number, limit: number): Promise<HttpResponseValue | { error: "timeout" | "connect" | "read"; message: string }>;
  /** Forwards a rewritten request to a `proxy` route's upstream. Rejects when it can't be reached. */
  proxy(request: HttpRequestValue): Promise<HttpResponseValue>;
  /** A static asset by URL path, or `null` when there's none. */
  asset(path: string, method: string, headers: HeaderPairs): Promise<HttpResponseValue | null>;
  /** Takes a token from a rate-limit bucket: `null` when allowed, or milliseconds until one is available. */
  rateLimit(key: string, requests: number, perMs: number): Promise<number | null>;
  cacheGet(key: string): Promise<Uint8Array | null>;
  cachePut(key: string, value: Uint8Array, ttlMs: number): Promise<void>;
  /** Keeps the isolate alive until `promise` settles, without delaying the response. */
  waitUntil(promise: Promise<unknown>): void;
  sleep(ms: number): Promise<void>;
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "Bridge")]
    #[derive(Clone)]
    pub type Bridge;

    #[wasm_bindgen(method, catch)]
    pub fn call(
        this: &Bridge,
        request: &str,
        body: &JsValue,
        timeout_ms: f64,
    ) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch, js_name = readBody)]
    pub fn read_body(this: &Bridge, body: &JsValue, max: f64) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch, js_name = limitBody)]
    pub fn limit_body(this: &Bridge, body: &JsValue, max: f64) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn collect(this: &Bridge, body: &JsValue, limit: f64) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn fetch(
        this: &Bridge,
        request: &JsValue,
        timeout_ms: f64,
        limit: f64,
    ) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn proxy(this: &Bridge, request: &JsValue) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn asset(
        this: &Bridge,
        path: &str,
        method: &str,
        headers: &JsValue,
    ) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch, js_name = rateLimit)]
    pub fn rate_limit(
        this: &Bridge,
        key: &str,
        requests: u32,
        per_ms: f64,
    ) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch, js_name = cacheGet)]
    pub fn cache_get(this: &Bridge, key: &str) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch, js_name = cachePut)]
    pub fn cache_put(
        this: &Bridge,
        key: &str,
        value: &Uint8Array,
        ttl_ms: f64,
    ) -> Result<Promise, JsValue>;
    #[wasm_bindgen(method, catch, js_name = waitUntil)]
    pub fn wait_until(this: &Bridge, promise: &Promise) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn sleep(this: &Bridge, ms: f64) -> Result<Promise, JsValue>;
}

/// A body the JS host owns: `null`, a `Uint8Array` or a `ReadableStream`.
pub struct JsBody(pub JsValue);

impl JsBody {
    pub fn is_none(&self) -> bool {
        self.0.is_null() || self.0.is_undefined()
    }

    pub fn as_bytes(&self) -> Option<Uint8Array> {
        self.0.dyn_ref::<Uint8Array>().cloned()
    }
}

/// Awaits a promise a bridge call returned.
pub async fn resolve(promise: Result<Promise, JsValue>) -> Result<JsValue, JsValue> {
    JsFuture::from(promise?).await
}

pub fn get(value: &JsValue, key: &str) -> JsValue {
    Reflect::get(value, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

pub fn set(target: &Object, key: &str, value: &JsValue) {
    let _ = Reflect::set(target, &JsValue::from_str(key), value);
}

/// The message of a thrown value.
pub fn error_text(e: &JsValue) -> String {
    if let Some(s) = e.as_string() {
        return s;
    }
    get(e, "message")
        .as_string()
        .unwrap_or_else(|| format!("{e:?}"))
}

pub fn headers_to_js(headers: &HeaderMap) -> Array {
    headers
        .iter()
        .map(|(k, v)| {
            Array::of2(
                &JsValue::from_str(k.as_str()),
                &JsValue::from_str(&String::from_utf8_lossy(v.as_bytes())),
            )
        })
        .collect()
}

/// Reads `[name, value]` pairs, skipping any that aren't valid headers.
pub fn headers_from_js(value: &JsValue) -> HeaderMap {
    let mut out = HeaderMap::new();
    let Some(pairs) = value.dyn_ref::<Array>() else {
        return out;
    };
    for pair in pairs.iter() {
        let (Some(k), Some(v)) = (get(&pair, "0").as_string(), get(&pair, "1").as_string()) else {
            continue;
        };
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out.append(k, v);
        }
    }
    out
}

pub fn bytes_to_js(bytes: &[u8]) -> Uint8Array {
    Uint8Array::from(bytes)
}

pub fn body_to_js(body: core::Body<JsBody>) -> JsValue {
    match body {
        core::Body::Empty => JsValue::NULL,
        core::Body::Bytes(b) => bytes_to_js(&b).into(),
        core::Body::Host(b) => b.0,
    }
}

pub fn body_from_js(value: JsValue) -> core::Body<JsBody> {
    if value.is_null() || value.is_undefined() {
        return core::Body::Empty;
    }
    match value.dyn_ref::<Uint8Array>() {
        Some(a) => core::Body::bytes(Some(Bytes::from(a.to_vec()))),
        None => core::Body::Host(JsBody(value)),
    }
}

/// `{ method, url, headers, body }` for a request the bridge sends.
pub fn request_to_js(parts: &http::request::Parts, body: JsValue) -> Object {
    let out = Object::new();
    set(&out, "method", &JsValue::from_str(parts.method.as_str()));
    set(&out, "url", &JsValue::from_str(&parts.uri.to_string()));
    set(&out, "headers", &headers_to_js(&parts.headers));
    set(&out, "body", &body);
    out
}

/// A response value from the bridge. A bad status becomes `502`.
pub fn response_from_js(value: &JsValue) -> core::Response<JsBody> {
    let status = get(value, "status")
        .as_f64()
        .and_then(|s| StatusCode::from_u16(s as u16).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp = http::Response::new(body_from_js(get(value, "body")));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers_from_js(&get(value, "headers"));
    resp
}

pub fn response_to_js(resp: core::Response<JsBody>) -> Object {
    let (parts, body) = resp.into_parts();
    let out = Object::new();
    set(&out, "status", &JsValue::from(parts.status.as_u16()));
    set(&out, "headers", &headers_to_js(&parts.headers));
    set(&out, "body", &body_to_js(body));
    out
}
