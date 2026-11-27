//! Byte-exact port of Go `pingcap/log` text encoder (`zap_text_encoder.go`).
//!
//! go-file: github.com/pingcap/log@v1.1.1-0.20250917021125-19901e015dc9/zap_text_encoder.go
//!
//! Layout:
//! `[Time] [LEVEL] [LoggerName] [Caller] [Message] <BoundWithFields> <CallSiteFields> [stack="..."]\n`
//!
//! Known differences from Go v8.5.8 (approved by owner 2026-11-27):
//! - Ruling R34 (#454): `LogValue::Skip` / `None` errors are silently omitted (no empty `[]`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use tidb_base::gostd::strconv::append_float;
use tidb_base::gostd::time::duration_string;
use tidb_base::structured_log::{LogField, LogValue};

use super::{EncoderConfig, Entry, append_filtered_caller, append_timestamp, log_time_to_datetime};

/// `needDoubleQuotes` (`zap_text_encoder.go:531-539`): returns `true` iff any raw byte is
/// `<= 0x20` or one of `\`, `"`, `[`, `]`, `=`.
///
/// Scans raw bytes without UTF-8 decoding (so invalid UTF-8 without special ASCII stays unquoted
/// and emits `\ufffd`, and `0x7f` stays unquoted raw).
// go-dep: github.com/pingcap/log@v1.1.1-0.20250917021125-19901e015dc9.needDoubleQuotes L1
pub fn need_double_quotes(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|&b| b <= 0x20 || matches!(b, b'\\' | b'"' | b'[' | b']' | b'='))
}

/// `safeAddByteString` (`zap_text_encoder.go:543-629`): escapes `\`, `"`, `\n`, `\r`, `\t`,
/// other `< 0x20` as `\u00xx`, passes valid UTF-8 and `0x20..=0x7f` raw, and encodes **each
/// invalid UTF-8 byte** as the 6-byte ASCII sequence `\ufffd`.
// go-dep: github.com/pingcap/log@v1.1.1-0.20250917021125-19901e015dc9.textEncoder.safeAddByteString L1
pub fn safe_add_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
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
                    let ch = s.chars().next().expect("non-empty");
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
                    // Go utf8.DecodeRune yields (RuneError, 1) -> 6-byte ASCII `\ufffd`.
                    buf.extend_from_slice(b"\\ufffd");
                    i += 1;
                }
            }
        }
    }
}

/// Encode `bytes` with `need_double_quotes` wrapping and `safe_add_bytes` escaping.
// glue: textEncoder.AppendByteString
pub fn append_quoted_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    let quote = need_double_quotes(bytes);
    if quote {
        buf.push(b'"');
    }
    safe_add_bytes(buf, bytes);
    if quote {
        buf.push(b'"');
    }
}

/// Encode a full log entry into `out` in `pingcap/log` text format, ending with a single `\n`
/// and containing zero raw `\n` or `\r` bytes before the trailing `\n`.
// go-dep: github.com/pingcap/log@v1.1.1-0.20250917021125-19901e015dc9.textEncoder.EncodeEntry L1
pub fn encode_entry(cfg: EncoderConfig, entry: &Entry<'_>, out: &mut Vec<u8>) {
    // 1. Time (omitted with NO leading space when disable_timestamp = true)
    if !cfg.disable_timestamp {
        out.push(b'[');
        append_timestamp(out, entry.time);
        out.push(b']');
        out.push(b' ');
    }

    // 2. LEVEL (always present)
    out.push(b'[');
    out.extend_from_slice(entry.level.as_upper_str().as_bytes());
    out.push(b']');

    // 3. LoggerName (when non-empty)
    if !entry.logger_name.is_empty() {
        out.push(b' ');
        out.push(b'[');
        append_quoted_bytes(out, entry.logger_name.as_bytes());
        out.push(b']');
    }

    // 4. Caller (when enabled and defined)
    if !cfg.disable_caller && !entry.file.is_empty() {
        out.push(b' ');
        out.push(b'[');
        append_filtered_caller(out, entry.file, entry.line);
        out.push(b']');
    }

    // 5. Message (omitted completely when empty in text mode)
    if !entry.msg.is_empty() {
        out.push(b' ');
        out.push(b'[');
        append_quoted_bytes(out, entry.msg.as_bytes());
        out.push(b']');
    }

    // 6. Bound .With(...) fields (in declaration order)
    for field in entry.bound_fields {
        encode_field(cfg, field, out);
    }

    // 7. Call-site fields (in declaration order)
    for field in entry.fields {
        encode_field(cfg, field, out);
    }

    // 8. Stack trace (AddStacktrace(FatalLevel) or explicit entry.stack)
    if !entry.stack.is_empty() {
        out.push(b' ');
        out.extend_from_slice(b"[stack=");
        append_quoted_bytes(out, entry.stack.as_bytes());
        out.push(b']');
    }

    // 9. Single trailing newline
    out.push(b'\n');
}

