//! Zero-dependency structured logging facade for tidb-rs (rank 0, N0 §4 rule 3).
//!
//! Every layered crate (ranks 0..=16) emits structured logs through this facade without
//! depending on `tidb-log` (rank 1). At startup `tidb_log::init_logger` registers the unified
//! backend via [`register_logger`]. Before initialization (e.g. warnings emitted during
//! `Config::valid()`), records fall back to a minimal synchronous single-line stdout encoder
//! matching Go's default `pingcap/log` stdout logger so pre-init warnings are never dropped.

use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::errors::Error;
use crate::gostd::strconv::append_float;
use crate::gostd::time::{Duration, duration_string};

/// Log severity levels aligned with Go `zapcore.Level`.
///
/// Rust's `log::Level::Trace` maps to [`LogLevel::Debug`] at the bridge so no non-Go `[TRACE]`
/// label is emitted into unified logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
    DPanic = 4,
    Panic = 5,
    Fatal = 6,
}

impl LogLevel {
    /// Uppercase level string matching Go `zapcore.CapitalLevelEncoder`.
    // glue: zapcore.CapitalLevelEncoder
    pub const fn as_upper_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::DPanic => "DPANIC",
            LogLevel::Panic => "PANIC",
            LogLevel::Fatal => "FATAL",
        }
    }

    /// Lowercase level string.
    // glue: zapcore.Level.String
    pub const fn as_lower_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::DPanic => "dpanic",
            LogLevel::Panic => "panic",
            LogLevel::Fatal => "fatal",
        }
    }

    /// Parse a level name case-insensitively (matching Go `zapcore.Level.UnmarshalText`).
    // glue: zapcore.Level.UnmarshalText
    pub fn parse_level(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "debug" => Ok(LogLevel::Debug),
            "info" | "" => Ok(LogLevel::Info),
            "warn" => Ok(LogLevel::Warn),
            "error" => Ok(LogLevel::Error),
            "dpanic" => Ok(LogLevel::DPanic),
            "panic" => Ok(LogLevel::Panic),
            "fatal" => Ok(LogLevel::Fatal),
            _ => Err(Error::plain(format!("unrecognized level: \"{s}\""))),
        }
    }

    /// Convert from raw `u8` representation.
    // glue: atomic level decoding
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => LogLevel::Debug,
            1 => LogLevel::Info,
            2 => LogLevel::Warn,
            3 => LogLevel::Error,
            4 => LogLevel::DPanic,
            5 => LogLevel::Panic,
            _ => LogLevel::Fatal,
        }
    }
}

/// Destination logger routing tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LogTarget {
    /// Main unified logger (`BgLogger` / `Logger(ctx)`).
    Main,
    /// Logger that forces `[errorVerbose=...]` regardless of `disable-error-stack`.
    ErrVerbose,
    /// General query log (`GeneralLogger`: always logs at INFO, routes to `general-log-file` if set).
    General,
    /// Fatal logger (`terror::must_nil`, unhandled panic hook).
    Fatal,
}

/// Timestamp value preserving its own fixed UTC offset (Go `time.Time` field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogTime {
    pub secs: i64,
    pub nanos: u32,
    pub offset_secs: i32,
}

impl LogTime {
    /// Construct a [`LogTime`] from Unix `(secs, nanos)` using the process's local time zone
    /// offset (Go `time.Unix(secs, nanos)` / `oracle.GetTimeFromTS` semantics).
    // glue: local-timezone LogTime from Unix timestamp
    pub fn from_local_unix(secs: i64, nanos: u32) -> Self {
        let (_, offset_secs) = crate::gostd::time::unix(secs, i64::from(nanos)).zone();
        Self {
            secs,
            nanos,
            offset_secs,
        }
    }
}

