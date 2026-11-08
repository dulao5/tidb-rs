//! `encoding/json` for `pkg/config`: the generic value of `json.Unmarshal(…, &any)`, Go's
//! encoder output (`json.Marshal` with HTML escaping, sorted map keys, Go float format) and
//! `json.Indent`.

use std::collections::BTreeMap;

/// A decoded JSON value (Go `any` from `json.Unmarshal`: `map[string]any`, `[]any`,
/// `float64`, `string`, `bool`, `nil`).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Value {
    #[default]
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
        // Four hex digits exactly (`from_str_radix` alone would accept a leading `+`).
        if !h.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
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

/// One step of [`scan`]: the values of a JSON text in source order, without building a tree.
#[derive(Debug, PartialEq)]
pub enum ScanEvent<'a> {
    BeginArray,
    BeginObject,
    /// An object member's key (unquoted as [`unmarshal_raw`] does); its value follows.
    Key(String),
    /// The end of the innermost open array or object.
    End,
    Null,
    Bool(bool),
    String,
    /// A number literal as written.
    Number(&'a str),
}

/// `json.Valid` / `checkValid` without recursion: accepts exactly the texts [`unmarshal_raw`]
/// accepts and reports their values to `f` in source order, keeping an explicit stack of
/// open containers instead of the call stack (Go's scanner is iterative too). Used for
/// texts too deeply nested for the recursive parser.
// go-std: encoding/json.checkValid L2 (iterative scanner emitting value events; syntax error messages differ)
pub fn scan(data: &[u8], mut f: impl FnMut(ScanEvent<'_>)) -> Result<(), String> {
    let mut p = JsonParser { b: data, i: 0 };
    // b'[' or b'{' for every open container.
    let mut stack: Vec<u8> = Vec::new();
    p.ws();
    'value: loop {
        // One value at p.i, nested in `stack.len()` containers (`raw(depth)`).
        if stack.len() > 10000 {
            return Err("exceeded max depth".to_owned());
        }
        match p.b.get(p.i) {
            None => return p.err(),
            Some(b'n') => {
                p.lit(b"null", Value::Null)?;
                f(ScanEvent::Null);
            }
            Some(b't') => {
                p.lit(b"true", Value::Null)?;
                f(ScanEvent::Bool(true));
            }
            Some(b'f') => {
                p.lit(b"false", Value::Null)?;
                f(ScanEvent::Bool(false));
            }
            Some(b'"') => {
                p.string()?;
                f(ScanEvent::String);
            }
            Some(b'[') => {
                p.i += 1;
                f(ScanEvent::BeginArray);
                p.ws();
                if p.b.get(p.i) == Some(&b']') {
                    p.i += 1;
                    f(ScanEvent::End);
                } else {
                    stack.push(b'[');
                    p.ws();
                    continue 'value;
                }
            }
            Some(b'{') => {
                p.i += 1;
                f(ScanEvent::BeginObject);
                p.ws();
                if p.b.get(p.i) == Some(&b'}') {
                    p.i += 1;
                    f(ScanEvent::End);
                } else {
                    stack.push(b'{');
                    let k = p.member_key()?;
                    f(ScanEvent::Key(k));
                    continue 'value;
                }
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let s = p.number_literal()?;
                f(ScanEvent::Number(&s));
            }
            Some(_) => return p.err(),
        }
        // A value is complete: a separator continues its container, a closer ends it.
        loop {
            p.ws();
            match stack.last() {
                None => break 'value,
                Some(b'[') => match p.b.get(p.i) {
                    Some(b',') => {
                        p.i += 1;
                        p.ws();
                        continue 'value;
                    }
                    Some(b']') => {
                        p.i += 1;
                        stack.pop();
                        f(ScanEvent::End);
                    }
                    _ => return p.err(),
                },
                Some(_) => match p.b.get(p.i) {
                    Some(b',') => {
                        p.i += 1;
                        let k = p.member_key()?;
                        f(ScanEvent::Key(k));
                        continue 'value;
                    }
                    Some(b'}') => {
                        p.i += 1;
                        stack.pop();
                        f(ScanEvent::End);
                    }
                    _ => return p.err(),
                },
            }
        }
    }
    p.ws();
    if p.i != p.b.len() {
        return Err("invalid character after top-level value".to_owned());
    }
    Ok(())
}

