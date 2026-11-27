//! Unified JSON log encoder aligned with Go `pingcap/log` + `zapcore.NewJSONEncoder`.
//!
//! Key order:
//! `"level"`, `"time"` (omitted if `disable_timestamp`), `"name"` (when non-empty), `"caller"`
//! (when enabled), `"message"` (always present, even `""`), bound fields, call-site fields,
//! `"stack"` (when non-empty).
//!
//! Known differences from Go v8.5.8 (approved by owner 2026-11-27):
//! - Ruling R33 (#453): JSON encoder honors `disable_error_verbose` identically to text encoder
//!   (still emits `"<key>Verbose"` when `force_error_verbose = true` / `ErrVerboseLogger`).
//! - Ruling R34 (#454): `LogValue::Skip` is silently omitted.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use tidb_base::gostd::strconv::append_float;
use tidb_base::gostd::time::duration_string;
use tidb_base::structured_log::{LogField, LogValue};

use super::text::safe_add_bytes;
use super::{EncoderConfig, Entry, append_filtered_caller, append_timestamp, log_time_to_datetime};

fn append_json_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    // JSON encoder does NOT HTML-escape `<`, `>`, `&`, and escapes every invalid UTF-8 byte as
    // `\ufffd` — identical to `safe_add_bytes`.
    safe_add_bytes(out, bytes);
    out.push(b'"');
}

fn append_json_float(out: &mut Vec<u8>, f: f64, bit_size: u8) {
    if f.is_nan() {
        out.extend_from_slice(b"\"NaN\"");
    } else if f.is_infinite() {
        if f.is_sign_positive() {
            out.extend_from_slice(b"\"+Inf\"");
        } else {
            out.extend_from_slice(b"\"-Inf\"");
        }
    } else {
        append_float(out, f, b'f', -1, bit_size);
    }
}

/// Encode a full log entry into `out` as a single JSON object terminated by `\n`, with zero raw
/// `\n` or `\r` bytes inside the line.
// glue: zapcore.jsonEncoder.EncodeEntry with R33 (#453) disable_error_verbose fix
pub fn encode_entry(cfg: EncoderConfig, entry: &Entry<'_>, out: &mut Vec<u8>) {
    out.push(b'{');

    // 1. "level" (first)
    out.extend_from_slice(b"\"level\":\"");
    out.extend_from_slice(entry.level.as_upper_str().as_bytes());
    out.push(b'"');

    // 2. "time" (omitted when disable_timestamp = true)
    if !cfg.disable_timestamp {
        out.extend_from_slice(b",\"time\":\"");
        append_timestamp(out, entry.time);
        out.push(b'"');
    }

    // 3. "name" (when non-empty)
    if !entry.logger_name.is_empty() {
        out.extend_from_slice(b",\"name\":");
        append_json_string(out, entry.logger_name.as_bytes());
    }

    // 4. "caller" (when enabled and defined)
    if !cfg.disable_caller && !entry.file.is_empty() {
        out.extend_from_slice(b",\"caller\":\"");
        append_filtered_caller(out, entry.file, entry.line);
        out.push(b'"');
    }

    // 5. "message" (always present, even when empty)
    out.extend_from_slice(b",\"message\":");
    append_json_string(out, entry.msg.as_bytes());

    // 6. Bound fields
    for field in entry.bound_fields {
        encode_field(cfg, field, out);
    }

    // 7. Call-site fields
    for field in entry.fields {
        encode_field(cfg, field, out);
    }

    // 8. "stack" (when non-empty)
    if !entry.stack.is_empty() {
        out.extend_from_slice(b",\"stack\":");
        append_json_string(out, entry.stack.as_bytes());
    }

    out.extend_from_slice(b"}\n");
}