/// Typed structured field value preserving exact Go / `zap` semantics (including raw non-UTF-8
/// bytes and `&tidb_base::Error`).
#[derive(Clone, Copy, Debug)]
pub enum LogValue<'a> {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F32(f32),
    F64(f64),
    Str(&'a str),
    Bytes(&'a [u8]),
    Binary(&'a [u8]),
    Duration(Duration),
    Time(LogTime),
    Error(&'a Error),
    Strings(&'a [&'a str]),
    I64s(&'a [i64]),
    U64s(&'a [u64]),
    Bools(&'a [bool]),
    F32s(&'a [f32]),
    F64s(&'a [f64]),
    Times(&'a [LogTime]),
    /// Pre-serialized JSON bytes (for `Reflect` / `Object` values).
    JsonBytes(&'a [u8]),
    /// Explicitly skipped field (`zap.Skip()` / `zap.Error(nil)`): silently omitted per R34 (#454).
    Skip,
}

/// Conversions into [`LogValue`] for the `log_*!` macros.
pub trait IntoLogValue<'a> {
    // glue: ergonomic field conversion for structured_log macros
    fn into_log_value(self) -> LogValue<'a>;
}

impl<'a> IntoLogValue<'a> for LogValue<'a> {
    fn into_log_value(self) -> LogValue<'a> {
        self
    }
}
impl<'a> IntoLogValue<'a> for bool {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Bool(self)
    }
}
impl<'a> IntoLogValue<'a> for i8 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::I64(i64::from(self))
    }
}
impl<'a> IntoLogValue<'a> for i16 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::I64(i64::from(self))
    }
}
impl<'a> IntoLogValue<'a> for i32 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::I64(i64::from(self))
    }
}
impl<'a> IntoLogValue<'a> for i64 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::I64(self)
    }
}
impl<'a> IntoLogValue<'a> for isize {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::I64(self as i64)
    }
}
impl<'a> IntoLogValue<'a> for u8 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::U64(u64::from(self))
    }
}
impl<'a> IntoLogValue<'a> for u16 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::U64(u64::from(self))
    }
}
impl<'a> IntoLogValue<'a> for u32 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::U64(u64::from(self))
    }
}
impl<'a> IntoLogValue<'a> for u64 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::U64(self)
    }
}
impl<'a> IntoLogValue<'a> for usize {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::U64(self as u64)
    }
}
impl<'a> IntoLogValue<'a> for f32 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::F32(self)
    }
}
impl<'a> IntoLogValue<'a> for f64 {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::F64(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a str {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Str(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a String {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Str(self.as_str())
    }
}
impl<'a> IntoLogValue<'a> for &'a [u8] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Bytes(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a Vec<u8> {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Bytes(self.as_slice())
    }
}
impl<'a> IntoLogValue<'a> for &'a Error {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Error(self)
    }
}
impl<'a> IntoLogValue<'a> for Option<&'a Error> {
    fn into_log_value(self) -> LogValue<'a> {
        match self {
            Some(e) => LogValue::Error(e),
            None => LogValue::Skip,
        }
    }
}
impl<'a> IntoLogValue<'a> for std::time::Duration {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Duration(self.as_nanos().min(i64::MAX as u128) as i64)
    }
}
impl<'a> IntoLogValue<'a> for LogTime {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Time(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a [&'a str] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Strings(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a [i64] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::I64s(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a [u64] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::U64s(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a [bool] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Bools(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a [f32] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::F32s(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a [f64] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::F64s(self)
    }
}
impl<'a> IntoLogValue<'a> for &'a [LogTime] {
    fn into_log_value(self) -> LogValue<'a> {
        LogValue::Times(self)
    }
}

/// A single key/value pair emitted on a log line.
#[derive(Clone, Copy, Debug)]
pub struct LogField<'a> {
    pub key: &'a str,
    pub value: LogValue<'a>,
}

impl<'a> LogField<'a> {
    // glue: field constructor
    pub fn new(key: &'a str, value: impl IntoLogValue<'a>) -> Self {
        Self {
            key,
            value: value.into_log_value(),
        }
    }
}

