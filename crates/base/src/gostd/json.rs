//! `encoding/json` for `pkg/config`: the generic value of `json.Unmarshal(…, &any)`, Go's
//! encoder output (`json.Marshal` with HTML escaping, sorted map keys, Go float format) and
//! `json.Indent`.

use std::collections::BTreeMap;

/// A decoded JSON value (Go `any` from `json.Unmarshal`: `map[string]any`, `[]any`,
/// `float64`, `string`, `bool`, `nil`).
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    /// An integer kept exact (Go encodes integer fields of structs without a float trip).
    Int(i128),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

/// `floatEncoder.encode` for float64.
// go-std: encoding/json.floatEncoder.encode L1 (bits 64)
pub fn format_float(f: f64) -> String {
    let abs = f.abs();
    let mut fmt = b'f';
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        fmt = b'e';
    }
    let mut b = crate::gofmt::append_float(f, fmt, -1, 64).into_bytes();
    if fmt == b'e' {
        // clean up e-09 to e-9
        let n = b.len();
        if n >= 4 && b[n - 4] == b'e' && b[n - 3] == b'-' && b[n - 2] == b'0' {
            b[n - 2] = b[n - 1];
            b.truncate(n - 1);
        }
    }
    String::from_utf8(b).unwrap_or_default()
}

/// `appendString` with `escapeHTML` (`json.Marshal`).
// go-std: encoding/json.appendString L1 (escapeHTML = true; input is valid UTF-8)
pub fn append_string(out: &mut String, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0}'..='\u{1f}' | '<' | '>' | '&' => {
                let b = c as u8;
                out.push_str("\\u00");
                out.push(char::from(HEX[usize::from(b >> 4)]));
                out.push(char::from(HEX[usize::from(b & 0xF)]));
            }
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `string(b)` read rune by rune as Go does (`for _, r := range s`): every byte of an invalid
/// sequence becomes one U+FFFD (`String::from_utf8_lossy` would replace a truncated multi-byte
/// sequence with a single one).
// go-std: unicode/utf8.DecodeRune L2 (RuneError, 1 for each invalid byte)
pub fn go_lossy(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    let mut rest = b;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(v) => {
                out.push_str(v);
                break;
            }
            Err(e) => {
                let (v, after) = rest.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(v).unwrap_or_default());
                let bad = e.error_len().unwrap_or(after.len()).max(1);
                for _ in 0..bad {
                    out.push('\u{FFFD}');
                }
                rest = &after[bad..];
            }
        }
    }
    out
}

/// `appendString` with `escapeHTML` for a Go string that may hold invalid UTF-8: each
/// invalid byte is written as the escape `\ufffd` (a valid U+FFFD is written as it is).
// go-std: encoding/json.appendString L1 (escapeHTML = true)
pub fn append_go_string(out: &mut String, s: &[u8]) {
    out.push('"');
    let mut rest = s;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                let mut tmp = String::new();
                append_string(&mut tmp, valid);
                out.push_str(&tmp[1..tmp.len() - 1]);
                break;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                let valid = std::str::from_utf8(valid).unwrap_or_default();
                let mut tmp = String::new();
                append_string(&mut tmp, valid);
                out.push_str(&tmp[1..tmp.len() - 1]);
                // utf8.DecodeRuneInString yields (RuneError, 1) per invalid byte.
                let bad = e.error_len().unwrap_or(after.len()).max(1);
                for _ in 0..bad {
                    out.push_str("\\ufffd");
                }
                rest = &after[bad..];
            }
        }
    }
    out.push('"');
}

/// `json.Marshal` of the value (compact).
// go-std: encoding/json.Marshal L2 (generic values; maps sorted by key)
pub fn marshal(v: &Value) -> String {
    let mut out = String::new();
    write(&mut out, v, None, 0);
    out
}

/// `json.Marshal` followed by `json.Indent(dst, src, "", indent)`.
// go-std: encoding/json.Indent L2 (applied to Marshal's compact output; empty {} and [] stay compact)
pub fn marshal_indent(v: &Value, indent: &str) -> String {
    let mut out = String::new();
    write(&mut out, v, Some(indent), 0);
    out
}

// go-std: encoding/json.Indent L2 (newline and prefix of the indenter)
fn newline(out: &mut String, indent: Option<&str>, depth: usize) {
    if let Some(ind) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(ind);
        }
    }
}