fn encode_field(cfg: EncoderConfig, field: &LogField<'_>, out: &mut Vec<u8>) {
    if matches!(field.value, LogValue::Skip) {
        return;
    }

    if let LogValue::Error(err) = field.value {
        let raw = err.error();
        out.push(b',');
        append_json_string(out, field.key.as_bytes());
        out.push(b':');
        append_json_string(out, &raw);

        // Ruling R33 (#453): JSON honors disable_error_verbose consistently with text.
        if (!cfg.disable_error_verbose || cfg.force_error_verbose) && err.has_stack() {
            let verbose = err.error_verbose();
            if verbose != raw {
                out.push(b',');
                let mut vkey = Vec::with_capacity(field.key.len() + 7);
                vkey.extend_from_slice(field.key.as_bytes());
                vkey.extend_from_slice(b"Verbose");
                append_json_string(out, &vkey);
                out.push(b':');
                append_json_string(out, &verbose);
            }
        }
        return;
    }

    out.push(b',');
    append_json_string(out, field.key.as_bytes());
    out.push(b':');
    encode_value(field.value, out);
}

fn encode_value(value: LogValue<'_>, out: &mut Vec<u8>) {
    match value {
        LogValue::Null => out.extend_from_slice(b"null"),
        LogValue::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        LogValue::I64(n) => {
            let mut b = itoa::Buffer::new();
            out.extend_from_slice(b.format(n).as_bytes());
        }
        LogValue::U64(n) => {
            let mut b = itoa::Buffer::new();
            out.extend_from_slice(b.format(n).as_bytes());
        }
        // JSON mode uses bitSize = 32 for Float32 (both scalars and arrays).
        LogValue::F32(f) => append_json_float(out, f64::from(f), 32),
        LogValue::F64(f) => append_json_float(out, f, 64),
        LogValue::Str(s) => append_json_string(out, s.as_bytes()),
        LogValue::Bytes(b) => append_json_string(out, b),
        LogValue::Binary(b) => {
            let encoded = BASE64_STANDARD.encode(b);
            append_json_string(out, encoded.as_bytes());
        }
        LogValue::Duration(d) => append_json_string(out, duration_string(d).as_bytes()),
        LogValue::Time(t) => {
            out.push(b'"');
            append_timestamp(out, log_time_to_datetime(t));
            out.push(b'"');
        }
        LogValue::Error(e) => append_json_string(out, &e.error()),
        LogValue::Strings(xs) => {
            out.push(b'[');
            for (i, s) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                append_json_string(out, s.as_bytes());
            }
            out.push(b']');
        }
        LogValue::I64s(xs) => {
            out.push(b'[');
            let mut itoa_buf = itoa::Buffer::new();
            for (i, &n) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(itoa_buf.format(n).as_bytes());
            }
            out.push(b']');
        }
        LogValue::U64s(xs) => {
            out.push(b'[');
            let mut itoa_buf = itoa::Buffer::new();
            for (i, &n) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(itoa_buf.format(n).as_bytes());
            }
            out.push(b']');
        }
        LogValue::Bools(xs) => {
            out.push(b'[');
            for (i, &b) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(if b { b"true" } else { b"false" });
            }
            out.push(b']');
        }
        LogValue::F32s(xs) => {
            out.push(b'[');
            for (i, &f) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                append_json_float(out, f64::from(f), 32);
            }
            out.push(b']');
        }
        LogValue::F64s(xs) => {
            out.push(b'[');
            for (i, &f) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                append_json_float(out, f, 64);
            }
            out.push(b']');
        }
        LogValue::Times(xs) => {
            out.push(b'[');
            for (i, &t) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.push(b'"');
                append_timestamp(out, log_time_to_datetime(t));
                out.push(b'"');
            }
            out.push(b']');
        }
        LogValue::JsonBytes(b) => {
            if serde_json::from_slice::<serde_json::Value>(b).is_ok()
                && !b.contains(&b'\n')
                && !b.contains(&b'\r')
            {
                out.extend_from_slice(b);
            } else {
                append_json_string(out, b);
            }
        }
        LogValue::Skip => {}
    }
}