/// A single structured log record passed to [`StructuredLogger::log`].
#[derive(Clone, Copy, Debug)]
pub struct LogRecord<'a> {
    pub target: LogTarget,
    pub level: LogLevel,
    pub logger_name: &'a str,
    pub file: &'a str,
    pub line: u32,
    pub msg: &'a str,
    pub fields: &'a [LogField<'a>],
}

/// Global structured logging backend implemented by `tidb-log` (rank 1) and registered once at
/// startup (N0 §4 rule 3).
pub trait StructuredLogger: Send + Sync {
    /// Returns whether a record at `(target, level)` would be emitted.
    fn enabled(&self, target: LogTarget, level: LogLevel) -> bool;

    /// Returns the current global log level threshold.
    fn level(&self) -> LogLevel;

    /// Formats and writes a single structured log record.
    fn log(&self, record: &LogRecord<'_>);

    /// Flushes any buffered sinks to disk / stdout.
    fn sync(&self) -> io::Result<()>;
}

static GLOBAL_LOGGER: OnceLock<&'static dyn StructuredLogger> = OnceLock::new();
static FALLBACK_LOGGER: FallbackStdoutLogger = FallbackStdoutLogger {
    level: AtomicU8::new(LogLevel::Info as u8),
};

/// Register the process-wide structured logger backend (`tidb-log`).
///
/// Returns `true` if registered, or `false` if a logger was already registered.
// glue: N0 §4 rule 3 OnceLock global registration
pub fn register_logger(logger: &'static dyn StructuredLogger) -> bool {
    GLOBAL_LOGGER.set(logger).is_ok()
}

/// Returns whether a dedicated backend (`tidb-log`) has been registered.
// glue: check if init_logger has run
pub fn is_registered() -> bool {
    GLOBAL_LOGGER.get().is_some()
}

/// Returns the active global [`StructuredLogger`], falling back to the pre-init synchronous
/// single-line stdout encoder if `register_logger` has not yet been called.
// glue: global logger accessor with pre-init stdout fallback
pub fn logger() -> &'static dyn StructuredLogger {
    match GLOBAL_LOGGER.get() {
        Some(l) => *l,
        None => &FALLBACK_LOGGER,
    }
}

