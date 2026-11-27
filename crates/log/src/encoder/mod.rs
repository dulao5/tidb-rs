//! Unified text and JSON log encoders matching Go TiDB v8.5.8 (`pingcap/log` + `zap`).

pub mod json;
pub mod text;

use chrono::{DateTime, FixedOffset, Local, Timelike};
use tidb_base::structured_log::{LogField, LogLevel, LogTime};

/// Output encoding format (`log.format`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

impl LogFormat {
    /// Parse `log.format` (`""` and `"text"` -> `Text`, `"json"` -> `Json`, anything else errors).
    // glue: pingcap/log Format parsing
    pub fn parse(s: &str) -> Result<Self, tidb_base::Error> {
        match s {
            "" | "text" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            other => Err(tidb_base::Error::plain(format!(
                "unsupported log format: {other}"
            ))),
        }
    }
}

/// Configuration knobs shared by both text and JSON encoders.
#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    pub disable_timestamp: bool,
    pub disable_caller: bool,
    pub disable_error_verbose: bool,
    pub force_error_verbose: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            disable_timestamp: false,
            disable_caller: false,
            disable_error_verbose: true,
            force_error_verbose: false,
        }
    }
}

/// Owned field stored for bound `.With(...)` logger fields (e.g. `keyspaceName`, `conn`,
/// `session_alias`).
#[derive(Clone, Debug, PartialEq)]
pub enum OwnedLogValue {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F32(f32),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Binary(Vec<u8>),
    Duration(tidb_base::gostd::time::Duration),
    Time(LogTime),
    JsonBytes(Vec<u8>),
}

/// Owned bound field (`key`, `value`).
#[derive(Clone, Debug, PartialEq)]
pub struct OwnedLogField {
    pub key: String,
    pub value: OwnedLogValue,
}

impl OwnedLogField {
    // glue: owned bound field constructor
    pub fn string(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: OwnedLogValue::Str(value.into()),
        }
    }

    // glue: owned bound u64 field constructor
    pub fn u64(key: impl Into<String>, value: u64) -> Self {
        Self {
            key: key.into(),
            value: OwnedLogValue::U64(value),
        }
    }

    // glue: borrow as LogField for encoder input
    pub fn as_field(&self) -> LogField<'_> {
        use tidb_base::structured_log::LogValue;
        let value = match &self.value {
            OwnedLogValue::Null => LogValue::Null,
            OwnedLogValue::Bool(b) => LogValue::Bool(*b),
            OwnedLogValue::I64(n) => LogValue::I64(*n),
            OwnedLogValue::U64(n) => LogValue::U64(*n),
            OwnedLogValue::F32(f) => LogValue::F32(*f),
            OwnedLogValue::F64(f) => LogValue::F64(*f),
            OwnedLogValue::Str(s) => LogValue::Str(s.as_str()),
            OwnedLogValue::Bytes(b) => LogValue::Bytes(b.as_slice()),
            OwnedLogValue::Binary(b) => LogValue::Binary(b.as_slice()),
            OwnedLogValue::Duration(d) => LogValue::Duration(*d),
            OwnedLogValue::Time(t) => LogValue::Time(*t),
            OwnedLogValue::JsonBytes(b) => LogValue::JsonBytes(b.as_slice()),
        };
        LogField {
            key: self.key.as_str(),
            value,
        }
    }
}

/// Complete entry passed into [`text::encode_entry`] or [`json::encode_entry`].
pub struct Entry<'a> {
    /// Entry timestamp with explicit fixed offset (local timezone for runtime logs).
    pub time: DateTime<FixedOffset>,
    pub level: LogLevel,
    pub logger_name: &'a str,
    pub file: &'a str,
    pub line: u32,
    pub msg: &'a str,
    pub bound_fields: &'a [LogField<'a>],
    pub fields: &'a [LogField<'a>],
    /// Optional explicit stack trace appended at the end (e.g. `AddStacktrace(FatalLevel)`).
    pub stack: &'a str,
}

/// Current local time as `DateTime<FixedOffset>`.
// glue: runtime log entry timestamp in process local timezone
pub fn now_local_fixed() -> DateTime<FixedOffset> {
    Local::now().fixed_offset()
}

/// Format a `DateTime<FixedOffset>` using Go layout `"2006/01/02 15:04:05.000 -07:00"`,
/// **truncating** sub-second nanoseconds toward zero (never rounding: `.999999999` -> `.999`).
// glue: pingcap/log DefaultTimeEncoder layout with truncated milliseconds
pub fn append_timestamp(buf: &mut Vec<u8>, dt: DateTime<FixedOffset>) {
    use chrono::Datelike;
    use std::io::Write as _;
    let millis = dt.nanosecond() / 1_000_000;
    let offset_secs = dt.offset().local_minus_utc();
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs_secs = offset_secs.unsigned_abs();
    let off_h = abs_secs / 3600;
    let off_m = (abs_secs % 3600) / 60;
    let _ = write!(
        buf,
        "{:04}/{:02}/{:02} {:02}:{:02}:{:02}.{:03} {sign}{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second().min(59),
        millis,
        off_h,
        off_m,
    );
}

/// Convert [`LogTime`] into a `DateTime<FixedOffset>` preserving its own fixed UTC offset.
/// Falls back to Go's zero time `0001/01/01 00:00:00.000 +00:00` if out of range.
// glue: LogTime -> chrono DateTime<FixedOffset>
pub fn log_time_to_datetime(t: LogTime) -> DateTime<FixedOffset> {
    let offset = FixedOffset::east_opt(t.offset_secs)
        .unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC offset"));
    DateTime::from_timestamp(t.secs, t.nanos)
        .map(|utc| utc.with_timezone(&offset))
        .unwrap_or_else(|| {
            DateTime::from_timestamp(-62_135_596_800, 0)
                .expect("Go zero time")
                .with_timezone(&offset)
        })
}

/// Go `pingcap/log` `ShortCallerEncoder` (`zap_text_encoder.go:68-94`): basename after last `/`,
/// filtering bytes strictly to ASCII `[A-Za-z0-9._-]`, followed by `:<line>`.
// go-dep: github.com/pingcap/log@v1.1.1-0.20250917021125-19901e015dc9.ShortCallerEncoder L1
pub fn append_filtered_caller(buf: &mut Vec<u8>, file: &str, line: u32) {
    let base = file.rsplit('/').next().unwrap_or(file);
    for &b in base.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            buf.push(b);
        }
    }
    buf.push(b':');
    let mut itoa_buf = itoa::Buffer::new();
    buf.extend_from_slice(itoa_buf.format(line).as_bytes());
}