// go-std: encoding/json.encodeState.reflectValue L2 (generic values; Indent applied inline)
fn write(out: &mut String, v: &Value, indent: Option<&str>, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(f) => out.push_str(&format_float(*f)),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::String(s) => append_string(out, s),
        Value::Array(a) => {
            out.push('[');
            if a.is_empty() {
                out.push(']');
                return;
            }
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write(out, e, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            if m.is_empty() {
                out.push('}');
                return;
            }
            for (i, (k, e)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                append_string(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write(out, e, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

/// `json.Unmarshal(data, &v)` with `v any`: numbers become `float64`. The error text is not
/// Go's (`*json.SyntaxError` messages are not ported).
// go-std: encoding/json.Unmarshal L2 (into any; syntax error messages differ)
pub fn unmarshal(data: &[u8]) -> Result<Value, String> {
    let mut p = JsonParser { b: data, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err("invalid character after top-level value".to_owned());
    }
    Ok(v)
}

struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl JsonParser<'_> {
    // go-std: encoding/json.isSpace L1
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    // glue: syntax error of the JSON decoder (message not Go's)
    fn err<T>(&self) -> Result<T, String> {
        if self.i >= self.b.len() {
            return Err("unexpected end of JSON input".to_owned());
        }
        Err(format!("invalid character at offset {}", self.i))
    }

    // go-std: encoding/json.decodeState.literalInterface L2 (null/true/false)
    fn lit(&mut self, s: &[u8], v: Value) -> Result<Value, String> {
        if self.b[self.i..].starts_with(s) {
            self.i += s.len();
            return Ok(v);
        }
        self.err()
    }

    // go-std: encoding/json.decodeState.valueInterface L2 (scanner merged into a recursive descent parser)
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > 10000 {
            return Err("exceeded max depth".to_owned());
        }
        match self.b.get(self.i) {
            None => self.err(),
            Some(b'n') => self.lit(b"null", Value::Null),
            Some(b't') => self.lit(b"true", Value::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Bool(false)),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Value::Array(a));
                }
                loop {
                    self.ws();
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::Array(a));
                        }
                        _ => return self.err(),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut m = BTreeMap::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Value::Object(m));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return self.err();
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return self.err();
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    m.insert(k, v);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Object(m));
                        }
                        _ => return self.err(),
                    }
                }
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let s = self.number_literal()?;
                let f: f64 = s.parse().map_err(|_| "number".to_owned())?;
                if f.is_infinite() {
                    return Err(format!(
                        "json: cannot unmarshal number {s} into Go value of type float64"
                    ));
                }
                Ok(Value::Number(f))
            }
            Some(_) => self.err(),
        }
    }

    // go-std: encoding/json.stateBeginValue L2 (number literal of the scanner)
    fn number_literal(&mut self) -> Result<String, String> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => {
                while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                    self.i += 1;
                }
            }
            _ => return self.err(),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                return self.err();
            }
            while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if !self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                return self.err();
            }
            while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
        }
        std::str::from_utf8(&self.b[start..self.i])
            .map(str::to_owned)
            .map_err(|e| e.to_string())
    }

    // go-std: encoding/json.getu4 L1
    fn hex4(&mut self) -> Option<u32> {
        let h = self.b.get(self.i..self.i + 4)?;
        let s = std::str::from_utf8(h).ok()?;
        let v = u32::from_str_radix(s, 16).ok()?;
        self.i += 4;
        Some(v)
    }

    // go-std: encoding/json.unquoteBytes L2 (invalid UTF-8 and lone surrogates become U+FFFD)
    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return self.err();
            };
            match c {
                b'"' => {
                    self.i += 1;
                    // Invalid UTF-8 is replaced by U+FFFD like Go: one per invalid byte
                    // (utf8.DecodeRune returns RuneError, 1).
                    return Ok(go_lossy(&out));
                }
                b'\\' => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else {
                        return self.err();
                    };
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let Some(r) = self.hex4() else {
                                return self.err();
                            };
                            if (0xD800..0xDC00).contains(&r) && self.b[self.i..].starts_with(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                match self.hex4() {
                                    Some(r2) if (0xDC00..0xE000).contains(&r2) => char::from_u32(
                                        0x10000 + ((r - 0xD800) << 10) + (r2 - 0xDC00),
                                    )
                                    .unwrap_or('\u{FFFD}'),
                                    _ => {
                                        self.i = save;
                                        '\u{FFFD}'
                                    }
                                }
                            } else {
                                char::from_u32(r).unwrap_or('\u{FFFD}')
                            }
                        }
                        _ => return self.err(),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c if c < 0x20 => return self.err(),
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Decoding into typed Go values (D02.3): `json.Unmarshal(data, &v)` where `v` is a struct, with
// the reflect-driven rules of `decodeState.object`, `array` and `literalStore`. A typed port
// writes its fields' decoding with [`Decoder`]; the generated layer of D06.1 builds on it.