impl JsonParser<'_> {
    /// `"key"` and `:` of an object member, with the white space around them.
    // glue: member key of the iterative scanner (the object loop of `raw`)
    fn member_key(&mut self) -> Result<String, String> {
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
        Ok(k)
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
    String(String, Span),
    Array(Vec<Raw>, Span),
    /// Members in source order; duplicate keys are kept (Go applies them one after another).
    Object(Vec<(String, Raw)>, Span),
}

/// The bytes `[start, end)` of a value in the input of [`unmarshal_raw`]: what Go passes to
/// an `UnmarshalJSON` method (`d.data[start:d.readIndex()]`), needed by `json.RawMessage`.
pub type Span = (usize, usize);

impl Raw {
    /// The `Value` of a `*json.UnmarshalTypeError` for this JSON value kind, without the
    /// number literal (`literalStore` adds it only for numeric destinations).
    // go-std: encoding/json.UnmarshalTypeError.Value L2 (kind names used by decodeState)
    pub fn kind(&self) -> &'static str {
        match self {
            Raw::Null => "null",
            Raw::Bool(_) => "bool",
            Raw::Number(_) => "number",
            Raw::String(..) => "string",
            Raw::Array(..) => "array",
            Raw::Object(..) => "object",
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
        let start = self.i;
        match self.b.get(self.i) {
            None => self.err(),
            Some(b'n') => self.lit(b"null", Value::Null).map(|_| Raw::Null),
            Some(b't') => self.lit(b"true", Value::Null).map(|_| Raw::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Null).map(|_| Raw::Bool(false)),
            Some(b'"') => {
                let s = self.string()?;
                Ok(Raw::String(s, (start, self.i)))
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Raw::Array(a, (start, self.i)));
                }
                loop {
                    self.ws();
                    a.push(self.raw(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Raw::Array(a, (start, self.i)));
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
                    return Ok(Raw::Object(m, (start, self.i)));
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
                            return Ok(Raw::Object(m, (start, self.i)));
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

/// An error saved while decoding: a `*json.UnmarshalTypeError` (its fields, so that an outer
/// decoder can apply its error context to an error returned by an `UnmarshalJSON` method, as
/// Go's `addErrorContext` does) or another error (base64, an `UnmarshalJSON` method's own).
// go-std: encoding/json.UnmarshalTypeError L2 (Value, Type, Struct, Field; no Offset)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Type {
        value: String,
        ty: String,
        struct_name: String,
        field: String,
    },
    Other(String),
}

impl std::fmt::Display for DecodeError {
    // go-std: encoding/json.UnmarshalTypeError.Error L1
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Type {
                value,
                ty,
                struct_name,
                field,
            } => {
                if !struct_name.is_empty() || !field.is_empty() {
                    write!(
                        f,
                        "json: cannot unmarshal {value} into Go struct field {struct_name}.{field} of type {ty}"
                    )
                } else {
                    write!(
                        f,
                        "json: cannot unmarshal {value} into Go value of type {ty}"
                    )
                }
            }
            DecodeError::Other(s) => f.write_str(s),
        }
    }
}

/// The decoding state of one `json.Unmarshal` call: the first saved error (later fields are
/// still decoded, as in Go) and the error context (innermost struct, field path).
// go-std: encoding/json.decodeState L2 (savedError and errorContext only; the value tree comes from unmarshal_raw)
#[derive(Debug, Default)]
pub struct Decoder {
    saved: Option<DecodeError>,
    /// An error returned (not saved) by Go: from an `UnmarshalJSON` method. It ends the
    /// decoding and is the result of `Unmarshal` even if an error was saved before.
    fatal: Option<DecodeError>,
    struct_name: &'static str,
    fields: Vec<&'static str>,
    /// The input of `Unmarshal` (`d.data`), for the [`Span`]s of `json.RawMessage` values.
    source: Option<Box<[u8]>>,
}

