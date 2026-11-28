//! The part of OTLP's protobuf that spans need, written by hand. The field
//! numbers are those of opentelemetry-proto's `trace.proto`, `common.proto`
//! and `resource.proto`.

use super::{Attr, Attributes, SpanId, SpanRecord, TraceId};
use crate::VERSION;

// How a field's value is laid out.
const VARINT: u32 = 0;
const FIXED64: u32 = 1;
const LEN: u32 = 2;

/// `Status.StatusCode.STATUS_CODE_ERROR`.
const ERROR: u64 = 2;

/// `AggregationTemporality.AGGREGATION_TEMPORALITY_CUMULATIVE`.
const CUMULATIVE: u64 = 2;

/// An attribute's value.
#[derive(Clone, Copy)]
enum Value<'a> {
    String(&'a str),
    Bool(bool),
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
            Value::Bool(b) => uint(any, 2, u64::from(b)),
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
    uint(out, 6, span.kind as u64);
    fixed64(out, 7, span.start_unix_nano);
    fixed64(out, 8, span.end_unix_nano);
    attributes(out, 9, &span.attributes);
    for event in &span.events {
        message(out, 11, |out| {
            fixed64(out, 1, event.time_unix_nano);
            bytes(out, 2, event.name.as_bytes());
            attributes(out, 3, &event.attributes);
        });
    }
    if let Some(why) = &span.error {
        message(out, 15, |out| failed(out, why));
    }
}

/// `KeyValue`s, each as the field `field`.
fn attributes(out: &mut Vec<u8>, field: u32, attributes: &[(&'static str, Attr)]) {
    for (key, value) in attributes {
        attribute(out, field, key, attr_value(value));
    }
}

fn attr_value(value: &Attr) -> Value<'_> {
    match value {
        Attr::Str(s) => Value::String(s),
        Attr::Static(s) => Value::String(s),
        Attr::Bool(b) => Value::Bool(*b),
        Attr::Int(n) => Value::Int(*n),
        Attr::Double(n) => Value::Double(*n),
    }
}

/// A `Status` saying a span failed, and why.
fn failed(out: &mut Vec<u8>, why: &str) {
    bytes(out, 2, why.as_bytes());
    uint(out, 3, ERROR);
}

/// One metric to send over OTLP.
pub struct Metric {
    pub name: &'static str,
    pub unit: &'static str,
    pub data: MetricData,
}

/// A metric's kind and data points.
pub enum MetricData {
    Gauge(Vec<NumberPoint>),
    /// A cumulative, monotonic sum.
    Sum(Vec<NumberPoint>),
    /// A cumulative histogram.
    Histogram(Vec<HistogramPoint>),
}

/// One point of a gauge or sum.
pub struct NumberPoint {
    pub attributes: Attributes,
    pub start_unix_nano: u64,
    pub time_unix_nano: u64,
    pub value: Number,
}

/// A point's number.
#[derive(Clone, Copy)]
pub enum Number {
    Int(i64),
    Double(f64),
}

/// One point of a histogram.
pub struct HistogramPoint {
    pub attributes: Attributes,
    pub start_unix_nano: u64,
    pub time_unix_nano: u64,
    pub count: u64,
    pub sum: f64,
    pub bounds: Vec<f64>,
    pub buckets: Vec<u64>,
}

/// An `ExportMetricsServiceRequest` with these metrics, all from `resource`.
pub fn metrics_request(resource: &[u8], metrics: &[Metric]) -> Vec<u8> {
    let mut out = Vec::new();
    // `resource_metrics`
    message(&mut out, 1, |resource_metrics| {
        bytes(resource_metrics, 1, resource);
        // `scope_metrics`
        message(resource_metrics, 2, |scope_metrics| {
            message(scope_metrics, 1, |scope| {
                bytes(scope, 1, b"unsworn");
                bytes(scope, 2, VERSION.as_bytes());
            });
            for m in metrics {
                message(scope_metrics, 2, |out| metric(out, m));
            }
        });
    });
    out
}

fn metric(out: &mut Vec<u8>, metric: &Metric) {
    bytes(out, 1, metric.name.as_bytes());
    if !metric.unit.is_empty() {
        bytes(out, 3, metric.unit.as_bytes());
    }
    match &metric.data {
        MetricData::Gauge(points) => message(out, 5, |gauge| {
            for point in points {
                message(gauge, 1, |out| number_point(out, point));
            }
        }),
        MetricData::Sum(points) => message(out, 7, |sum| {
            for point in points {
                message(sum, 1, |out| number_point(out, point));
            }
            uint(sum, 2, CUMULATIVE);
            uint(sum, 3, 1);
        }),
        MetricData::Histogram(points) => message(out, 9, |histogram| {
            for point in points {
                message(histogram, 1, |out| histogram_point(out, point));
            }
            uint(histogram, 2, CUMULATIVE);
        }),
    }
}

fn number_point(out: &mut Vec<u8>, point: &NumberPoint) {
    attributes(out, 7, &point.attributes);
    if point.start_unix_nano != 0 {
        fixed64(out, 2, point.start_unix_nano);
    }
    fixed64(out, 3, point.time_unix_nano);
    match point.value {
        Number::Double(n) => fixed64(out, 4, n.to_bits()),
        Number::Int(n) => fixed64(out, 6, n as u64),
    }
}

fn histogram_point(out: &mut Vec<u8>, point: &HistogramPoint) {
    attributes(out, 9, &point.attributes);
    if point.start_unix_nano != 0 {
        fixed64(out, 2, point.start_unix_nano);
    }
    fixed64(out, 3, point.time_unix_nano);
    fixed64(out, 4, point.count);
    fixed64(out, 5, point.sum.to_bits());
    packed_fixed64(out, 6, point.buckets.iter().copied());
    packed_fixed64(out, 7, point.bounds.iter().map(|b| b.to_bits()));
}

fn packed_fixed64(out: &mut Vec<u8>, field: u32, values: impl ExactSizeIterator<Item = u64>) {
    if values.len() == 0 {
        return;
    }
    tag(out, field, LEN);
    varint(out, (values.len() * 8) as u64);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// Severity of a log record, as OTLP numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Trace = 1,
    Debug = 5,
    Info = 9,
    Warn = 13,
    Error = 17,
}

impl Severity {
    pub fn text(self) -> &'static str {
        match self {
            Severity::Trace => "TRACE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
        }
    }
}