/// Emit a structured log record through the active global logger.
// glue: dispatch helper for log_*! macros
pub fn emit(
    target: LogTarget,
    level: LogLevel,
    logger_name: &str,
    file: &str,
    line: u32,
    msg: &str,
    fields: &[LogField<'_>],
) {
    let l = logger();
    if l.enabled(target, level) {
        l.log(&LogRecord {
            target,
            level,
            logger_name,
            file,
            line,
            msg,
            fields,
        });
    }
}

/// Minimal synchronous single-line stdout logger active before `tidb_log::init_logger` runs.
/// Matches Go's default `pingcap/log` stdout logger (`pingcap/log/log.go:37-41`) so warnings from
/// `Config::valid()` / `initialize_config()` are never dropped.
struct FallbackStdoutLogger {
    level: AtomicU8,
}

impl StructuredLogger for FallbackStdoutLogger {
    fn enabled(&self, target: LogTarget, level: LogLevel) -> bool {
        if matches!(target, LogTarget::General | LogTarget::Fatal) {
            return true;
        }
        (level as u8) >= self.level.load(Ordering::Relaxed)
    }

    fn level(&self) -> LogLevel {
        LogLevel::from_u8(self.level.load(Ordering::Relaxed))
    }

    fn log(&self, record: &LogRecord<'_>) {
        // Align with Go `pingcap/log` default logger (`init()` in pingcap/log/log.go:37-41):
        // timestamp + level + caller + message + fields (with `errorVerbose` when error has a
        // stack, since default `DisableErrorVerbose == false`) + automatic stack at ERROR+.
        let mut buf = Vec::with_capacity(192);
        let now = now_utc_log_time();
        buf.push(b'[');
        append_log_time(&mut buf, now);
        buf.push(b']');
        buf.push(b' ');
        buf.push(b'[');
        buf.extend_from_slice(record.level.as_upper_str().as_bytes());
        buf.push(b']');
        if !record.logger_name.is_empty() {
            buf.push(b' ');
            buf.push(b'[');
            append_fallback_bytes(&mut buf, record.logger_name.as_bytes());
            buf.push(b']');
        }
        if !record.file.is_empty() {
            buf.push(b' ');
            buf.push(b'[');
            append_filtered_caller(&mut buf, record.file, record.line);
            buf.push(b']');
        }
        if !record.msg.is_empty() {
            buf.push(b' ');
            buf.push(b'[');
            append_fallback_bytes(&mut buf, record.msg.as_bytes());
            buf.push(b']');
        }
        for f in record.fields {
            if matches!(f.value, LogValue::Skip) {
                continue;
            }
            buf.push(b' ');
            buf.push(b'[');
            append_fallback_bytes(&mut buf, f.key.as_bytes());
            buf.push(b'=');
            append_fallback_value(&mut buf, f.value);
            buf.push(b']');
            if let LogValue::Error(e) = f.value
                && e.has_stack()
            {
                let verbose = e.error_verbose();
                let mut vkey = String::with_capacity(f.key.len() + 7);
                vkey.push_str(f.key);
                vkey.push_str("Verbose");
                buf.push(b' ');
                buf.push(b'[');
                append_fallback_bytes(&mut buf, vkey.as_bytes());
                buf.push(b'=');
                append_fallback_bytes(&mut buf, &verbose);
                buf.push(b']');
            }
        }
        if (record.level as u8) >= (LogLevel::Error as u8) {
            let stack =
                crate::errors::format_backtrace_string(&std::backtrace::Backtrace::force_capture());
            if !stack.is_empty() {
                buf.push(b' ');
                buf.push(b'[');
                buf.extend_from_slice(b"stack=");
                append_fallback_bytes(&mut buf, stack.as_bytes());
                buf.push(b']');
            }
        }
        buf.push(b'\n');
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(&buf);
    }

    fn sync(&self) -> io::Result<()> {
        io::stdout().flush()
    }
}

fn now_utc_log_time() -> LogTime {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    LogTime {
        secs: dur.as_secs() as i64,
        nanos: dur.subsec_nanos(),
        offset_secs: 0,
    }
}

/// Howard Hinnant civil-from-days algorithm: format `LogTime` as
/// `YYYY/MM/DD HH:MM:SS.mmm ±HH:MM` with zero external crate dependencies.
fn append_log_time(buf: &mut Vec<u8>, t: LogTime) {
    let adjusted = t.secs.saturating_add(i64::from(t.offset_secs));
    let days = adjusted.div_euclid(86_400);
    let secs_of_day = adjusted.rem_euclid(86_400);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    let millis = t.nanos / 1_000_000;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let sign = if t.offset_secs < 0 { '-' } else { '+' };
    let abs_off = t.offset_secs.unsigned_abs();
    let off_h = abs_off / 3600;
    let off_m = (abs_off % 3600) / 60;
    let _ = write!(
        buf,
        "{y:04}/{m:02}/{d:02} {hh:02}:{mm:02}:{ss:02}.{millis:03} {sign}{off_h:02}:{off_m:02}"
    );
}

fn encode_base64_std(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < input.len() {
        let b0 = input[i];
        let b1 = if i + 1 < input.len() { input[i + 1] } else { 0 };
        let b2 = if i + 2 < input.len() { input[i + 2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < input.len() {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < input.len() {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn append_filtered_caller(buf: &mut Vec<u8>, file: &str, line: u32) {
    let base = file.rsplit('/').next().unwrap_or(file);
    for &b in base.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            buf.push(b);
        }
    }
    buf.push(b':');
    let _ = write!(buf, "{line}");
}

fn need_quotes(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|&b| b <= 0x20 || matches!(b, b'\\' | b'"' | b'[' | b']' | b'='))
}

fn append_fallback_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    let quote = need_quotes(bytes);
    if quote {
        buf.push(b'"');
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            match b {
                b'\\' => buf.extend_from_slice(b"\\\\"),
                b'"' => buf.extend_from_slice(b"\\\""),
                b'\n' => buf.extend_from_slice(b"\\n"),
                b'\r' => buf.extend_from_slice(b"\\r"),
                b'\t' => buf.extend_from_slice(b"\\t"),
                0x00..=0x1f => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    buf.extend_from_slice(b"\\u00");
                    buf.push(HEX[(b >> 4) as usize]);
                    buf.push(HEX[(b & 0x0f) as usize]);
                }
                _ => buf.push(b),
            }
            i += 1;
        } else {
            match std::str::from_utf8(&bytes[i..]) {
                Ok(s) => {
                    let ch = s.chars().next().unwrap();
                    let len = ch.len_utf8();
                    buf.extend_from_slice(&bytes[i..i + len]);
                    i += len;
                }
                Err(e) if e.valid_up_to() > 0 => {
                    let n = e.valid_up_to();
                    buf.extend_from_slice(&bytes[i..i + n]);
                    i += n;
                }
                Err(_) => {
                    buf.extend_from_slice(b"\\ufffd");
                    i += 1;
                }
            }
        }
    }
    if quote {
        buf.push(b'"');
    }
}

fn append_fallback_value(buf: &mut Vec<u8>, v: LogValue<'_>) {
    match v {
        LogValue::Null => buf.extend_from_slice(b"null"),
        LogValue::Bool(b) => buf.extend_from_slice(if b { b"true" } else { b"false" }),
        LogValue::I64(n) => {
            let _ = write!(buf, "{n}");
        }
        LogValue::U64(n) => {
            let _ = write!(buf, "{n}");
        }
        LogValue::F32(f) => append_float(buf, f64::from(f), b'f', -1, 64),
        LogValue::F64(f) => append_float(buf, f, b'f', -1, 64),
        LogValue::Str(s) => append_fallback_bytes(buf, s.as_bytes()),
        LogValue::Bytes(b) | LogValue::JsonBytes(b) => append_fallback_bytes(buf, b),
        LogValue::Binary(b) => {
            let encoded = encode_base64_std(b);
            append_fallback_bytes(buf, encoded.as_bytes());
        }
        LogValue::Duration(d) => buf.extend_from_slice(duration_string(d).as_bytes()),
        LogValue::Time(t) => {
            let mut ts_buf = Vec::with_capacity(32);
            append_log_time(&mut ts_buf, t);
            append_fallback_bytes(buf, &ts_buf);
        }
        LogValue::Error(e) => {
            let raw = e.error();
            append_fallback_bytes(buf, &raw);
        }
        LogValue::Strings(xs) => {
            let joined = format!("[{}]", xs.join(","));
            append_fallback_bytes(buf, joined.as_bytes());
        }
        LogValue::I64s(xs) => {
            use std::fmt::Write as _;
            let mut s = String::from("[");
            for (i, n) in xs.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let _ = write!(s, "{n}");
            }
            s.push(']');
            append_fallback_bytes(buf, s.as_bytes());
        }
        LogValue::U64s(xs) => {
            let s = format!("{xs:?}");
            append_fallback_bytes(buf, s.as_bytes());
        }
        LogValue::Bools(xs) => {
            let s = format!("{xs:?}");
            append_fallback_bytes(buf, s.as_bytes());
        }
        LogValue::F32s(xs) => {
            let s = format!("{xs:?}");
            append_fallback_bytes(buf, s.as_bytes());
        }
        LogValue::F64s(xs) => {
            let s = format!("{xs:?}");
            append_fallback_bytes(buf, s.as_bytes());
        }
        LogValue::Times(xs) => {
            let s = format!("{xs:?}");
            append_fallback_bytes(buf, s.as_bytes());
        }
        LogValue::Skip => {}
    }
}