/// A syntactically valid JSON value with object members in source order and numbers kept as
/// their literal text (Go decodes struct fields straight from the literal, without a float).
#[derive(Clone, Debug, PartialEq)]
pub enum Raw {
    Null,
    Bool(bool),
    /// The number literal as written (`1`, `-0`, `1e2`, ...).
    Number(String),
    /// The unquoted string (invalid UTF-8 and lone surrogates are U+FFFD, as in Go).
    String(String),
    Array(Vec<Raw>),
    /// Members in source order; duplicate keys are kept (Go applies them one after another).
    Object(Vec<(String, Raw)>),
}

impl Raw {
    /// The `Value` of a `*json.UnmarshalTypeError` for this JSON value kind, without the
    /// number literal (`literalStore` adds it only for numeric destinations).
    // go-std: encoding/json.UnmarshalTypeError.Value L2 (kind names used by decodeState)
    pub fn kind(&self) -> &'static str {
        match self {
            Raw::Null => "null",
            Raw::Bool(_) => "bool",
            Raw::Number(_) => "number",
            Raw::String(_) => "string",
            Raw::Array(_) => "array",
            Raw::Object(_) => "object",
        }
    }
}

/// `checkValid` plus the value tree `decodeState` walks. A syntax error returns `Err` before
/// anything is decoded, as in Go. The error text is not Go's (`*json.SyntaxError` messages
/// are not ported, as for [`unmarshal`]).
// go-std: encoding/json.checkValid L2 (the scanner merged into a recursive descent parser; syntax error messages differ)
pub fn unmarshal_raw(data: &[u8]) -> Result<Raw, String> {
    let mut p = JsonParser { b: data, i: 0 };
    p.ws();
    let v = p.raw(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err("invalid character after top-level value".to_owned());
    }
    Ok(v)
}

impl JsonParser<'_> {
    // go-std: encoding/json.decodeState.value L2 (into the ordered tree; scanner merged)
    fn raw(&mut self, depth: usize) -> Result<Raw, String> {
        if depth > 10000 {
            return Err("exceeded max depth".to_owned());
        }
        match self.b.get(self.i) {
            None => self.err(),
            Some(b'n') => self.lit(b"null", Value::Null).map(|_| Raw::Null),
            Some(b't') => self.lit(b"true", Value::Null).map(|_| Raw::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Null).map(|_| Raw::Bool(false)),
            Some(b'"') => Ok(Raw::String(self.string()?)),
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Raw::Array(a));
                }
                loop {
                    self.ws();
                    a.push(self.raw(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Raw::Array(a));
                        }
                        _ => return self.err(),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut m = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Raw::Object(m));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return self.err();
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return self.err();
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.raw(depth + 1)?;
                    m.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Raw::Object(m));
                        }
                        _ => return self.err(),
                    }
                }
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => Ok(Raw::Number(self.number_literal()?)),
            Some(_) => self.err(),
        }
    }
}

/// `foldName`: the key under which `byFoldedName` finds a field, so that two names match
/// exactly when `bytes.EqualFold` says so.
// go-std: encoding/json.appendFoldedName L1
pub fn fold_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(fold_rune(c));
        }
    }
    out
}

/// `foldRune`: the smallest rune of the fold set of `r`.
// go-std: encoding/json.foldRune L1
fn fold_rune(mut r: char) -> char {
    loop {
        let r2 = super::simple_fold(r);
        if r2 <= r {
            return r2;
        }
        r = r2;
    }
}

/// The struct field a JSON object key selects: an exact name first (`byExactName`), then a
/// case-insensitive match (`byFoldedName`). `names` are the JSON names of the fields in
/// declaration order. Unknown keys select nothing (no `DisallowUnknownFields`).
// go-std: encoding/json.decodeState.object L2 (field lookup of cachedTypeFields)
pub fn find_field(names: &[&str], key: &str) -> Option<usize> {
    if let Some(i) = names.iter().position(|n| *n == key) {
        return Some(i);
    }
    let k = fold_name(key);
    names.iter().position(|n| fold_name(n) == k)
}

/// The decoding state of one `json.Unmarshal` call: the first saved error (later fields are
/// still decoded, as in Go) and the error context (innermost struct, field path).
// go-std: encoding/json.decodeState L2 (savedError and errorContext only; the value tree comes from unmarshal_raw)
#[derive(Debug, Default)]
pub struct Decoder {
    saved: Option<String>,
    struct_name: &'static str,
    fields: Vec<&'static str>,
}