fn encode_field(cfg: EncoderConfig, field: &LogField<'_>, out: &mut Vec<u8>) {
    // Ruling R34 (#454): silently omit Skip fields instead of emitting Go's spurious `[]`.
    if matches!(field.value, LogValue::Skip) {
        return;
    }

    if let LogValue::Error(err) = field.value {
        let raw = err.error();
        out.push(b' ');
        out.push(b'[');
        append_quoted_bytes(out, field.key.as_bytes());
        out.push(b'=');
        append_quoted_bytes(out, &raw);
        out.push(b']');

        if (!cfg.disable_error_verbose || cfg.force_error_verbose) && err.has_stack() {
            let verbose = err.error_verbose();
            if verbose != raw {
                out.push(b' ');
                out.push(b'[');
                let mut vkey = Vec::with_capacity(field.key.len() + 7);
                vkey.extend_from_slice(field.key.as_bytes());
                vkey.extend_from_slice(b"Verbose");
                append_quoted_bytes(out, &vkey);
                out.push(b'=');
                append_quoted_bytes(out, &verbose);
                out.push(b']');
            }
        }
        return;
    }

    out.push(b' ');
    out.push(b'[');
    append_quoted_bytes(out, field.key.as_bytes());
    out.push(b'=');
    encode_value(field.value, out);
    out.push(b']');
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
        // Go pingcap/log textEncoder.AddFloat32 widens to float64 (bitSize = 64).
        LogValue::F32(f) => append_float(out, f64::from(f), b'f', -1, 64),
        LogValue::F64(f) => append_float(out, f, b'f', -1, 64),
        LogValue::Str(s) => append_quoted_bytes(out, s.as_bytes()),
        LogValue::Bytes(b) => append_quoted_bytes(out, b),
        LogValue::Binary(b) => {
            let encoded = BASE64_STANDARD.encode(b);
            append_quoted_bytes(out, encoded.as_bytes());
        }
        LogValue::Duration(d) => out.extend_from_slice(duration_string(d).as_bytes()),
        // Go DefaultTimeEncoder writes directly into e.buf without quoting, preserving the
        // value's own fixed timezone offset.
        LogValue::Time(t) => append_timestamp(out, log_time_to_datetime(t)),
        LogValue::Error(e) => append_quoted_bytes(out, &e.error()),
        LogValue::Strings(xs) => {
            let mut sub = Vec::new();
            sub.push(b'[');
            for (i, s) in xs.iter().enumerate() {
                if i > 0 {
                    sub.push(b',');
                }
                append_quoted_bytes(&mut sub, s.as_bytes());
            }
            sub.push(b']');
            append_quoted_bytes(out, &sub);
        }
        LogValue::I64s(xs) => {
            let mut sub = Vec::new();
            sub.push(b'[');
            let mut itoa_buf = itoa::Buffer::new();
            for (i, &n) in xs.iter().enumerate() {
                if i > 0 {
                    sub.push(b',');
                }
                sub.extend_from_slice(itoa_buf.format(n).as_bytes());
            }
            sub.push(b']');
            append_quoted_bytes(out, &sub);
        }
        LogValue::U64s(xs) => {
            let mut sub = Vec::new();
            sub.push(b'[');
            let mut itoa_buf = itoa::Buffer::new();
            for (i, &n) in xs.iter().enumerate() {
                if i > 0 {
                    sub.push(b',');
                }
                sub.extend_from_slice(itoa_buf.format(n).as_bytes());
            }
            sub.push(b']');
            append_quoted_bytes(out, &sub);
        }
        LogValue::Bools(xs) => {
            let mut sub = Vec::new();
            sub.push(b'[');
            for (i, &b) in xs.iter().enumerate() {
                if i > 0 {
                    sub.push(b',');
                }
                sub.extend_from_slice(if b { b"true" } else { b"false" });
            }
            sub.push(b']');
            append_quoted_bytes(out, &sub);
        }
        LogValue::F32s(xs) => {
            // Go pingcap/log AppendFloat32 (array elements) uses bitSize = 32.
            let mut sub = Vec::new();
            sub.push(b'[');
            for (i, &f) in xs.iter().enumerate() {
                if i > 0 {
                    sub.push(b',');
                }
                append_float(&mut sub, f64::from(f), b'f', -1, 32);
            }
            sub.push(b']');
            append_quoted_bytes(out, &sub);
        }
        LogValue::F64s(xs) => {
            let mut sub = Vec::new();
            sub.push(b'[');
            for (i, &f) in xs.iter().enumerate() {
                if i > 0 {
                    sub.push(b',');
                }
                append_float(&mut sub, f, b'f', -1, 64);
            }
            sub.push(b']');
            append_quoted_bytes(out, &sub);
        }
        LogValue::Times(xs) => {
            // Go pingcap/log DefaultTimeEncoder in text array does not call addElementSeparator.
            let mut sub = Vec::new();
            sub.push(b'[');
            for &t in xs {
                append_timestamp(&mut sub, log_time_to_datetime(t));
            }
            sub.push(b']');
            append_quoted_bytes(out, &sub);
        }
        LogValue::Objects(objs) => {
            // Go pingcap/log textEncoder AppendArray + AppendObject (zap_text_encoder.go:259-297):
            // each object formats as `{k=v,...}`, quoted as an array element, then the whole
            // `[...]` array is quoted for the outer `[key=value]` field.
            let mut arr = Vec::new();
            arr.push(b'[');
            for (i, obj) in objs.iter().enumerate() {
                if i > 0 {
                    arr.push(b',');
                }
                let mut obj_buf = Vec::new();
                obj_buf.push(b'{');
                let mut first = true;
                for f in *obj {
                    if matches!(f.value, LogValue::Skip) {
                        continue;
                    }
                    if !first {
                        obj_buf.push(b',');
                    }
                    first = false;
                    append_quoted_bytes(&mut obj_buf, f.key.as_bytes());
                    obj_buf.push(b'=');
                    encode_value(f.value, &mut obj_buf);
                }
                obj_buf.push(b'}');
                append_quoted_bytes(&mut arr, &obj_buf);
            }
            arr.push(b']');
            append_quoted_bytes(out, &arr);
        }
        LogValue::JsonBytes(b) => append_quoted_bytes(out, b),
        LogValue::Skip => {}
    }
}