/// Emit a structured log record to an explicit [`LogTarget`] and [`LogLevel`].
///
/// Supports arbitrary string-literal keys including spaces (e.g. `"connection ID"`,
/// `"stack trace"`, `"affected rows"`, `"listening on addr"`).
#[macro_export]
macro_rules! log_target {
    ($target:expr, $level:expr, $msg:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        let __logger = $crate::structured_log::logger();
        let __target = $target;
        let __level = $level;
        if __logger.enabled(__target, __level) {
            let __fields: &[$crate::structured_log::LogField<'_>] = &[
                $(
                    $crate::structured_log::LogField::new($k, $v),
                )*
            ];
            __logger.log(&$crate::structured_log::LogRecord {
                target: __target,
                level: __level,
                logger_name: "",
                file: file!(),
                line: line!(),
                msg: $msg,
                fields: __fields,
            });
        }
    }};
}

/// Emit a `DEBUG` structured log record to the main logger.
#[macro_export]
macro_rules! log_debug {
    ($msg:expr $(, $k:expr => $v:expr)* $(,)?) => {
        $crate::log_target!(
            $crate::structured_log::LogTarget::Main,
            $crate::structured_log::LogLevel::Debug,
            $msg $(, $k => $v)*
        )
    };
}

/// Emit an `INFO` structured log record to the main logger.
#[macro_export]
macro_rules! log_info {
    ($msg:expr $(, $k:expr => $v:expr)* $(,)?) => {
        $crate::log_target!(
            $crate::structured_log::LogTarget::Main,
            $crate::structured_log::LogLevel::Info,
            $msg $(, $k => $v)*
        )
    };
}

