//! go-file: pkg/types/json_binary.go
//!
//! The binary JSON value (exception A4): layout, accessors, text output, text input and the
//! constructors. The bytes produced are the same as Go's (A4 spec 3.1): the constructors
//! follow Go's layout decisions line by line.
//!
//! Go's `BinaryJSON` is a slice header (`TypeCode` + `Value []byte`) that aliases the bytes it
//! was read from; here [`BinaryJSON`] holds a `Cow<'a, [u8]>`, borrowed when it is a view into
//! another value or a chunk column and owned when it was built. Go's `any` argument of
//! `CreateBinaryJSON` is the [`JSONValue`] enum (the Go types `appendBinaryJSON` switches on).
//!
//! Accessors index the bytes the way Go does: a corrupted value panics in both (Go's panic is
//! recovered at the statement level, Rust's in `spawn_with_recover`; A4 spec 8.5).

#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::strconv::format_float;
use tidb_base::{Error, mysql, terror};

use super::json_binary_functions::{build_binary_json_array, quote_json_string};
use super::json_constants::*;
use crate::core_time::CoreTime;
use crate::fsp::DefaultFsp;
use crate::gostd::{parse_float, parse_int, parse_uint};
use crate::time::{Duration, Time, new_time};

/// `jsonZero`: `CreateBinaryJSON(uint64(0))`, the value `IsZero` compares with.
// glue: the jsonZero package variable (CreateBinaryJSON(uint64(0)))
pub(crate) fn json_zero() -> BinaryJSON<'static> {
    BinaryJSON::owned(JSONTypeCodeUint64, 0u64.to_le_bytes().to_vec())
}

pub(crate) const maxJSONDepth: isize = 100;

/// BinaryJSON represents a binary encoded JSON object.
/// It can be randomly accessed without deserialization.
///
/// There is no `PartialEq`/`Ord`/`Hash`: JSON values compare with [`compare_binary_json`]
/// and hash with [`BinaryJSON::hash_value`] (A4 spec 5.2).
///
/// [`compare_binary_json`]: super::compare_binary_json
#[derive(Clone, Debug, Default)]
pub struct BinaryJSON<'a> {
    /// `TypeCode`.
    pub type_code: JSONTypeCode,
    /// `Value`.
    pub value: Cow<'a, [u8]>,
}

/// Opaque represents a raw binary type
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Opaque<'a> {
    /// TypeCode is the same with database type code
    pub type_code: u8,
    /// Buf is the underlying bytes of the data
    pub buf: Cow<'a, [u8]>,
}

