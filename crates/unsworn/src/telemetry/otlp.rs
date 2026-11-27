//! The part of OTLP's protobuf that spans need, written by hand. The field
//! numbers are those of opentelemetry-proto's `trace.proto`, `common.proto`
//! and `resource.proto`.

use unsworn_core::VERSION;

use super::SpanRecord;

// How a field's value is laid out.
const VARINT: u32 = 0;
const FIXED64: u32 = 1;
const LEN: u32 = 2;

/// `Span.SpanKind.SPAN_KIND_SERVER`.
const SERVER: u64 = 2;
/// `Status.StatusCode.STATUS_CODE_ERROR`.
const ERROR: u64 = 2;

/// An attribute's value.
#[derive(Clone, Copy)]
enum Value<'a> {
    String(&'a str),
    Int(i64),
    Double(f64),
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn tag(out: &mut Vec<u8>, field: u32, wire: u32) {
    varint(out, u64::from(field << 3 | wire));
}

fn uint(out: &mut Vec<u8>, field: u32, value: u64) {
    tag(out, field, VARINT);
    varint(out, value);
}

fn fixed64(out: &mut Vec<u8>, field: u32, value: u64) {
    tag(out, field, FIXED64);
    out.extend_from_slice(&value.to_le_bytes());
}

fn bytes(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    tag(out, field, LEN);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// A field holding another message, which `write` puts together.
fn message(out: &mut Vec<u8>, field: u32, write: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    write(&mut inner);
    bytes(out, field, &inner);
}

/// A `KeyValue`, as the field `field` of the message in `out`.
fn attribute(out: &mut Vec<u8>, field: u32, key: &str, value: Value<'_>) {
    message(out, field, |kv| {
        bytes(kv, 1, key.as_bytes());
        message(kv, 2, |any| match value {
            Value::String(s) => bytes(any, 1, s.as_bytes()),
            Value::Int(n) => uint(any, 3, n as u64),
            Value::Double(n) => fixed64(any, 4, n.to_bits()),
        });
    });
}

/// A `Resource`: what the spans come from.
pub fn resource(attributes: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in attributes {
        attribute(&mut out, 1, key, Value::String(value));
    }
    out
}

/// An `ExportTraceServiceRequest` with these spans, all from `resource`.
pub fn request<'a>(resource: &[u8], spans: impl Iterator<Item = &'a SpanRecord>) -> Vec<u8> {
    let mut out = Vec::new();
    // `resource_spans`
    message(&mut out, 1, |resource_spans| {
        bytes(resource_spans, 1, resource);
        // `scope_spans`
        message(resource_spans, 2, |scope_spans| {
            message(scope_spans, 1, |scope| {
                bytes(scope, 1, b"unsworn");
                bytes(scope, 2, VERSION.as_bytes());
            });
            for record in spans {
                message(scope_spans, 2, |out| span(out, record));
            }
        });
    });
    out
}

fn span(out: &mut Vec<u8>, span: &SpanRecord) {
    bytes(out, 1, &span.trace_id.0);
    bytes(out, 2, &span.span_id.0);
    if let Some(parent) = &span.parent {
        bytes(out, 4, &parent.0);
    }
    bytes(out, 5, span.name.as_bytes());
    uint(out, 6, SERVER);
    fixed64(out, 7, span.start_unix_nano);
    fixed64(out, 8, span.end_unix_nano);
    let client = span.client.to_string();
    let mut put = |key: &str, value: Value<'_>| attribute(out, 9, key, value);
    put("http.request.method", Value::String(span.method));
    put("url.path", Value::String(&span.path));
    put("url.scheme", Value::String(span.scheme));
    if let Some(route) = &span.route {
        put("http.route", Value::String(route));
    }
    put(
        "http.response.status_code",
        Value::Int(i64::from(span.status)),
    );
    put("client.address", Value::String(&client));
    put("network.protocol.version", Value::String(span.protocol));
    put("unsworn.request_id", Value::String(&span.request_id));
    if let Some(runtime) = &span.runtime {
        put("unsworn.runtime", Value::String(runtime));
    }
    put("unsworn.in_handlers_ms", Value::Double(span.in_handlers_ms));
    // A client's error isn't the server's: only `500` and above fail a
    // server span.
    if span.status >= 500 {
        message(out, 15, |status| uint(status, 3, ERROR));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        let encoded = |value| {
            let mut out = Vec::new();
            varint(&mut out, value);
            out
        };
        assert_eq!(encoded(0), [0x00]);
        assert_eq!(encoded(1), [0x01]);
        assert_eq!(encoded(127), [0x7f]);
        assert_eq!(encoded(128), [0x80, 0x01]);
        assert_eq!(encoded(300), [0xac, 0x02]);
        let mut most = vec![0xff; 9];
        most.push(0x01);
        assert_eq!(encoded(u64::MAX), most);
    }

    #[test]
    fn fields() {
        let mut out = Vec::new();
        fixed64(&mut out, 7, 0x0102_0304_0506_0708);
        assert_eq!(out, [0x39, 8, 7, 6, 5, 4, 3, 2, 1]);
        let mut out = Vec::new();
        message(&mut out, 15, |status| uint(status, 3, ERROR));
        assert_eq!(out, [0x7a, 0x02, 0x18, 0x02]);
        // An attribute: the key, then an `AnyValue` holding a string.
        assert_eq!(
            resource(&[("k".into(), "v".into())]),
            [0x0a, 0x08, 0x0a, 0x01, b'k', 0x12, 0x03, 0x0a, 0x01, b'v']
        );
    }
}
