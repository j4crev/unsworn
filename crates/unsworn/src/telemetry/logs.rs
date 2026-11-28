//! OTLP log export: a `tracing_subscriber` layer that forwards every event
//! `UNSWORN_LOG` lets through (except `access` and `telemetry`) to the
//! configured `[outbound]` service over `POST /v1/logs`.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use arc_swap::ArcSwapOption;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use unsworn_core::project::LOGS_PATH;
use unsworn_core::telemetry::{Attr, SpanId, TraceId, cut, otlp};

use super::{BATCH, DELAY, HttpClient, Lost, QUEUE, Sink, post, unix_nano};

/// A log record waiting to be sent, and where to.
type QueuedLog = (Arc<Sink>, otlp::LogRecord);

/// Active log sink: where events go when `[telemetry].logs = true`.
struct LogPipe {
    sink: Arc<Sink>,
    logs: mpsc::Sender<QueuedLog>,
    dropped: Arc<AtomicU64>,
}

static LOGS: LazyLock<ArcSwapOption<LogPipe>> = LazyLock::new(ArcSwapOption::empty);

/// The `tracing_subscriber` layer that forwards events to the OTLP log
/// exporter when `[telemetry].logs` is enabled.
pub struct LogLayer;

pub fn log_layer() -> LogLayer {
    LogLayer
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let pipe = LOGS.load();
        let Some(pipe) = pipe.as_ref() else { return };
        let meta = event.metadata();
        let target = meta.target();
        if matches!(target, "access" | "telemetry") {
            return;
        }
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        visitor
            .attributes
            .push(("target".to_string(), Attr::Str(target.to_string())));
        let record = otlp::LogRecord {
            time_unix_nano: unix_nano(SystemTime::now()),
            severity: match *meta.level() {
                Level::TRACE => otlp::Severity::Trace,
                Level::DEBUG => otlp::Severity::Debug,
                Level::INFO => otlp::Severity::Info,
                Level::WARN => otlp::Severity::Warn,
                Level::ERROR => otlp::Severity::Error,
            },
            body: cut(&visitor.message).to_string(),
            attributes: visitor.attributes,
            trace_id: visitor.trace_id,
            span_id: visitor.span_id,
        };
        if pipe.logs.try_send((pipe.sink.clone(), record)).is_err() {
            pipe.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
struct LogVisitor {
    message: String,
    attributes: Vec<(String, Attr)>,
    trace_id: Option<TraceId>,
    span_id: Option<SpanId>,
}

impl LogVisitor {
    fn record_text(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = value;
        } else if name == "trace_id"
            && let Some(id) = TraceId::from_hex(&value)
        {
            self.trace_id = Some(id);
        } else if name == "span_id"
            && let Some(id) = SpanId::from_hex(&value)
        {
            self.span_id = Some(id);
        } else {
            self.attributes
                .push((name.to_string(), Attr::Str(cut(&value).to_string())));
        }
    }
}

impl Visit for LogVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attributes
            .push((field.name().to_string(), Attr::Int(value)));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if let Ok(n) = i64::try_from(value) {
            self.attributes
                .push((field.name().to_string(), Attr::Int(n)));
        } else {
            self.attributes
                .push((field.name().to_string(), Attr::Str(value.to_string())));
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.attributes
            .push((field.name().to_string(), Attr::Double(value)));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attributes
            .push((field.name().to_string(), Attr::Bool(value)));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_text(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record_text(field.name(), format!("{value:?}"));
    }
}

/// Batches and sends log records, mirroring the span exporter.
pub(super) struct LogExporter {
    logs: mpsc::Sender<QueuedLog>,
    dropped: Arc<AtomicU64>,
    flushes: mpsc::Sender<oneshot::Sender<()>>,
    idle: Mutex<Option<SendingLogs>>,
}

impl LogExporter {
    pub(super) fn new(client: HttpClient) -> LogExporter {
        let (logs, queued) = mpsc::channel(QUEUE);
        let (flushes, asked) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        LogExporter {
            logs,
            dropped: dropped.clone(),
            flushes,
            idle: Mutex::new(Some(SendingLogs {
                client,
                logs: queued,
                flushes: asked,
                lost: Lost::new("logs", dropped),
            })),
        }
    }

    /// Points the global [`LogLayer`] at `sink`, or disables it when `None`.
    pub(super) fn set_sink(&self, sink: Option<Arc<Sink>>) {
        match sink {
            Some(sink) => {
                if let Some(sending) = self.idle.lock().unwrap().take() {
                    tokio::spawn(sending.run());
                }
                LOGS.store(Some(Arc::new(LogPipe {
                    sink,
                    logs: self.logs.clone(),
                    dropped: self.dropped.clone(),
                })));
            }
            None => {
                LOGS.store(None);
            }
        }
    }

    pub(super) async fn flush(&self, timeout: std::time::Duration) {
        if self.idle.lock().unwrap().is_some() {
            return;
        }
        let (done, sent) = oneshot::channel();
        let flushed = async {
            if self.flushes.send(done).await.is_ok() {
                let _ = sent.await;
            }
        };
        if tokio::time::timeout(timeout, flushed).await.is_err() {
            tracing::warn!(target: "telemetry", "not every log record was sent before shutting down");
        }
    }
}

struct SendingLogs {
    client: HttpClient,
    logs: mpsc::Receiver<QueuedLog>,
    flushes: mpsc::Receiver<oneshot::Sender<()>>,
    lost: Lost,
}

impl SendingLogs {
    async fn run(mut self) {
        let mut batch = Vec::new();
        let mut due = None;
        loop {
            let warning = self.lost.due();
            tokio::select! {
                record = self.logs.recv() => {
                    let Some(record) = record else { return };
                    if batch.is_empty() {
                        due = Some(Instant::now() + DELAY);
                    }
                    batch.push(record);
                    if batch.len() >= BATCH {
                        self.send(&mut batch).await;
                        due = None;
                    }
                }
                () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                    self.send(&mut batch).await;
                    due = None;
                }
                Some(done) = self.flushes.recv() => {
                    while let Ok(record) = self.logs.try_recv() {
                        batch.push(record);
                        if batch.len() >= BATCH {
                            self.send(&mut batch).await;
                        }
                    }
                    self.send(&mut batch).await;
                    due = None;
                    self.lost.warn(true);
                    let _ = done.send(());
                }
                () = tokio::time::sleep_until(warning.unwrap_or_else(Instant::now)), if warning.is_some() => {}
            }
            self.lost.warn(false);
        }
    }

    async fn send(&mut self, batch: &mut Vec<QueuedLog>) {
        let mut rest = std::mem::take(batch);
        while let Some((sink, _)) = rest.first() {
            let sink = sink.clone();
            let (records, others): (Vec<QueuedLog>, Vec<QueuedLog>) = rest
                .into_iter()
                .partition(|(other, _)| Arc::ptr_eq(other, &sink));
            rest = others;
            let body = otlp::logs_request(&sink.resource, records.iter().map(|(_, r)| r));
            if let Err(why) = post(&self.client, &sink.outbound, LOGS_PATH, body).await {
                self.lost.failed += records.len() as u64;
                self.lost.why = format!("[outbound.{}] {why}", sink.outbound.name);
            }
        }
    }
}
