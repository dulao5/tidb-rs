//! Direct `tracing_subscriber::Layer` bridge into `UnifiedLoggerCore`.
//!
//! Captures `tracing::*!` events in field declaration order without `tracing_subscriber::fmt`
//! or `tracing-log`.

use std::fmt::{self, Write as _};

use tidb_base::structured_log::{LogField, LogLevel, LogRecord, LogTarget, StructuredLogger};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::encoder::{OwnedLogField, OwnedLogValue};
use crate::logger;

/// `tracing_subscriber::Layer` forwarding events directly to [`crate::logger::UnifiedLoggerCore`].
pub struct UnifiedTracingLayer;

/// Register `tracing_subscriber::registry().with(UnifiedTracingLayer)` globally.
pub fn install() {
    let _ = tracing_subscriber::registry()
        .with(UnifiedTracingLayer)
        .try_init();
}

fn map_level(level: &Level) -> Option<LogLevel> {
    match *level {
        // Filter out TRACE events so third-party crates (hyper/h2/tonic) do not flood [DEBUG].
        Level::TRACE => None,
        Level::DEBUG => Some(LogLevel::Debug),
        Level::INFO => Some(LogLevel::Info),
        Level::WARN => Some(LogLevel::Warn),
        Level::ERROR => Some(LogLevel::Error),
    }
}

struct EventVisitor {
    message: String,
    fields: Vec<OwnedLogField>,
}

impl Visit for EventVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(OwnedLogField {
                key: field.name().to_owned(),
                value: OwnedLogValue::Bool(value),
            });
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(OwnedLogField {
                key: field.name().to_owned(),
                value: OwnedLogValue::I64(value),
            });
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(OwnedLogField {
                key: field.name().to_owned(),
                value: OwnedLogValue::U64(value),
            });
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(OwnedLogField {
                key: field.name().to_owned(),
                value: OwnedLogValue::F64(value),
            });
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.fields.push(OwnedLogField {
                key: field.name().to_owned(),
                value: OwnedLogValue::Str(value.to_owned()),
            });
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let mut s = String::new();
        let _ = write!(&mut s, "{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.fields.push(OwnedLogField {
                key: field.name().to_owned(),
                value: OwnedLogValue::Str(s),
            });
        }
    }
}

impl<S: Subscriber> Layer<S> for UnifiedTracingLayer {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        let Some(level) = map_level(metadata.level()) else {
            return false;
        };
        logger::core().enabled(LogTarget::Main, level)
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let Some(level) = map_level(meta.level()) else {
            return;
        };
        let core = logger::core();
        if !core.enabled(LogTarget::Main, level) {
            return;
        }
        let mut visitor = EventVisitor {
            message: String::new(),
            fields: Vec::new(),
        };
        event.record(&mut visitor);
        let borrowed: Vec<LogField<'_>> =
            visitor.fields.iter().map(OwnedLogField::as_field).collect();
        core.emit_with_bound(
            &LogRecord {
                target: LogTarget::Main,
                level,
                logger_name: "",
                file: meta.file().unwrap_or(""),
                line: meta.line().unwrap_or(0),
                msg: &visitor.message,
                fields: &borrowed,
            },
            &[],
        );
    }
}