/// The Go values `CreateBinaryJSON` / `appendBinaryJSON` accept (`in any`). Go's `map[string]any`
/// is a [`BTreeMap`] keyed by the key bytes: its iteration order is the byte order Go sorts the
/// fields into, and assigning a key twice keeps the last value, as in a Go map.
#[derive(Clone, Debug)]
pub enum JSONValue<'a> {
    /// `nil`.
    Nil,
    /// `bool`.
    Bool(bool),
    /// `int64`.
    Int64(i64),
    /// `uint64`.
    Uint64(u64),
    /// `float64`.
    Float64(f64),
    /// `json.Number` (the literal text).
    Number(String),
    /// `string` (Go strings are bytes).
    String(Vec<u8>),
    /// `BinaryJSON`.
    BinaryJSON(BinaryJSON<'a>),
    /// `[]any`.
    Array(Vec<JSONValue<'a>>),
    /// `map[string]any`.
    Object(BTreeMap<Vec<u8>, JSONValue<'a>>),
    /// `Opaque`.
    Opaque(Opaque<'a>),
    /// `Time`.
    Time(Time),
    /// `Duration`.
    Duration(Duration),
}

impl<'a> BinaryJSON<'a> {
    /// A value that borrows its bytes (Go: `BinaryJSON{TypeCode: tc, Value: b}` on a slice).
    // glue: constructor of the Go struct literal
    pub fn borrowed(type_code: JSONTypeCode, value: &'a [u8]) -> BinaryJSON<'a> {
        BinaryJSON {
            type_code,
            value: Cow::Borrowed(value),
        }
    }

    /// A value that owns its bytes.
    // glue: constructor of the Go struct literal
    pub fn owned(type_code: JSONTypeCode, value: Vec<u8>) -> BinaryJSON<'static> {
        BinaryJSON {
            type_code,
            value: Cow::Owned(value),
        }
    }

    /// A view of the same bytes (Go copies the slice header).
    // glue: Go value semantics of the slice header
    pub fn as_ref(&self) -> BinaryJSON<'_> {
        BinaryJSON::borrowed(self.type_code, &self.value)
    }

    /// The value with owned bytes.
    // glue: lifetime conversion (Go has no lifetimes)
    pub fn into_owned(self) -> BinaryJSON<'static> {
        BinaryJSON::owned(self.type_code, self.value.into_owned())
    }

    /// The address of the first value byte, Go's `&bj.Value[0]` (identity of a sub-value inside
    /// one document, used by `Extract` and `binaryModifier`).
    // glue: Go's &bj.Value[0] pointer identity
    pub(crate) fn ptr(&self) -> usize {
        // Go takes &Value[0], which panics on an empty value; values are never empty.
        let _ = self.value[0];
        self.value.as_ptr() as usize
    }

    /// String implements fmt.Stringer interface.
    // go: BinaryJSON.String L1 [M-ERR]
    pub fn string(&self) -> Vec<u8> {
        match self.marshal_json() {
            Ok(out) => out,
            Err(err) => {
                terror::log(Some(&err));
                Vec::new()
            }
        }
    }

    /// Copy makes a copy of the BinaryJSON
    // go: BinaryJSON.Copy L1
    pub fn copy(&self) -> BinaryJSON<'static> {
        BinaryJSON::owned(self.type_code, self.value.to_vec())
    }

    /// MarshalJSON implements the json.Marshaler interface.
    // go: BinaryJSON.MarshalJSON L1 [M-ERR]
    pub fn marshal_json(&self) -> Result<Vec<u8>, Error> {
        let buf = Vec::with_capacity(self.value.len() * 3 / 2);
        self.marshal_to(buf)
    }

    // go: BinaryJSON.marshalTo L1 [M-ERR]
    pub(crate) fn marshal_to(&self, buf: Vec<u8>) -> Result<Vec<u8>, Error> {
        match self.type_code {
            JSONTypeCodeOpaque => Ok(json_marshal_opaque_to(buf, &self.get_opaque())),
            JSONTypeCodeString => Ok(json_marshal_string_to(buf, self.get_string())),
            JSONTypeCodeLiteral => Ok(json_marshal_literal_to(buf, self.value[0])),
            JSONTypeCodeInt64 => {
                let mut buf = buf;
                buf.extend_from_slice(self.get_int64().to_string().as_bytes());
                Ok(buf)
            }
            JSONTypeCodeUint64 => {
                let mut buf = buf;
                buf.extend_from_slice(self.get_uint64().to_string().as_bytes());
                Ok(buf)
            }
            JSONTypeCodeFloat64 => self.marshal_float64_to(buf),
            JSONTypeCodeArray => self.marshal_array_to(buf),
            JSONTypeCodeObject => self.marshal_obj_to(buf),
            JSONTypeCodeDate | JSONTypeCodeDatetime | JSONTypeCodeTimestamp => {
                Ok(json_marshal_time_to(buf, self.get_time()))
            }
            JSONTypeCodeDuration => Ok(json_marshal_duration_to(buf, self.get_duration())),
            _ => Ok(buf),
        }
    }

    /// IsZero return a boolean indicate whether BinaryJSON is Zero
    // go: BinaryJSON.IsZero L1
    pub fn is_zero(&self) -> bool {
        // This behavior is different on MySQL 5.7 and 8.0
        //
        // In MySQL 5.7, most of these non-integer values are 0, and return a warning:
        // "Invalid JSON value for CAST to INTEGER from column j"
        //
        // In MySQL 8, most of these non-integer values are not zero, with a warning:
        // > "Evaluating a JSON value in SQL boolean context does an implicit comparison
        // > against JSON integer 0; if this is not what you want, consider converting
        // > JSON to a SQL numeric type with JSON_VALUE RETURNING"
        //
        // TODO: return a warning as MySQL 8 does
        super::compare_binary_json(self.as_ref(), json_zero()) == 0
    }

    /// GetInt64 gets the int64 value.
    // go: BinaryJSON.GetInt64 L1
    pub fn get_int64(&self) -> i64 {
        self.get_uint64() as i64
    }

    /// GetUint64 gets the uint64 value.
    // go: BinaryJSON.GetUint64 L1
    pub fn get_uint64(&self) -> u64 {
        le_u64(&self.value)
    }

    /// GetFloat64 gets the float64 value.
    // go: BinaryJSON.GetFloat64 L1
    pub fn get_float64(&self) -> f64 {
        f64::from_bits(self.get_uint64())
    }

    /// GetString gets the string value.
    // go: BinaryJSON.GetString L1
    pub fn get_string(&self) -> &[u8] {
        let (str_len, len_len) = uvarint(&self.value);
        go_slice(&self.value, len_len, len_len + str_len as isize)
    }

    /// GetOpaque gets the opaque value
    // go: BinaryJSON.GetOpaque L1
    pub fn get_opaque(&self) -> Opaque<'_> {
        let typ = self.value[0];

        let (str_len, len_len) = uvarint(&self.value[1..]);
        let buf_start = len_len + 1;
        Opaque {
            type_code: typ,
            buf: Cow::Borrowed(go_slice(
                &self.value,
                buf_start,
                buf_start + str_len as isize,
            )),
        }
    }

    /// GetTime gets the time value with default fsp
    ///
    /// Deprecated: use GetTimeWithFsp instead. The `BinaryJSON` doesn't contain the fsp information, so the caller
    /// should always provide the fsp.
    // go: BinaryJSON.GetTime L1
    pub fn get_time(&self) -> Time {
        self.get_time_with_fsp(DefaultFsp)
    }

    /// GetTimeWithFsp gets the time value with given fsp
    // go: BinaryJSON.GetTimeWithFsp L1
    pub fn get_time_with_fsp(&self, fsp: isize) -> Time {
        let core_time = CoreTime(self.get_uint64());

        let mut tp = mysql::TypeDate;
        if self.type_code == JSONTypeCodeDatetime {
            tp = mysql::TypeDatetime;
        } else if self.type_code == JSONTypeCodeTimestamp {
            tp = mysql::TypeTimestamp;
        }

        new_time(core_time, tp, fsp)
    }

    /// GetDuration gets the duration value
    // go: BinaryJSON.GetDuration L1
    pub fn get_duration(&self) -> Duration {
        Duration {
            duration: self.get_int64(),
            fsp: le_u32(&self.value[8..]) as isize,
        }
    }

    /// GetOpaqueFieldType returns the type of opaque value
    // go: BinaryJSON.GetOpaqueFieldType L1
    pub fn get_opaque_field_type(&self) -> u8 {
        self.value[0]
    }

    /// GetKeys gets the keys of the object
    // go: BinaryJSON.GetKeys L1
    pub fn get_keys(&self) -> BinaryJSON<'static> {
        let count = self.get_elem_count();
        let mut ret = Vec::with_capacity(count);
        for i in 0..count {
            ret.push(create_binary_json(JSONValue::String(
                self.object_get_key(i).to_vec(),
            )));
        }
        build_binary_json_array(&ret)
    }

    /// GetElemCount gets the count of Object or Array.
    // go: BinaryJSON.GetElemCount L1
    pub fn get_elem_count(&self) -> usize {
        le_u32(&self.value) as usize
    }

    /// ArrayGetElem gets the element of the index `idx`.
    // go: BinaryJSON.ArrayGetElem L1
    pub fn array_get_elem(&self, idx: usize) -> BinaryJSON<'_> {
        self.val_entry_get(headerSize + idx * valEntrySize)
    }

    // go: BinaryJSON.objectGetKey L1
    pub(crate) fn object_get_key(&self, i: usize) -> &[u8] {
        let key_off = le_u32(&self.value[headerSize + i * keyEntrySize..]) as usize;
        let key_len = le_u16(&self.value[headerSize + i * keyEntrySize + keyLenOff..]) as usize;
        &self.value[key_off..key_off + key_len]
    }

    // go: BinaryJSON.objectGetVal L1
    pub(crate) fn object_get_val(&self, i: usize) -> BinaryJSON<'_> {
        let elem_count = self.get_elem_count();
        self.val_entry_get(headerSize + elem_count * keyEntrySize + i * valEntrySize)
    }

    // go: BinaryJSON.valEntryGet L1
    pub(crate) fn val_entry_get(&self, val_entry_off: usize) -> BinaryJSON<'_> {
        let v: &[u8] = &self.value;
        let tp_code = v[val_entry_off];
        let val_off = le_u32(&v[val_entry_off + valTypeSize..]) as usize;
        match tp_code {
            JSONTypeCodeLiteral => {
                return BinaryJSON::borrowed(
                    JSONTypeCodeLiteral,
                    &v[val_entry_off + valTypeSize..val_entry_off + valTypeSize + 1],
                );
            }
            JSONTypeCodeUint64 | JSONTypeCodeInt64 | JSONTypeCodeFloat64 => {
                return BinaryJSON::borrowed(tp_code, &v[val_off..val_off + 8]);
            }
            JSONTypeCodeString => {
                let (str_len, len_len) = uvarint(&v[val_off..]);
                let total_len = (len_len as u32).wrapping_add(str_len as u32) as usize;
                return BinaryJSON::borrowed(tp_code, &v[val_off..val_off + total_len]);
            }
            JSONTypeCodeOpaque => {
                let (str_len, len_len) = uvarint(&v[val_off + 1..]);
                let total_len = 1u32
                    .wrapping_add(len_len as u32)
                    .wrapping_add(str_len as u32) as usize;
                return BinaryJSON::borrowed(tp_code, &v[val_off..val_off + total_len]);
            }
            JSONTypeCodeDate | JSONTypeCodeDatetime | JSONTypeCodeTimestamp => {
                return BinaryJSON::borrowed(tp_code, &v[val_off..val_off + 8]);
            }
            JSONTypeCodeDuration => {
                return BinaryJSON::borrowed(tp_code, &v[val_off..val_off + 12]);
            }
            _ => {}
        }
        let data_size = le_u32(&v[val_off + dataSizeOff..]) as usize;
        BinaryJSON::borrowed(tp_code, &v[val_off..val_off + data_size])
    }

    // go: BinaryJSON.marshalFloat64To L1 [M-ERR]
    fn marshal_float64_to(&self, mut buf: Vec<u8>) -> Result<Vec<u8>, Error> {
        // NOTE: copied from Go standard library.
        let f = self.get_float64();
        if f.is_infinite() || f.is_nan() {
            return Err(Error::plain(format!(
                "json: unsupported value: {}",
                format_float(f, b'g', -1, 64)
            )));
        }

        // Convert as if by ES6 number to string conversion.
        // This matches most other JSON generators.
        // See golang.org/issue/6384 and golang.org/issue/14135.
        // Like fmt %g, but the exponent cutoffs are different
        // and exponents themselves are not padded to two digits.
        let abs = f.abs();
        let mut ffmt = b'f';
        // Note: Must use float32 comparisons for underlying float32 value to get precise cutoffs right.
        if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
            ffmt = b'e';
        }
        buf.extend_from_slice(format_float(f, ffmt, -1, 64).as_bytes());
        if ffmt == b'e' {
            // clean up e-09 to e-9
            let n = buf.len();
            if n >= 4 && buf[n - 4] == b'e' && buf[n - 3] == b'-' && buf[n - 2] == b'0' {
                buf[n - 2] = buf[n - 1];
                buf.truncate(n - 1);
            }
        }
        Ok(buf)
    }

    // go: BinaryJSON.marshalArrayTo L1 [M-ERR]
    fn marshal_array_to(&self, mut buf: Vec<u8>) -> Result<Vec<u8>, Error> {
        let elem_count = le_u32(&self.value) as usize;
        buf.push(b'[');
        for i in 0..elem_count {
            if i != 0 {
                buf.extend_from_slice(b", ");
            }
            buf = self.array_get_elem(i).marshal_to(buf)?;
        }
        buf.push(b']');
        Ok(buf)
    }

    // go: BinaryJSON.marshalObjTo L1 [M-ERR]
    fn marshal_obj_to(&self, mut buf: Vec<u8>) -> Result<Vec<u8>, Error> {
        let elem_count = le_u32(&self.value) as usize;
        buf.push(b'{');
        for i in 0..elem_count {
            if i != 0 {
                buf.extend_from_slice(b", ");
            }
            buf = json_marshal_string_to(buf, self.object_get_key(i));
            buf.extend_from_slice(b": ");
            buf = self.object_get_val(i).marshal_to(buf)?;
        }
        buf.push(b'}');
        Ok(buf)
    }

    /// UnmarshalJSON implements the json.Unmarshaler interface.
    ///
    /// Go decodes the first JSON value of `data` with a `json.Decoder` (`UseNumber`); the
    /// callers pass text that `json.Valid` accepted. Here the whole input must be one value.
    // go: BinaryJSON.UnmarshalJSON L2 (A4: decodes with the gostd ordered decoder instead of json.Decoder; trailing data after the first value is an error)
    pub fn unmarshal_json(&mut self, data: &[u8]) -> Result<(), Error> {
        let raw = tidb_base::gostd::json::unmarshal_raw(data).map_err(Error::plain)?;
        let input = raw_to_value(raw);
        let new_bj = create_binary_json_with_check(&input)?;
        self.type_code = new_bj.type_code;
        self.value = new_bj.value;
        Ok(())
    }

    /// HashValue converts certain JSON values for aggregate comparisons.
    /// For example int64(3) == float64(3.0)
    /// Other than the numeric condition, this function has to construct a bidirectional map between hash value
    /// and the original representation
    // go: BinaryJSON.HashValue L1
    pub fn hash_value(&self, mut buf: Vec<u8>) -> Vec<u8> {
        match self.type_code {
            JSONTypeCodeInt64 => {
                // Convert to a FLOAT if no precision is lost.
                // In the future, it will be better to convert to a DECIMAL value instead
                // See: https://github.com/pingcap/tidb/issues/9988

                // A double precision float can have 52-bit in fraction part.
                if get_int64_fraction_length(self.get_int64()) <= 52 {
                    buf.push(JSONTypeCodeFloat64);
                    append_binary_float64(&mut buf, self.get_int64() as f64);
                } else {
                    buf.push(self.type_code);
                    buf.extend_from_slice(&self.value);
                }
            }
            JSONTypeCodeUint64 => {
                // A double precision float can have 52-bit in fraction part.
                if get_uint64_fraction_length(self.get_uint64()) <= 52 {
                    buf.push(JSONTypeCodeFloat64);
                    append_binary_float64(&mut buf, self.get_uint64() as f64);
                } else {
                    buf.push(self.type_code);
                    buf.extend_from_slice(&self.value);
                }
            }
            JSONTypeCodeArray => {
                // this hash value is bidirectional, because you can get the element one-by-one
                // and you know the end of it, as the elemCount is also appended here
                buf.push(self.type_code);
                let elem_count = le_u32(&self.value) as usize;
                buf.extend_from_slice(&self.value[0..dataSizeOff]);
                for i in 0..elem_count {
                    buf = self.array_get_elem(i).hash_value(buf);
                }
            }
            JSONTypeCodeObject => {
                // this hash value is bidirectional, because you can get the key using the json
                // string format, and get the value accordingly.
                buf.push(self.type_code);
                let elem_count = le_u32(&self.value) as usize;
                buf.extend_from_slice(&self.value[0..dataSizeOff]);
                for i in 0..elem_count {
                    let key_json =
                        create_binary_json(JSONValue::String(self.object_get_key(i).to_vec()));
                    buf.extend_from_slice(&key_json.value);
                    buf = self.object_get_val(i).hash_value(buf);
                }
            }
            _ => {
                buf.push(self.type_code);
                buf.extend_from_slice(&self.value);
            }
        }
        buf
    }

    /// GetValue return the primitive value of the JSON.
    // go: BinaryJSON.GetValue L1
    pub fn get_value(&self) -> Option<JSONValue<'_>> {
        match self.type_code {
            JSONTypeCodeInt64 => return Some(JSONValue::Int64(self.get_int64())),
            JSONTypeCodeUint64 => return Some(JSONValue::Uint64(self.get_uint64())),
            JSONTypeCodeDuration => return Some(JSONValue::Duration(self.get_duration())),
            JSONTypeCodeFloat64 => return Some(JSONValue::Float64(self.get_float64())),
            JSONTypeCodeString => return Some(JSONValue::String(self.get_string().to_vec())),
            JSONTypeCodeDate | JSONTypeCodeDatetime => {
                return Some(JSONValue::Time(self.get_time()));
            }
            _ => {}
        }
        log::error!("unreachable JSON type type={}", self.type_code);
        None
    }
}