impl Decoder {
    // glue: a fresh decodeState
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// The first error saved during decoding (`d.savedError`), formatted like
    /// `*json.UnmarshalTypeError.Error()`.
    // go-std: encoding/json.decodeState.unmarshal L2 (returns savedError)
    pub fn finish(self) -> Result<(), String> {
        match self.saved {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// `d.saveError(&UnmarshalTypeError{Value: value, Type: ty})` with the current error
    /// context applied (`addErrorContext`).
    // go-std: encoding/json.decodeState.saveError L1
    // go-std: encoding/json.decodeState.addErrorContext L1
    // go-std: encoding/json.UnmarshalTypeError.Error L1
    pub fn save_type_error(&mut self, value: &str, ty: &str) {
        if self.saved.is_some() {
            return;
        }
        let msg = if !self.struct_name.is_empty() || !self.fields.is_empty() {
            format!(
                "json: cannot unmarshal {value} into Go struct field {}.{} of type {ty}",
                self.struct_name,
                self.fields.join(".")
            )
        } else {
            format!("json: cannot unmarshal {value} into Go value of type {ty}")
        };
        self.saved = Some(msg);
    }

    /// Decodes the members of a JSON object into a struct: `field(d, index, value)` is called
    /// for every member whose key selects a field (see [`find_field`]), with the error context
    /// set to `struct_name` and the field's name. A non-object saves a type error with
    /// `ty` (the Go type, e.g. `types.jsonFieldType`); `null` leaves the struct unchanged.
    // go-std: encoding/json.decodeState.object L2 (struct destinations without embedded fields or tags options)
    pub fn object(
        &mut self,
        v: &Raw,
        struct_name: &'static str,
        ty: &str,
        names: &[&'static str],
        mut field: impl FnMut(&mut Decoder, usize, &Raw),
    ) {
        let members = match v {
            Raw::Null => return,
            Raw::Object(m) => m,
            other => {
                self.save_type_error(other.kind(), ty);
                return;
            }
        };
        for (k, val) in members {
            let Some(i) = find_field(names, k) else {
                continue;
            };
            let orig_struct = self.struct_name;
            let orig_len = self.fields.len();
            self.fields.push(names[i]);
            self.struct_name = struct_name;
            field(self, i, val);
            self.fields.truncate(orig_len);
            self.struct_name = orig_struct;
        }
    }

    /// A signed integer destination of Go type `ty` in `[min, max]`.
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.Int* destinations)
    pub fn int(&mut self, v: &Raw, dst: &mut i64, min: i64, max: i64, ty: &str) {
        match v {
            Raw::Null => {}
            Raw::Number(s) => match s.parse::<i64>() {
                // strconv.ParseInt(s, 10, 64): decimal digits with an optional sign only.
                Ok(n) if n >= min && n <= max && !s.starts_with('+') => *dst = n,
                _ => self.save_type_error(&format!("number {s}"), ty),
            },
            other => self.save_type_error(other.kind(), ty),
        }
    }

    /// An unsigned integer destination of Go type `ty` in `[0, max]`.
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.Uint* destinations)
    pub fn uint(&mut self, v: &Raw, dst: &mut u64, max: u64, ty: &str) {
        match v {
            Raw::Null => {}
            Raw::Number(s) => match s.parse::<u64>() {
                Ok(n) if n <= max && !s.starts_with('+') => *dst = n,
                _ => self.save_type_error(&format!("number {s}"), ty),
            },
            other => self.save_type_error(other.kind(), ty),
        }
    }

    /// A `bool` destination.
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.Bool destinations)
    pub fn bool(&mut self, v: &Raw, dst: &mut bool) {
        match v {
            Raw::Null => {}
            Raw::Bool(b) => *dst = *b,
            other => self.save_type_error(other.kind(), "bool"),
        }
    }

    /// A `string` destination (the unquoted text; invalid UTF-8 was already replaced).
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.String destinations)
    pub fn string(&mut self, v: &Raw, dst: &mut String) {
        match v {
            Raw::Null => {}
            Raw::String(s) => s.clone_into(dst),
            other => self.save_type_error(other.kind(), "string"),
        }
    }

    /// A slice destination (`None` is a nil slice) of Go type `ty`: `null` makes it nil, an
    /// array decodes into the existing elements first (Go reuses the backing array), then
    /// truncates; `[]` gives an empty, non-nil slice.
    // go-std: encoding/json.decodeState.array L2 (slice destinations)
    pub fn slice<T: Default>(
        &mut self,
        v: &Raw,
        dst: &mut Option<Vec<T>>,
        ty: &str,
        mut elem: impl FnMut(&mut Decoder, &Raw, &mut T),
    ) {
        let items = match v {
            Raw::Null => {
                *dst = None;
                return;
            }
            Raw::Array(a) => a,
            other => {
                self.save_type_error(other.kind(), ty);
                return;
            }
        };
        let vec = dst.get_or_insert_with(Vec::new);
        for (i, item) in items.iter().enumerate() {
            if i >= vec.len() {
                vec.push(T::default());
            }
            elem(self, item, &mut vec[i]);
        }
        vec.truncate(items.len());
    }
}
