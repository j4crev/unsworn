//! Sends the core's `tracing` events and panics to the JS console, which
//! edge platforms collect as the Worker's logs.

use std::fmt::Write;
use std::sync::Once;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber, span};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(s: &str);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(s: &str);
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(s: &str);
    #[wasm_bindgen(js_namespace = console, js_name = debug)]
    fn console_debug(s: &str);
}

struct Console {
    max: Level,
}

#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

impl Subscriber for Console {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= self.max
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = Line::default();
        event.record(&mut line);
        let text = format!("unsworn: {}{}", line.message, line.fields);
        match *event.metadata().level() {
            Level::ERROR => console_error(&text),
            Level::WARN => console_warn(&text),
            Level::INFO => console_log(&text),
            _ => console_debug(&text),
        }
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

static INIT: Once = Once::new();

/// Installs the console logger and panic hook, once per isolate.
pub fn init(dev: bool) {
    INIT.call_once(|| {
        std::panic::set_hook(Box::new(|info| console_error(&format!("unsworn: {info}"))));
        let max = if dev { Level::DEBUG } else { Level::INFO };
        let _ = tracing::subscriber::set_global_default(Console { max });
    });
}
