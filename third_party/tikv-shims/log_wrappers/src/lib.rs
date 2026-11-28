//! Shim of TiKV `log_wrappers`: hex-printing of keys/values with a zero-dependency
//! redaction provider hook wired at server startup (`D00.3` Batch 4).
use std::fmt;
use std::sync::OnceLock;

pub fn hex_encode_upper<T: AsRef<[u8]>>(data: T) -> String {
    hex::encode_upper(data)
}

/// Redaction mode read by [`Value`] when formatting keys/values for logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedactMode {
    Off,
    On,
    Marker,
}

static REDACT_PROVIDER: OnceLock<fn() -> RedactMode> = OnceLock::new();

/// Install a process-wide redaction mode provider (`poc-server` wires this to
/// `tidb_base::errors::redact_log_enabled_load()` at startup).
pub fn set_redact_provider(provider: fn() -> RedactMode) {
    let _ = REDACT_PROVIDER.set(provider);
}

fn current_redact_mode() -> RedactMode {
    REDACT_PROVIDER.get().map_or(RedactMode::Off, |f| f())
}

pub struct Value<'a>(pub &'a [u8]);
impl<'a> Value<'a> {
    pub fn key(key: &'a [u8]) -> Self {
        Value(key)
    }
    #[allow(clippy::self_named_constructors)]
    pub fn value(v: &'a [u8]) -> Self {
        Value(v)
    }
}

fn format_redacted(bytes: &[u8], mode: RedactMode, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match mode {
        RedactMode::Off => f.write_str(&hex_encode_upper(bytes)),
        RedactMode::On => f.write_str("?"),
        RedactMode::Marker => write!(f, "‹{}›", hex_encode_upper(bytes)),
    }
}

struct RedactedLogValue<'a>(&'a [u8]);

impl fmt::Display for RedactedLogValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_redacted(self.0, current_redact_mode(), f)
    }
}

impl slog::Value for Value<'_> {
    fn serialize(
        &self,
        _r: &slog::Record<'_>,
        key: slog::Key,
        s: &mut dyn slog::Serializer,
    ) -> slog::Result {
        // Apply the redaction provider only to structured log serialization (`slog::Value`),
        // keeping `Display`/`Debug` unredacted so vendored `tidb_query_*` error texts sent to
        // SQL clients are not altered by `tidb_redact_log`.
        s.emit_arguments(key, &format_args!("{}", RedactedLogValue(self.0)))
    }
}
impl fmt::Display for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex_encode_upper(self.0))
    }
}
impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU8, Ordering};

    use slog::Value as SlogValue;

    use super::*;

    static MODE: AtomicU8 = AtomicU8::new(0);

    fn test_provider() -> RedactMode {
        match MODE.load(Ordering::SeqCst) {
            1 => RedactMode::On,
            2 => RedactMode::Marker,
            _ => RedactMode::Off,
        }
    }

    struct CaptureSerializer(String);

    impl slog::Serializer for CaptureSerializer {
        fn emit_arguments(&mut self, _key: slog::Key, val: &fmt::Arguments<'_>) -> slog::Result {
            self.0 = val.to_string();
            Ok(())
        }
    }

    fn serialize_slog(v: Value<'_>) -> String {
        let mut ser = CaptureSerializer(String::new());
        let loc = slog::RecordLocation {
            file: file!(),
            line: line!(),
            column: column!(),
            function: "",
            module: module_path!(),
        };
        let rs = slog::RecordStatic {
            location: &loc,
            level: slog::Level::Info,
            tag: "",
        };
        let kv = slog::b!();
        let args = format_args!("");
        let rec = slog::Record::new(&rs, &args, kv);
        v.serialize(&rec, "k", &mut ser).unwrap();
        ser.0
    }

    #[test]
    fn slog_value_respects_redact_provider_while_display_and_hex_stay_raw() {
        set_redact_provider(test_provider);
        let raw = [0xde, 0xad, 0xbe, 0xef];

        MODE.store(0, Ordering::SeqCst);
        assert_eq!(serialize_slog(Value::key(&raw)), "DEADBEEF");
        assert_eq!(Value::key(&raw).to_string(), "DEADBEEF");
        assert_eq!(format!("{:?}", Value::value(&raw)), "DEADBEEF");
        assert_eq!(hex_encode_upper(raw), "DEADBEEF");

        MODE.store(1, Ordering::SeqCst);
        assert_eq!(serialize_slog(Value::key(&raw)), "?");
        assert_eq!(Value::key(&raw).to_string(), "DEADBEEF");
        assert_eq!(format!("{:?}", Value::value(&raw)), "DEADBEEF");
        assert_eq!(hex_encode_upper(raw), "DEADBEEF");

        MODE.store(2, Ordering::SeqCst);
        assert_eq!(serialize_slog(Value::key(&raw)), "‹DEADBEEF›");
        assert_eq!(Value::key(&raw).to_string(), "DEADBEEF");
        assert_eq!(format!("{:?}", Value::value(&raw)), "DEADBEEF");
        assert_eq!(hex_encode_upper(raw), "DEADBEEF");
    }
}
