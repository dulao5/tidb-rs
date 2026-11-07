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
                let s = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
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
                    // Invalid UTF-8 is replaced by U+FFFD like Go.
                    return Ok(String::from_utf8_lossy(&out).into_owned());
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
