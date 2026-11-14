//! Sends `tracing` events (and `log` records) to Unsworn.
//!
//! [`run`](crate::run) and [`serve`](crate::serve) install [`LogLayer`] as
//! the global subscriber unless the program already set one. Events emitted
//! while a handler runs are tagged with its request:
//!
//! ```no_run
//! # async fn charge(req: unsworn_sdk::Request) -> unsworn_sdk::Result {
//! tracing::info!(amount = 1200, "charged card");
//! # Ok(unsworn_sdk::Reply::status(200))
//! # }
//! ```

use std::fmt;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use unsworn_proto::{Frame, Log, LogLevel, Message};

use crate::worker::Out;

tokio::task_local! {
    pub(crate) static REQUEST_ID: String;
}

static SINK: Mutex<Option<Out>> = Mutex::new(None);

/// The least severe level Unsworn wants, from `UNSWORN_LOG_LEVEL`. `None`
/// when Unsworn doesn't accept log records.
fn max_level() -> Option<Option<Level>> {
    static LEVEL: OnceLock<Option<Option<Level>>> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        let value = std::env::var("UNSWORN_LOG_LEVEL").ok()?;
        Some(match value.as_str() {
            "error" => Some(Level::ERROR),
            "warn" => Some(Level::WARN),
            "info" => Some(Level::INFO),
            "debug" => Some(Level::DEBUG),
            "trace" => Some(Level::TRACE),
            _ => None, // "off"
        })
    })
}

pub(crate) fn install(out: &Out) {
    if max_level().is_none() {
        return;
    }
    *SINK.lock().unwrap() = Some(out.clone());
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let _ = tracing_subscriber::registry().with(LogLayer).try_init();
}

/// Lets the worker's output close at shutdown.
pub(crate) fn uninstall() {
    SINK.lock().unwrap().take();
}

/// A [`Layer`] that sends events to Unsworn as log records, for programs
/// that set up their own subscriber:
///
/// ```no_run
/// use tracing_subscriber::prelude::*;
/// tracing_subscriber::registry()
///     .with(unsworn_sdk::LogLayer)
///     .init();
/// ```
///
/// Before the worker starts, or under an Unsworn version without log
/// records, events are written to stderr.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogLayer;

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if let Some(max) = max_level()
            && max.is_none_or(|max| level > max)
        {
            return;
        }
        let mut visitor = Fields::default();
        event.record(&mut visitor);
        let sink = SINK.lock().unwrap().clone();
        let Some(out) = sink else {
            let fields = Value::Object(visitor.fields);
            eprintln!("[{level}] {} {fields}", visitor.message);
            return;
        };
        out.send(Frame::new(Message::Log(Log {
            id: REQUEST_ID.try_with(Clone::clone).ok(),
            level: match level {
                Level::ERROR => LogLevel::Error,
                Level::WARN => LogLevel::Warn,
                Level::INFO => LogLevel::Info,
                Level::DEBUG => LogLevel::Debug,
                _ => LogLevel::Trace,
            },
            message: visitor.message,
            fields: visitor.fields,
        })));
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    fields: Map<String, Value>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: Value) {
        match field.name() {
            "message" => {
                self.message = match value {
                    Value::String(s) => s,
                    other => other.to_string(),
                }
            }
            // Added by tracing-log for `log` records.
            name if name.starts_with("log.") => {}
            name => {
                self.fields.insert(name.to_string(), value);
            }
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::from(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::from(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, Value::from(format!("{value:?}")));
    }
}