impl fmt::Display for BinaryJSON<'_> {
    // glue: fmt.Stringer through Display (lossy for invalid UTF-8; use string() for bytes)
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.string()))
    }
}

// go: jsonMarshalStringTo L1
pub(crate) fn json_marshal_string_to(mut buf: Vec<u8>, s: &[u8]) -> Vec<u8> {
    // NOTE: copied from Go standard library.
    // NOTE: keep in sync with string above.
    buf.push(b'"');
    let mut start = 0;
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        if b < 0x80 {
            if jsonSafeSet[b as usize] {
                i += 1;
                continue;
            }
            if start < i {
                buf.extend_from_slice(&s[start..i]);
            }
            match b {
                b'\\' | b'"' => buf.extend_from_slice(&[b'\\', b]),
                b'\n' => buf.extend_from_slice(b"\\n"),
                b'\r' => buf.extend_from_slice(b"\\r"),
                b'\t' => buf.extend_from_slice(b"\\t"),
                _ => {
                    // This encodes bytes < 0x20 except for \t, \n and \r.
                    // If escapeHTML is set, it also escapes <, >, and &
                    // because they can lead to security holes when
                    // user-controlled strings are rendered into JSON
                    // and served to some browsers.
                    buf.extend_from_slice(b"\\u00");
                    buf.push(jsonHexChars[(b >> 4) as usize]);
                    buf.push(jsonHexChars[(b & 0xF) as usize]);
                }
            }
            i += 1;
            start = i;
            continue;
        }
        let (c, size) = tidb_base::gostd::decode_rune(&s[i..]);
        if c == '\u{FFFD}' && size == 1 {
            if start < i {
                buf.extend_from_slice(&s[start..i]);
            }
            buf.extend_from_slice(b"\\ufffd");
            i += size;
            start = i;
            continue;
        }
        // U+2028 is LINE SEPARATOR.
        // U+2029 is PARAGRAPH SEPARATOR.
        // They are both technically valid characters in JSON strings,
        // but don't work in JSONP, which has to be evaluated as JavaScript,
        // and can lead to security holes there. It is valid JSON to
        // escape them, so we do so unconditionally.
        // See http://timelessrepo.com/json-isnt-a-javascript-subset for discussion.
        if c == '\u{2028}' || c == '\u{2029}' {
            if start < i {
                buf.extend_from_slice(&s[start..i]);
            }
            buf.extend_from_slice(b"\\u202");
            buf.push(jsonHexChars[(c as u32 & 0xF) as usize]);
            i += size;
            start = i;
            continue;
        }
        i += size;
    }
    if start < s.len() {
        buf.extend_from_slice(&s[start..]);
    }
    buf.push(b'"');
    buf
}

