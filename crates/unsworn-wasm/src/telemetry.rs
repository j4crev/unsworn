//! Per-request span recording and OTLP/HTTP export for WebAssembly hosts.
//!
//! Spans are built by [`unsworn_core::telemetry`] and collected in memory
//! for the duration of a sampled request. When the response is ready, one
//! `POST /v1/traces` is sent to the configured `[outbound]` service through
//! `bridge.waitUntil`, so the response never waits for the collector.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, header};
use unsworn_core::outbound;
use unsworn_core::project::{Outbound, TRACES_PATH};
use unsworn_core::telemetry::{Moment, Recorder, SpanRecord, otlp};

use crate::host::{WasmHost, now_ms};

pub type Parent = unsworn_core::telemetry::Parent<Arc<WasmRecorder>>;

/// Maximum bytes read from the collector's response.
const MAX_ANSWER: usize = 64 * 1024;

/// A random 64-bit integer from `Math.random()`. Span and trace ids need
/// to differ, not to be secret.
pub fn random() -> u64 {
    let hi = (js_sys::Math::random() * 4_294_967_296.0) as u64;
    let lo = (js_sys::Math::random() * 4_294_967_296.0) as u64;
    (hi << 32) | lo
}

/// Wall clock in nanoseconds since the Unix epoch.
pub fn now_unix_nano() -> u64 {
    (now_ms() * 1_000_000.0) as u64
}

/// Collects every span recorded for one sampled request.
#[derive(Default)]
pub struct WasmRecorder {
    spans: Mutex<Vec<SpanRecord>>,
}

impl WasmRecorder {
    pub fn take(&self) -> Vec<SpanRecord> {
        std::mem::take(&mut *self.spans.lock().unwrap())
    }
}

impl Recorder for WasmRecorder {
    fn record(&self, span: SpanRecord) {
        self.spans.lock().unwrap().push(span);
    }

    fn now(&self) -> Moment {
        let ms = now_ms();
        Moment {
            unix_nano: (ms * 1_000_000.0) as u64,
            monotonic: Duration::from_secs_f64(ms / 1000.0),
        }
    }

    fn random(&self) -> u64 {
        random()
    }
}

/// Sends one request's spans to the collector over OTLP/HTTP, without
/// following redirects.
pub async fn export(
    host: WasmHost,
    service: Arc<Outbound>,
    resource: Bytes,
    spans: Vec<SpanRecord>,
) {
    if spans.is_empty() {
        return;
    }
    let planned = match outbound::plan(&service, "POST", TRACES_PATH, &BTreeMap::new(), None) {
        Ok(p) => p,
        Err((_, why)) => {
            tracing::warn!("spans weren't exported: [outbound.{}] {why}", service.name);
            return;
        }
    };
    let mut request = http::Request::post(planned.url);
    for (name, value) in &planned.headers {
        request = request.header(name, value);
    }
    let body = Bytes::from(otlp::request(&resource, spans.iter()));
    let mut request = match request.body(body) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "spans weren't exported: [outbound.{}] can't be sent to: {e}",
                service.name
            );
            return;
        }
    };
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-protobuf"),
    );
    match host.send(request, service.timeout, MAX_ANSWER, true).await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            tracing::warn!(
                "spans weren't exported: [outbound.{}] answered {}",
                service.name,
                resp.status()
            );
        }
        Err(e) => {
            tracing::warn!("spans weren't exported: [outbound.{}] {e}", service.name);
        }
    }
}
