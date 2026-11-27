//! Direct `slog-global` bridge into `UnifiedLoggerCore` for `third_party/tikv` and
//! `third_party/tikv-shims` (3 call sites).
//!
//! `slog::OwnedKVList` stores logger-bound fields in prepend order (newest first). `BridgeDrain`
//! reverses collected bound fields so Go `.With(...)` declaration order is preserved before
//! call-site `record.kv()`.
//!
//! CRITICAL: NEVER call `slog_global::redirect_std_log`!

use std::fmt;

use slog::{Drain, KV, Level, OwnedKVList, Record, Serializer};
use tidb_base::structured_log::{LogField, LogLevel, LogRecord, LogTarget, StructuredLogger};

use crate::encoder::{OwnedLogField, OwnedLogValue};
use crate::logger;

/// `slog::Drain` bridging `slog-global` records directly to [`crate::logger::UnifiedLoggerCore`].
pub struct BridgeDrain;

/// Install `BridgeDrain` as the `slog-global` logger.
///
/// Intentionally does NOT call `slog_global::redirect_std_log` (which conflicts with
/// `log::set_logger`).
pub fn install() {
    let root = slog::Logger::root(BridgeDrain, slog::o!());
    slog_global::set_global(root);
}

fn map_level(level: Level) -> LogLevel {
    match level {
        Level::Trace | Level::Debug => LogLevel::Debug,
        Level::Info => LogLevel::Info,
        Level::Warning => LogLevel::Warn,
        Level::Error => LogLevel::Error,
        Level::Critical => LogLevel::Fatal,
    }
}

struct SlogCollector {
    fields: Vec<OwnedLogField>,
}

impl Serializer for SlogCollector {
    fn emit_arguments(&mut self, key: slog::Key, val: &fmt::Arguments<'_>) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::Str(val.to_string()),
        });
        Ok(())
    }

    fn emit_none(&mut self, key: slog::Key) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::Null,
        });
        Ok(())
    }

    fn emit_usize(&mut self, key: slog::Key, val: usize) -> slog::Result {
        self.emit_u64(key, val as u64)
    }

    fn emit_isize(&mut self, key: slog::Key, val: isize) -> slog::Result {
        self.emit_i64(key, val as i64)
    }

    fn emit_u8(&mut self, key: slog::Key, val: u8) -> slog::Result {
        self.emit_u64(key, u64::from(val))
    }

    fn emit_i8(&mut self, key: slog::Key, val: i8) -> slog::Result {
        self.emit_i64(key, i64::from(val))
    }

    fn emit_u16(&mut self, key: slog::Key, val: u16) -> slog::Result {
        self.emit_u64(key, u64::from(val))
    }

    fn emit_i16(&mut self, key: slog::Key, val: i16) -> slog::Result {
        self.emit_i64(key, i64::from(val))
    }

    fn emit_u32(&mut self, key: slog::Key, val: u32) -> slog::Result {
        self.emit_u64(key, u64::from(val))
    }

    fn emit_i32(&mut self, key: slog::Key, val: i32) -> slog::Result {
        self.emit_i64(key, i64::from(val))
    }

    fn emit_f32(&mut self, key: slog::Key, val: f32) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::F32(val),
        });
        Ok(())
    }

    fn emit_bool(&mut self, key: slog::Key, val: bool) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::Bool(val),
        });
        Ok(())
    }

    fn emit_i64(&mut self, key: slog::Key, val: i64) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::I64(val),
        });
        Ok(())
    }

    fn emit_u64(&mut self, key: slog::Key, val: u64) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::U64(val),
        });
        Ok(())
    }

    fn emit_f64(&mut self, key: slog::Key, val: f64) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::F64(val),
        });
        Ok(())
    }

    fn emit_str(&mut self, key: slog::Key, val: &str) -> slog::Result {
        self.fields.push(OwnedLogField {
            key: key.to_owned(),
            value: OwnedLogValue::Str(val.to_owned()),
        });
        Ok(())
    }
}

impl Drain for BridgeDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(&self, record: &Record<'_>, values: &OwnedKVList) -> Result<Self::Ok, Self::Err> {
        let level = map_level(record.level());
        let core = logger::core();
        if !core.enabled(LogTarget::Main, level) {
            return Ok(());
        }

        let mut bound = SlogCollector { fields: Vec::new() };
        let _ = values.serialize(record, &mut bound);
        // slog::OwnedKVList is a prepended linked list; reverse to restore Go .With order.
        bound.fields.reverse();

        let mut call = SlogCollector { fields: Vec::new() };
        let _ = record.kv().serialize(record, &mut call);
        // slog::b! / slog::kv! also builds a prepended list (newest first); reverse to restore
        // declaration order.
        call.fields.reverse();

        let bound_fields: Vec<LogField<'_>> =
            bound.fields.iter().map(OwnedLogField::as_field).collect();
        let call_fields: Vec<LogField<'_>> =
            call.fields.iter().map(OwnedLogField::as_field).collect();
        let msg = record.msg().to_string();

        core.emit_with_bound(
            &LogRecord {
                target: LogTarget::Main,
                level,
                logger_name: "",
                file: record.file(),
                line: record.line(),
                msg: &msg,
                fields: &call_fields,
            },
            &bound_fields,
        );
        Ok(())
    }
}