impl Decoder {
    // glue: a fresh decodeState
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// A decodeState over `data` (the input the [`Raw`] tree was parsed from): needed when
    /// the destination holds a `json.RawMessage`, which keeps the value's bytes as written.
    // glue: a fresh decodeState with d.data
    pub fn with_source(data: &[u8]) -> Decoder {
        Decoder {
            source: Some(data.into()),
            ..Decoder::default()
        }
    }

    /// A `json.RawMessage` destination: `UnmarshalJSON` copies the value's bytes as they are
    /// in the input (`d.data[start:d.readIndex()]`, whitespace and escapes included; `null`
    /// gives the bytes `null`, not a nil slice). Never an error.
    ///
    /// Without the input (a decoder from [`Decoder::new`]) the bytes are rebuilt from the tree
    /// in the compact form `json.Marshal` writes, which differs from Go's only inside the
    /// value (whitespace, escapes); the generated decoders always pass the input.
    // go-std: encoding/json.RawMessage.UnmarshalJSON L1
    pub fn raw_message(&mut self, v: &Raw, dst: &mut Option<Vec<u8>>) {
        let span = match v {
            Raw::Null => Some(b"null".to_vec()),
            Raw::Bool(true) => Some(b"true".to_vec()),
            Raw::Bool(false) => Some(b"false".to_vec()),
            Raw::Number(n) => Some(n.as_bytes().to_vec()),
            Raw::String(_, sp) | Raw::Array(_, sp) | Raw::Object(_, sp) => self
                .source
                .as_deref()
                .and_then(|src| src.get(sp.0..sp.1))
                .map(<[u8]>::to_vec),
        };
        *dst = Some(span.unwrap_or_else(|| raw_compact(v).into_bytes()));
    }