// opaque value will yield "base64:typeXX:<base64 encoded string>"
// go: jsonMarshalOpaqueTo L1
fn json_marshal_opaque_to(mut buf: Vec<u8>, opaque: &Opaque<'_>) -> Vec<u8> {
    let b64 = base64_std_encode(&opaque.buf);
    let output = sprintf(
        "\"base64:type%d:%s\"",
        &[Arg::from(opaque.type_code), Arg::from(b64)],
    );

    // as the base64 string is simple and predictable, it could be appended
    // to the buf directly.
    buf.extend_from_slice(&output);
    buf
}

// go: jsonMarshalLiteralTo L1
fn json_marshal_literal_to(mut b: Vec<u8>, lit_type: u8) -> Vec<u8> {
    match lit_type {
        JSONLiteralFalse => b.extend_from_slice(b"false"),
        JSONLiteralTrue => b.extend_from_slice(b"true"),
        JSONLiteralNil => b.extend_from_slice(b"null"),
        _ => {}
    }
    b
}

// go: jsonMarshalTimeTo L1
fn json_marshal_time_to(mut buf: Vec<u8>, mut time: Time) -> Vec<u8> {
    // printing json datetime/duration will always keep 6 fsp
    time.set_fsp(6);
    buf.extend_from_slice(&quote_json_string(&time.string()));
    buf
}