/// One log record to send over OTLP.
#[derive(Debug)]
pub struct LogRecord {
    pub time_unix_nano: u64,
    pub severity: Severity,
    pub body: String,
    pub attributes: Vec<(String, Attr)>,
    pub trace_id: Option<TraceId>,
    pub span_id: Option<SpanId>,
}

/// An `ExportLogsServiceRequest` with these records, all from `resource`.
pub fn logs_request<'a>(resource: &[u8], records: impl Iterator<Item = &'a LogRecord>) -> Vec<u8> {
    let mut out = Vec::new();
    // `resource_logs`
    message(&mut out, 1, |resource_logs| {
        bytes(resource_logs, 1, resource);
        // `scope_logs`
        message(resource_logs, 2, |scope_logs| {
            message(scope_logs, 1, |scope| {
                bytes(scope, 1, b"unsworn");
                bytes(scope, 2, VERSION.as_bytes());
            });
            for record in records {
                message(scope_logs, 2, |out| log_record(out, record));
            }
        });
    });
    out
}

fn log_record(out: &mut Vec<u8>, record: &LogRecord) {
    fixed64(out, 1, record.time_unix_nano);
    uint(out, 2, record.severity as u64);
    bytes(out, 3, record.severity.text().as_bytes());
    message(out, 5, |any| bytes(any, 1, record.body.as_bytes()));
    for (key, value) in &record.attributes {
        attribute(out, 6, key, attr_value(value));
    }
    if let Some(trace_id) = &record.trace_id {
        bytes(out, 9, &trace_id.0);
    }
    if let Some(span_id) = &record.span_id {
        bytes(out, 10, &span_id.0);
    }
    fixed64(out, 11, record.time_unix_nano);
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
        // A status: the message, then the code.
        let mut out = Vec::new();
        message(&mut out, 15, |status| failed(status, "no"));
        assert_eq!(out, [0x7a, 0x06, 0x12, 0x02, b'n', b'o', 0x18, 0x02]);
        // An attribute: the key, then an `AnyValue` holding a string.
        assert_eq!(
            resource(&[("k".into(), "v".into())]),
            [0x0a, 0x08, 0x0a, 0x01, b'k', 0x12, 0x03, 0x0a, 0x01, b'v']
        );
    }
}