    /// The first error saved during decoding (`d.savedError`).
    // go-std: encoding/json.decodeState.unmarshal L2 (returns savedError)
    pub fn finish(self) -> Result<(), DecodeError> {
        match self.fatal.or(self.saved) {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Whether an `UnmarshalJSON` method failed: Go returns from every enclosing
    /// `object`/`array` at once, so nothing more is decoded.
    // glue: the early return of decodeState.value
    pub fn failed(&self) -> bool {
        self.fatal.is_some()
    }

    /// The error of an `UnmarshalJSON` method (`return u.UnmarshalJSON(...)`): decoding
    /// stops, and `d.unmarshal` applies the error context of the failure point to it.
    // go-std: encoding/json.decodeState.unmarshal L2 (err := d.value(rv); return d.addErrorContext(err))
    pub fn fail(&mut self, err: DecodeError) {
        if self.fatal.is_some() {
            return;
        }
        let saved = self.saved.take();
        self.save_error(err);
        self.fatal = self.saved.take();
        self.saved = saved;
    }

    /// `d.saveError(err)`: keeps the first error, with the current error context applied to
    /// a type error (`addErrorContext`: the innermost struct, and the field path prepended to
    /// the error's own field).
    // go-std: encoding/json.decodeState.saveError L1
    // go-std: encoding/json.decodeState.addErrorContext L1
    pub fn save_error(&mut self, mut err: DecodeError) {
        if self.saved.is_some() {
            return;
        }
        if let DecodeError::Type {
            struct_name, field, ..
        } = &mut err
            && (!self.struct_name.is_empty() || !self.fields.is_empty())
        {
            *struct_name = self.struct_name.to_owned();
            let mut stack: Vec<&str> = self.fields.clone();
            if !field.is_empty() {
                stack.push(field);
            }
            *field = stack.join(".");
        }
        self.saved = Some(err);
    }

    /// `d.saveError(&UnmarshalTypeError{Value: value, Type: ty})`.
    // go-std: encoding/json.decodeState.saveError L1 (UnmarshalTypeError)
    pub fn save_type_error(&mut self, value: &str, ty: &str) {
        self.save_error(DecodeError::Type {
            value: value.to_owned(),
            ty: ty.to_owned(),
            struct_name: String::new(),
            field: String::new(),
        });
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
        field: impl FnMut(&mut Decoder, usize, &Raw),
    ) {
        self.object_promoted(v, struct_name, ty, names, &[], field);
    }

    /// [`Decoder::object`] for a struct with fields promoted from embedded structs:
    /// `paths[i]` are the Go names of the embedded fields leading to field `i` (empty for a
    /// field of the struct itself), which Go puts in the error context before the field's
    /// JSON name (`Struct.Embedded.json_name`). An empty `paths` means no promoted fields.
    // go-std: encoding/json.decodeState.object L2 (errorContext.FieldStack for f.index)
    pub fn object_promoted(
        &mut self,
        v: &Raw,
        struct_name: &'static str,
        ty: &str,
        names: &[&'static str],
        paths: &[&[&'static str]],
        mut field: impl FnMut(&mut Decoder, usize, &Raw),
    ) {
        let members = match v {
            Raw::Null => return,
            Raw::Object(m, _) => m,
            other => {
                self.save_type_error(other.kind(), ty);
                return;
            }
        };
        for (k, val) in members {
            if self.failed() {
                return;
            }
            let Some(i) = find_field(names, k) else {
                continue;
            };
            let orig_struct = self.struct_name;
            let orig_len = self.fields.len();
            if let Some(path) = paths.get(i) {
                self.fields.extend_from_slice(path);
            }
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

    /// A destination of a named string type `ty` (the type in the error message).
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.String destinations of a named type)
    pub fn string_typed(&mut self, v: &Raw, dst: &mut String, ty: &str) {
        match v {
            Raw::Null => {}
            Raw::String(s, _) => s.clone_into(dst),
            other => self.save_type_error(other.kind(), ty),
        }
    }

    /// A destination of a named bool type `ty`.
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.Bool destinations of a named type)
    pub fn bool_typed(&mut self, v: &Raw, dst: &mut bool, ty: &str) {
        match v {
            Raw::Null => {}
            Raw::Bool(b) => *dst = *b,
            other => self.save_type_error(other.kind(), ty),
        }
    }

    /// A `string` destination (the unquoted text; invalid UTF-8 was already replaced).
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.String destinations)
    pub fn string(&mut self, v: &Raw, dst: &mut String) {
        match v {
            Raw::Null => {}
            Raw::String(s, _) => s.clone_into(dst),
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
            Raw::Array(a, _) => a,
            other => {
                self.save_type_error(other.kind(), ty);
                return;
            }
        };
        let vec = dst.get_or_insert_with(Vec::new);
        for (i, item) in items.iter().enumerate() {
            if self.failed() {
                return;
            }
            if i >= vec.len() {
                vec.push(T::default());
            }
            elem(self, item, &mut vec[i]);
        }
        vec.truncate(items.len());
    }

    /// A `float64` destination: `strconv.ParseFloat(s, 64)`; out of range is a type error.
    // go-std: encoding/json.decodeState.literalStore L2 (reflect.Float64 destinations)
    pub fn float64(&mut self, v: &Raw, dst: &mut f64) {
        match v {
            Raw::Null => {}
            Raw::Number(s) => match s.parse::<f64>() {
                Ok(f) if f.is_finite() => *dst = f,
                _ => self.save_type_error(&format!("number {s}"), "float64"),
            },
            other => self.save_type_error(other.kind(), "float64"),
        }
    }

    /// An `interface{}` destination: replaced by the generic value (`objectInterface`,
    /// `arrayInterface`, `literalInterface`): numbers become `float64` (no `UseNumber`),
    /// objects `map[string]any`. A top-level number out of the float64 range saves a type
    /// error and leaves the destination unchanged (see [`Decoder::any_opt`]).
    // go-std: encoding/json.decodeState.valueInterface L2 (from the Raw tree)
    pub fn any(&mut self, v: &Raw, dst: &mut Value) {
        if let Some(x) = self.any_opt(v) {
            *dst = x;
        }
    }

    /// The value an `interface{}` destination is set to, or `None` when Go leaves the
    /// destination unchanged: a top-level number out of the float64 range
    /// (`literalStore`: `convertNumber` fails, `d.saveError(err); break` before `v.Set`).
    /// Nested out-of-range numbers become `nil` elements (`literalInterface` returns the
    /// nil `n` after saving the error).
    // go-std: encoding/json.decodeState.literalStore L1 (reflect.Interface destinations)
    pub fn any_opt(&mut self, v: &Raw) -> Option<Value> {
        if let Raw::Number(s) = v
            && !matches!(s.parse::<f64>(), Ok(f) if f.is_finite())
        {
            self.save_type_error(&format!("number {s}"), "float64");
            return None;
        }
        Some(self.any_value(v))
    }

    // go-std: encoding/json.decodeState.valueInterface L2 (from the Raw tree)
    fn any_value(&mut self, v: &Raw) -> Value {
        match v {
            Raw::Null => Value::Null,
            Raw::Bool(b) => Value::Bool(*b),
            Raw::String(s, _) => Value::String(s.clone()),
            Raw::Number(s) => match s.parse::<f64>() {
                Ok(f) if f.is_finite() => Value::Number(f),
                _ => {
                    self.save_type_error(&format!("number {s}"), "float64");
                    Value::Null
                }
            },
            Raw::Array(a, _) => Value::Array(a.iter().map(|x| self.any_value(x)).collect()),
            Raw::Object(m, _) => {
                let mut out = BTreeMap::new();
                for (k, x) in m {
                    let x = self.any_value(x);
                    out.insert(k.clone(), x);
                }
                Value::Object(out)
            }
        }
    }

    /// A `[]byte` destination (`None` is nil): a JSON string is standard base64 (a decoding
    /// error is saved and leaves the slice unchanged), `null` makes it nil, and an array of
    /// numbers fills the bytes like any slice.
    // go-std: encoding/json.decodeState.literalStore L2 ([]byte destinations)
    ///
    /// `ty` is the Go slice type (`[]uint8`, or `[]model.ActionType` for a named byte type)
    /// and `elem_ty` its element type, for the type errors.
    pub fn bytes(&mut self, v: &Raw, dst: &mut Option<Vec<u8>>, ty: &str, elem_ty: &str) {
        match v {
            Raw::String(s, _) => match base64_std_decode(s.as_bytes()) {
                Ok(b) => *dst = Some(b),
                Err(pos) => self.save_error(DecodeError::Other(format!(
                    "illegal base64 data at input byte {pos}"
                ))),
            },
            _ => self.slice(v, dst, ty, |d, v, b: &mut u8| {
                let mut n = u64::from(*b);
                d.uint(v, &mut n, u64::from(u8::MAX), elem_ty);
                *b = n as u8;
            }),
        }
    }

    /// A map destination (`None` is nil) with keys parsed by `key`: `null` makes it nil, an
    /// object is merged into the existing map (each value decoded into a fresh zero value).
    // go-std: encoding/json.decodeState.object L2 (map destinations)
    pub fn map<K: Ord, V: Default>(
        &mut self,
        v: &Raw,
        dst: &mut Option<BTreeMap<K, V>>,
        ty: &str,
        mut key: impl FnMut(&mut Decoder, &str) -> Option<K>,
        mut elem: impl FnMut(&mut Decoder, &Raw, &mut V),
    ) {
        let members = match v {
            Raw::Null => {
                *dst = None;
                return;
            }
            Raw::Object(m, _) => m,
            other => {
                self.save_type_error(other.kind(), ty);
                return;
            }
        };
        let m = dst.get_or_insert_with(BTreeMap::new);
        for (k, val) in members {
            if self.failed() {
                return;
            }
            // The value is decoded before the key is converted: an error in the value
            // is saved first; an UnmarshalJSON error returns before the key is stored.
            let mut e = V::default();
            elem(self, val, &mut e);
            if self.failed() {
                return;
            }
            if let Some(k) = key(self, k) {
                m.insert(k, e);
            }
        }
    }

    /// A map key of an integer kind: `strconv.ParseInt(key, 10, 64)` with the range check;
    /// a bad key saves `UnmarshalTypeError{Value: "number " + key}` and drops the member.
    // go-std: encoding/json.decodeState.object L2 (integer map keys)
    pub fn int_key(&mut self, key: &str, min: i64, max: i64, ty: &str) -> Option<i64> {
        match key.parse::<i64>() {
            // strconv.ParseInt(s, 10, 64) accepts a leading '+'; then OverflowInt.
            Ok(n) if n >= min && n <= max => Some(n),
            _ => {
                self.save_type_error(&format!("number {key}"), ty);
                None
            }
        }
    }
}

/// The compact text of a [`Raw`] tree: strings re-escaped by [`append_string`] (only the
/// fallback of [`Decoder::raw_message`] without the input).
// glue: rebuilds a RawMessage from the decoded tree
fn raw_compact(v: &Raw) -> String {
    fn go(out: &mut String, v: &Raw) {
        match v {
            Raw::Null => out.push_str("null"),
            Raw::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Raw::Number(n) => out.push_str(n),
            Raw::String(s, _) => append_string(out, s),
            Raw::Array(a, _) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    go(out, x);
                }
                out.push(']');
            }
            Raw::Object(m, _) => {
                out.push('{');
                for (i, (k, x)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    append_string(out, k);
                    out.push(':');
                    go(out, x);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    go(&mut out, v);
    out
}

/// `json.Marshal` of a `json.RawMessage` (`None` is nil): `MarshalJSON` returns the bytes
/// (`null` for nil) and the encoder appends them with `appendCompact(dst, b, escapeHTML)`:
/// whitespace outside strings is dropped, `<`, `>`, `&`, U+2028 and U+2029 are escaped,
/// everything else (escapes as written included) is copied. Bytes that are not one valid
/// JSON value are Go's `*json.MarshalerError` (the syntax error text is not Go's, except for
/// empty bytes).
// go-std: encoding/json.marshalerEncoder L1 (json.RawMessage)
// go-std: encoding/json.appendCompact L2 (escape = true; validity checked by unmarshal_raw)
pub fn append_raw_message(out: &mut String, m: &Option<Vec<u8>>) -> Result<(), String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let Some(b) = m else {
        out.push_str("null");
        return Ok(());
    };
    if let Err(e) = unmarshal_raw(b) {
        return Err(format!(
            "json: error calling MarshalJSON for type json.RawMessage: {e}"
        ));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(b.len());
    let (mut in_str, mut esc) = (false, false);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        i += 1;
        if !in_str && matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        }
        match c {
            b'<' | b'>' | b'&' => {
                buf.extend_from_slice(b"\\u00");
                buf.push(HEX[usize::from(c >> 4)]);
                buf.push(HEX[usize::from(c & 0xF)]);
            }
            0xE2 if b.get(i) == Some(&0x80) && matches!(b.get(i + 1), Some(0xA8 | 0xA9)) => {
                buf.extend_from_slice(b"\\u202");
                buf.push(HEX[usize::from(b[i + 1] & 0xF)]);
                i += 2;
            }
            _ => buf.push(c),
        }
    }
    // Invalid UTF-8 inside a string is copied by Go as it is; a Rust `String` cannot hold it
    // (spec D06.1 §4: written as U+FFFD; Go itself never writes such bytes into meta).
    match String::from_utf8(buf) {
        Ok(s) => out.push_str(&s),
        Err(e) => out.push_str(&go_lossy(e.as_bytes())),
    }
    Ok(())
}

/// `base64.StdEncoding.DecodeString` (padding required; `\r` and `\n` are skipped). `Err` is
/// the input offset of the first illegal byte (`base64.CorruptInputError`).
// go-std: encoding/base64.Encoding.Decode L2 (StdEncoding; CorruptInputError offset as in decodeQuantum)
pub fn base64_std_decode(src: &[u8]) -> Result<Vec<u8>, usize> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let mut si = 0;
    loop {
        // decodeQuantum: up to 4 symbols, skipping newlines.
        let mut dbuf = [0u8; 4];
        let mut dlen = 4;
        let mut j = 0;
        while j < 4 {
            if si == src.len() {
                if j == 0 {
                    return Ok(out);
                }
                if j == 1 {
                    return Err(si - j);
                }
                // StdEncoding has padding: a short final quantum is corrupt.
                return Err(si - j);
            }
            let c = src[si];
            si += 1;
            if c == b'\n' || c == b'\r' {
                continue;
            }
            if let Some(x) = val(c) {
                dbuf[j] = x;
                j += 1;
                continue;
            }
            if c != b'=' {
                return Err(si - 1);
            }
            // Padding.
            match j {
                0 | 1 => return Err(si - 1),
                2 => {
                    // "==" is expected, possibly with newlines.
                    while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
                        si += 1;
                    }
                    if si == src.len() {
                        return Err(src.len());
                    }
                    if src[si] != b'=' {
                        return Err(si - 1);
                    }
                    si += 1;
                }
                _ => {}
            }
            while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
                si += 1;
            }
            if si < src.len() {
                return Err(si);
            }
            dlen = j;
            break;
        }
        let v = (u32::from(dbuf[0]) << 18)
            | (u32::from(dbuf[1]) << 12)
            | (u32::from(dbuf[2]) << 6)
            | u32::from(dbuf[3]);
        let b = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        match dlen {
            4 => out.extend_from_slice(&b),
            3 => {
                if b[2] != 0 {
                    // Strict mode is off in StdEncoding: trailing bits are ignored.
                }
                out.extend_from_slice(&b[..2]);
            }
            2 => out.push(b[0]),
            _ => {}
        }
        if dlen < 4 {
            return Ok(out);
        }
    }
}

/// `base64.StdEncoding.EncodeToString` (how `json.Marshal` writes a `[]byte`).
// go-std: encoding/base64.Encoding.Encode L1 (StdEncoding)
pub fn base64_std_encode(src: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(src.len().div_ceil(3) * 4);
    for c in src.chunks(3) {
        let v = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(char::from(T[(v >> 18) as usize & 63]));
        out.push(char::from(T[(v >> 12) as usize & 63]));
        out.push(if c.len() > 1 {
            char::from(T[(v >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            char::from(T[v as usize & 63])
        } else {
            '='
        });
    }
    out
}

/// `floatEncoder.encode` with its error: `json.Marshal` fails on NaN and ±Inf
/// (`*json.UnsupportedValueError`, message `json: unsupported value: NaN`).
// go-std: encoding/json.floatEncoder.encode L1 (bits 64, with the unsupported-value error)
pub fn encode_float(out: &mut String, f: f64) -> Result<(), String> {
    if f.is_nan() || f.is_infinite() {
        return Err(format!(
            "json: unsupported value: {}",
            if f.is_nan() {
                "NaN"
            } else if f > 0.0 {
                "+Inf"
            } else {
                "-Inf"
            }
        ));
    }
    out.push_str(&format_float(f));
    Ok(())
}

/// `json.Marshal` of an `interface{}` value (compact), failing on NaN / ±Inf like Go.
// go-std: encoding/json.encodeState.reflectValue L2 (generic values)
pub fn encode_value(out: &mut String, v: &Value) -> Result<(), String> {
    match v {
        Value::Number(f) => encode_float(out, *f),
        Value::Array(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_value(out, e)?;
            }
            out.push(']');
            Ok(())
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, e)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                append_string(out, k);
                out.push(':');
                encode_value(out, e)?;
            }
            out.push('}');
            Ok(())
        }
        other => {
            out.push_str(&marshal(other));
            Ok(())
        }
    }
}