// go: jsonMarshalDurationTo L1
fn json_marshal_duration_to(mut buf: Vec<u8>, mut duration: Duration) -> Vec<u8> {
    // printing json datetime/duration will always keep 6 fsp
    duration.fsp = 6;
    buf.extend_from_slice(&quote_json_string(&duration.string()));
    buf
}

/// ParseBinaryJSONFromString parses a json from string.
///
/// Documents nested deeper than [`DEEP_NESTING`] are converted on a thread with a large
/// stack: Go's goroutine stacks grow, the recursion here does not (the result is the same).
// go: ParseBinaryJSONFromString L1 [M-ERR]
pub fn parse_binary_json_from_string(s: &[u8]) -> Result<BinaryJSON<'static>, Error> {
    if s.is_empty() {
        return Err(
            ErrInvalidJSONText.gen_with_stack_by_args(vec![Arg::from("The document is empty")])
        );
    }
    let data = s;
    if !json_valid(data) {
        return Err(ErrInvalidJSONText.gen_with_stack_by_args(vec![Arg::from(
            "The document root must not be followed by other values.",
        )]));
    }
    let run = move |data: Vec<u8>| -> Result<BinaryJSON<'static>, Error> {
        let mut bj = BinaryJSON::default();
        match bj.unmarshal_json(&data) {
            Ok(()) => Ok(bj.into_owned()),
            Err(err)
                if !ErrJSONObjectKeyTooLong.equal(&err) && !ErrJSONDocumentTooDeep.equal(&err) =>
            {
                Err(ErrInvalidJSONText.gen_with_stack_by_args(vec![Arg::from(err)]))
            }
            Err(err) => Err(err),
        }
    };
    if max_nesting(data) <= DEEP_NESTING {
        return run(data.to_vec());
    }
    let data = data.to_vec();
    std::thread::Builder::new()
        .name("json-deep".to_owned())
        .stack_size(512 << 20)
        .spawn(move || run(data))
        .expect("spawn the deep JSON thread")
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p))
}

