//! Direct `log::Log` bridge into `UnifiedLoggerCore` (covers `client-rust` and external deps).

use std::fmt::Write as _;

use log::kv::{Key, Value, VisitSource};
use tidb_base::structured_log::{LogField, LogLevel, LogRecord, LogTarget, StructuredLogger};

use crate::encoder::{OwnedLogField, OwnedLogValue};
use crate::logger;

struct UnifiedStdLogBridge;

static STD_LOG_BRIDGE: UnifiedStdLogBridge = UnifiedStdLogBridge;

/// Register the `log` facade bridge globally.
pub fn install() {
    let _ = log::set_logger(&STD_LOG_BRIDGE);
    let level = logger::get_level();
    log::set_max_level(match level {
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Warn => log::LevelFilter::Warn,
        _ => log::LevelFilter::Error,
    });
}

fn map_level(level: log::Level) -> LogLevel {
    match level {
        // Spec §1.1 / §3.3: Rust Trace maps to DEBUG (no non-Go [TRACE] level).
        log::Level::Trace | log::Level::Debug => LogLevel::Debug,
        log::Level::Info => LogLevel::Info,
        log::Level::Warn => LogLevel::Warn,
        log::Level::Error => LogLevel::Error,
    }
}

struct KvCollector {
    fields: Vec<OwnedLogField>,
}

impl<'kvs> VisitSource<'kvs> for KvCollector {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), log::kv::Error> {
        let owned = if let Some(b) = value.to_bool() {
            OwnedLogValue::Bool(b)
        } else if let Some(n) = value.to_i64() {
            OwnedLogValue::I64(n)
        } else if let Some(n) = value.to_u64() {
            OwnedLogValue::U64(n)
        } else if let Some(f) = value.to_f64() {
            OwnedLogValue::F64(f)
        } else {
            let mut s = String::new();
            let _ = write!(&mut s, "{value}");
            OwnedLogValue::Str(s)
        };
        self.fields.push(OwnedLogField {
            key: key.as_str().to_owned(),
            value: owned,
        });
        Ok(())
    }
}

impl log::Log for UnifiedStdLogBridge {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        logger::core().enabled(LogTarget::Main, map_level(metadata.level()))
    }

    fn log(&self, record: &log::Record<'_>) {
        let level = map_level(record.level());
        let core = logger::core();
        if !core.enabled(LogTarget::Main, level) {
            return;
        }
        let msg = record.args().to_string();
        let mut collector = KvCollector { fields: Vec::new() };
        let _ = record.key_values().visit(&mut collector);
        let borrowed: Vec<LogField<'_>> = collector
            .fields
            .iter()
            .map(OwnedLogField::as_field)
            .collect();
        core.emit_with_bound(
            &LogRecord {
                target: LogTarget::Main,
                level,
                logger_name: "",
                file: record.file().unwrap_or(""),
                line: record.line().unwrap_or(0),
                msg: &msg,
                fields: &borrowed,
            },
            &[],
        );
    }

    fn flush(&self) {
        let _ = logger::sync();
    }
}