/// Emit a `WARN` structured log record to the main logger.
#[macro_export]
macro_rules! log_warn {
    ($msg:expr $(, $k:expr => $v:expr)* $(,)?) => {
        $crate::log_target!(
            $crate::structured_log::LogTarget::Main,
            $crate::structured_log::LogLevel::Warn,
            $msg $(, $k => $v)*
        )
    };
}

/// Emit an `ERROR` structured log record to the main logger.
#[macro_export]
macro_rules! log_error {
    ($msg:expr $(, $k:expr => $v:expr)* $(,)?) => {
        $crate::log_target!(
            $crate::structured_log::LogTarget::Main,
            $crate::structured_log::LogLevel::Error,
            $msg $(, $k => $v)*
        )
    };
}

/// Emit a `FATAL` structured log record to the fatal logger (does not exit the process).
#[macro_export]
macro_rules! log_fatal {
    ($msg:expr $(, $k:expr => $v:expr)* $(,)?) => {
        $crate::log_target!(
            $crate::structured_log::LogTarget::Fatal,
            $crate::structured_log::LogLevel::Fatal,
            $msg $(, $k => $v)*
        )
    };
}

/// Emit an `ERROR` structured log record via the `ErrVerboseLogger` target (forces
/// `[errorVerbose=...]` even when `disable-error-stack = true`).
#[macro_export]
macro_rules! log_err_verbose {
    ($level:expr, $msg:expr $(, $k:expr => $v:expr)* $(,)?) => {
        $crate::log_target!(
            $crate::structured_log::LogTarget::ErrVerbose,
            $level,
            $msg $(, $k => $v)*
        )
    };
}

/// Emit an `INFO` structured log record to the `GeneralLogger` target.
#[macro_export]
macro_rules! log_general {
    ($msg:expr $(, $k:expr => $v:expr)* $(,)?) => {
        $crate::log_target!(
            $crate::structured_log::LogTarget::General,
            $crate::structured_log::LogLevel::Info,
            $msg $(, $k => $v)*
        )
    };
}