/// Nesting beyond which parsing runs on a large-stack thread.
pub(crate) const DEEP_NESTING: usize = 256;

/// `json.Valid`: one JSON value, nested at most 10000 levels deep (encoding/json's
/// `maxNestingDepth`).
// go-std: encoding/json.Valid L2 (the gostd decoder plus the scanner's nesting limit)
pub fn json_valid(data: &[u8]) -> bool {
    if max_nesting(data) > 10000 {
        return false;
    }
    if max_nesting(data) <= DEEP_NESTING {
        return tidb_base::gostd::json::unmarshal_raw(data).is_ok();
    }
    let data = data.to_vec();
    std::thread::Builder::new()
        .name("json-deep".to_owned())
        .stack_size(512 << 20)
        .spawn(move || tidb_base::gostd::json::unmarshal_raw(&data).is_ok())
        .expect("spawn the deep JSON thread")
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p))
}

/// The deepest nesting of `[`/`{` outside strings (no syntax check).
// glue: nesting depth for the scanner limit and the stack choice
pub(crate) fn max_nesting(data: &[u8]) -> usize {
    let (mut depth, mut max, mut in_str, mut esc) = (0usize, 0usize, false, false);
    for &b in data {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// The `any` value `json.Decoder.Decode` (with `UseNumber`) produces for `raw`.
// glue: gostd ordered tree -> the any value of encoding/json (map: last key wins)
fn raw_to_value(raw: tidb_base::gostd::json::Raw) -> JSONValue<'static> {
    use tidb_base::gostd::json::Raw;
    match raw {
        Raw::Null => JSONValue::Nil,
        Raw::Bool(b) => JSONValue::Bool(b),
        Raw::Number(n) => JSONValue::Number(n),
        Raw::String(s) => JSONValue::String(s.into_bytes()),
        Raw::Array(a) => JSONValue::Array(a.into_iter().map(raw_to_value).collect()),
        Raw::Object(m) => {
            let mut map = BTreeMap::new();
            for (k, v) in m {
                map.insert(k.into_bytes(), raw_to_value(v));
            }
            JSONValue::Object(map)
        }
    }
}

// go: getInt64FractionLength L1 [M-INT]
fn get_int64_fraction_length(i: i64) -> isize {
    let abs_int = if i < 0 {
        i.wrapping_neg() as u64
    } else {
        i as u64
    };
    get_uint64_fraction_length(abs_int)
}

// go: getUint64FractionLength L1
fn get_uint64_fraction_length(i: u64) -> isize {
    let lz = i.leading_zeros() as isize;
    let tz = i.trailing_zeros() as isize;
    // 64 bit removes the leading zero, removes the trailing zero and also removes the first "1".
    let mut fraction = 64 - lz - tz - 1;
    if lz == 64 && tz == 64 {
        fraction = 0;
    }

    fraction
}

/// CreateBinaryJSON creates a BinaryJSON from interface.
// go: CreateBinaryJSON L1
pub fn create_binary_json(in_: JSONValue<'_>) -> BinaryJSON<'static> {
    match create_binary_json_with_check(&in_) {
        Ok(bj) => bj,
        Err(err) => panic!("{err}"),
    }
}

/// CreateBinaryJSONWithCheck creates a BinaryJSON from interface with error check.
// go: CreateBinaryJSONWithCheck L1 [M-ERR]
pub fn create_binary_json_with_check(in_: &JSONValue<'_>) -> Result<BinaryJSON<'static>, Error> {
    let mut buf = Vec::new();
    let type_code = append_binary_json(&mut buf, in_)?;
    let bj = BinaryJSON::owned(type_code, buf);
    // GetElemDepth always returns +1.
    if bj.get_elem_depth() - 1 > maxJSONDepth {
        return Err(ErrJSONDocumentTooDeep.clone());
    }
    Ok(bj)
}

// go: appendBinaryJSON L1 [M-ERR]
pub(crate) fn append_binary_json(
    buf: &mut Vec<u8>,
    in_: &JSONValue<'_>,
) -> Result<JSONTypeCode, Error> {
    let type_code;
    match in_ {
        JSONValue::Nil => {
            type_code = JSONTypeCodeLiteral;
            buf.push(JSONLiteralNil);
        }
        JSONValue::Bool(x) => {
            type_code = JSONTypeCodeLiteral;
            if *x {
                buf.push(JSONLiteralTrue);
            } else {
                buf.push(JSONLiteralFalse);
            }
        }
        JSONValue::Int64(x) => {
            type_code = JSONTypeCodeInt64;
            append_binary_uint64(buf, *x as u64);
        }
        JSONValue::Uint64(x) => {
            type_code = JSONTypeCodeUint64;
            append_binary_uint64(buf, *x);
        }
        JSONValue::Float64(x) => {
            type_code = JSONTypeCodeFloat64;
            append_binary_float64(buf, *x);
        }
        JSONValue::Number(x) => {
            type_code = append_binary_number(buf, x)?;
        }
        JSONValue::String(x) => {
            type_code = JSONTypeCodeString;
            append_binary_string(buf, x);
        }
        JSONValue::BinaryJSON(x) => {
            type_code = x.type_code;
            buf.extend_from_slice(&x.value);
        }
        JSONValue::Array(x) => {
            type_code = JSONTypeCodeArray;
            append_binary_array(buf, x)?;
        }
        JSONValue::Object(x) => {
            type_code = JSONTypeCodeObject;
            append_binary_object(buf, x)?;
        }
        JSONValue::Opaque(x) => {
            type_code = JSONTypeCodeOpaque;
            append_binary_opaque(buf, x);
        }
        JSONValue::Time(x) => {
            let mut tc = JSONTypeCodeDate;
            if x.tp() == mysql::TypeDatetime {
                tc = JSONTypeCodeDatetime;
            } else if x.tp() == mysql::TypeTimestamp {
                tc = JSONTypeCodeTimestamp;
            }
            type_code = tc;
            append_binary_uint64(buf, x.core_time().0);
        }
        JSONValue::Duration(x) => {
            type_code = JSONTypeCodeDuration;
            append_binary_uint64(buf, x.duration as u64);
            append_binary_uint32(buf, x.fsp as u32);
        }
    }
    Ok(type_code)
}

// go: appendZero L1
fn append_zero(buf: &mut Vec<u8>, length: usize) {
    buf.resize(buf.len() + length, 0);
}

// go: appendUint32 L1
fn append_uint32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

// go: appendBinaryNumber L1 [M-ERR]
fn append_binary_number(buf: &mut Vec<u8>, x: &str) -> Result<JSONTypeCode, Error> {
    // The type interpretation process is as follows:
    // - Attempt float64 if it contains Ee.
    // - Next attempt int64
    // - Then uint64 (valid in MySQL JSON, not in JSON decode library)
    // - Then float64
    // - Return an error
    if x.contains("Ee.") {
        let (f64v, err) = parse_float(x);
        if let Err(e) = err {
            return Err(Error::plain(e.to_string()));
        }
        append_binary_float64(buf, f64v);
        return Ok(JSONTypeCodeFloat64);
    }
    let (val, err) = parse_int(x.as_bytes(), 64);
    if err.is_ok() {
        append_binary_uint64(buf, val as u64);
        return Ok(JSONTypeCodeInt64);
    }
    let (val, err) = parse_uint(x.as_bytes(), 64);
    if err.is_ok() {
        append_binary_uint64(buf, val);
        return Ok(JSONTypeCodeUint64);
    }
    let (val, err) = parse_float(x);
    match err {
        Ok(()) => {
            append_binary_float64(buf, val);
            Ok(JSONTypeCodeFloat64)
        }
        Err(e) => Err(Error::plain(e.to_string())),
    }
}

// go: appendBinaryString L1
pub(crate) fn append_binary_string(buf: &mut Vec<u8>, v: &[u8]) {
    put_uvarint(buf, v.len() as u64);
    buf.extend_from_slice(v);
}

// go: appendBinaryOpaque L1
fn append_binary_opaque(buf: &mut Vec<u8>, v: &Opaque<'_>) {
    buf.push(v.type_code);
    put_uvarint(buf, v.buf.len() as u64);
    buf.extend_from_slice(&v.buf);
}

// go: appendBinaryFloat64 L1
fn append_binary_float64(buf: &mut Vec<u8>, v: f64) {
    buf.extend_from_slice(&v.to_bits().to_le_bytes());
}

// go: appendBinaryUint64 L1
fn append_binary_uint64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

// go: appendBinaryUint32 L1
fn append_binary_uint32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

// go: appendBinaryArray L1 [M-ERR]
fn append_binary_array(buf: &mut Vec<u8>, array: &[JSONValue<'_>]) -> Result<(), Error> {
    let doc_off = buf.len();
    append_uint32(buf, array.len() as u32);
    append_zero(buf, dataSizeOff);
    let val_entry_begin = buf.len();
    append_zero(buf, array.len() * valEntrySize);
    for (i, val) in array.iter().enumerate() {
        append_binary_val_elem(buf, doc_off, val_entry_begin + i * valEntrySize, val)?;
    }
    let doc_size = buf.len() - doc_off;
    put_u32(&mut buf[doc_off + dataSizeOff..], doc_size as u32);
    Ok(())
}

// go: appendBinaryValElem L1 [M-ERR]
fn append_binary_val_elem(
    buf: &mut Vec<u8>,
    doc_off: usize,
    val_entry_off: usize,
    val: &JSONValue<'_>,
) -> Result<(), Error> {
    let elem_doc_off = buf.len();
    let type_code = append_binary_json(buf, val)?;
    if type_code == JSONTypeCodeLiteral {
        let lit_code = buf[elem_doc_off];
        buf.truncate(elem_doc_off);
        buf[val_entry_off] = JSONTypeCodeLiteral;
        buf[val_entry_off + 1] = lit_code;
        return Ok(());
    }
    buf[val_entry_off] = type_code;
    let val_off = elem_doc_off - doc_off;
    put_u32(&mut buf[val_entry_off + 1..], val_off as u32);
    Ok(())
}

// go: appendBinaryObject L1 [M-ERR]
fn append_binary_object(
    buf: &mut Vec<u8>,
    x: &BTreeMap<Vec<u8>, JSONValue<'_>>,
) -> Result<(), Error> {
    let doc_off = buf.len();
    append_uint32(buf, x.len() as u32);
    append_zero(buf, dataSizeOff);
    let key_entry_begin = buf.len();
    append_zero(buf, x.len() * keyEntrySize);
    let val_entry_begin = buf.len();
    append_zero(buf, x.len() * valEntrySize);

    // Go collects the map into `fields` and sorts them by key; the BTreeMap iterates in
    // that order.
    for (i, key) in x.keys().enumerate() {
        let key_entry_off = key_entry_begin + i * keyEntrySize;
        let key_off = buf.len() - doc_off;
        let key_len = key.len() as u32;
        if key_len > u16::MAX as u32 {
            return Err(ErrJSONObjectKeyTooLong.clone());
        }
        put_u32(&mut buf[key_entry_off..], key_off as u32);
        buf[key_entry_off + keyLenOff..key_entry_off + keyLenOff + 2]
            .copy_from_slice(&(key_len as u16).to_le_bytes());
        buf.extend_from_slice(key);
    }
    for (i, val) in x.values().enumerate() {
        append_binary_val_elem(buf, doc_off, val_entry_begin + i * valEntrySize, val)?;
    }
    let doc_size = buf.len() - doc_off;
    put_u32(&mut buf[doc_off + dataSizeOff..], doc_size as u32);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// encoding/binary and encoding/base64 helpers.

// go-std: encoding/binary.LittleEndian.Uint64 L1
pub(crate) fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().expect("8 bytes"))
}

// go-std: encoding/binary.LittleEndian.Uint32 L1
pub(crate) fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().expect("4 bytes"))
}

// go-std: encoding/binary.LittleEndian.Uint16 L1
pub(crate) fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes(b[..2].try_into().expect("2 bytes"))
}

// go-std: encoding/binary.LittleEndian.PutUint32 L1
pub(crate) fn put_u32(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
}

/// `binary.Uvarint`: the value and the number of bytes read; `n == 0` if the buffer is too
/// small, `n < 0` (the negated count) on a 64-bit overflow.
// go-std: encoding/binary.Uvarint L1
pub(crate) fn uvarint(buf: &[u8]) -> (u64, isize) {
    let mut x: u64 = 0;
    let mut s: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if i == 10 {
            // Catch byte reads past MaxVarintLen64.
            // See issue https://golang.org/issues/41185
            return (0, -(i as isize + 1)); // overflow
        }
        if b < 0x80 {
            if i == 9 && b > 1 {
                return (0, -(i as isize + 1)); // overflow
            }
            return (x | (u64::from(b) << s), i as isize + 1);
        }
        x |= u64::from(b & 0x7f) << s;
        s += 7;
    }
    (0, 0)
}

// go-std: encoding/binary.PutUvarint L1 (appends instead of writing into a sized buffer)
pub(crate) fn put_uvarint(buf: &mut Vec<u8>, mut x: u64) {
    while x >= 0x80 {
        buf.push(x as u8 | 0x80);
        x >>= 7;
    }
    buf.push(x as u8);
}

/// Go's `s[lo:hi]` with `int` bounds: panics like Go on bounds Go rejects.
// glue: Go slice expression with int indices
pub(crate) fn go_slice(s: &[u8], lo: isize, hi: isize) -> &[u8] {
    assert!(
        0 <= lo && lo <= hi && hi as usize <= s.len(),
        "runtime error: slice bounds out of range [{lo}:{hi}] with length {}",
        s.len()
    );
    &s[lo as usize..hi as usize]
}

// go-std: encoding/base64.StdEncoding.EncodeToString L1
fn base64_std_encode(src: &[u8]) -> String {
    const ENC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(src.len().div_ceil(3) * 4);
    for chunk in src.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ENC[(n >> 18) as usize & 63] as char);
        out.push(ENC[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ENC[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ENC[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
