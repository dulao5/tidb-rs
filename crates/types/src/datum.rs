//! go-file: pkg/types/datum.go
//!
//! Datum is a data box holds different kind of data (block D01.1, second part).
//!
//! Mapping:
//! - `Datum { k, decimal, length, i, collation, b, x }` keeps Go's fields. `b` is
//!   `Option<Vec<u8>>`: Go tells a nil slice from an empty one (`GetBytes`, `Copy`,
//!   `MarshalJSON`'s `omitempty`). `collation` is a `Cow<'static, str>` (collation names
//!   are mostly the static names of the collation tables). `x` holds Go's `any` field:
//!   `*MyDecimal` (boxed, as Go holds a pointer), `Time`, or another dynamic value.
//! - Go's `hack.Slice` / `hack.String` zero-copy views become owned copies (the Rust
//!   Datum owns its bytes); a Go struct copy `d2 := d` that shares `b` and the decimal
//!   pointer is a deep [`Clone`] here (M-POOL: no aliasing mutation is relied upon).
//! - Go `string` values are `&[u8]` / `Vec<u8>` (any byte).
//! - `GetValue() any` / `SetValue(any)` use [`GoValue`], an enum of the Go dynamic types
//!   the functions switch on. `%v` / `%T` of such a value is [`GoValue::arg`].
//! - Functions returning `(T, error)` where T is meaningful with the error return
//!   `(T, Result<(), Error>)` (M-ERRVAL); `errors.Trace` is not ported (M-ERR).
//! - `Hash64ForDatum` (a function variable set by the codec package) is a [`OnceLock`].

#![allow(non_upper_case_globals)]

use std::any::Any;
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use tidb_base::errors::errorf;
use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::json as gojson;
use tidb_base::gostd::strconv::format_float;
use tidb_base::mysql;
use tidb_parser::gocharset::encoding::{self as genc, Encoding};
use tidb_parser::gocharset::{self, CharsetBin, CharsetUTF8, CollationBin};
use tidb_parser::types::{IHasher, UnspecifiedLength};

use crate::binary_literal::{BinaryLiteral, BitLiteral, HexLiteral, new_binary_literal_from_uint};
use crate::collate::{self, Collator};
use crate::context::{Context, Flags};
use crate::convert::*;
use crate::enum_type::{Enum, parse_enum, parse_enum_value};
use crate::errors::{
    ErrDataTooLong, ErrMBiggerThanD, ErrOverflow, ErrTruncated, ErrTruncatedWrongVal,
    ErrWrongValue, Error, TimeStr, TimestampStr,
};
use crate::etc::{FieldType, is_binary_str, is_type_char, kind_str, overflow, type_str};
use crate::fsp::{DefaultFsp, MaxFsp};
use crate::helper::{get_max_float, truncate_float};
use crate::json::{
    BinaryJSON, ErrInvalidJSONCharset, JSONValue, compare_binary_json, create_binary_json,
    parse_binary_json_from_string,
};
use crate::mydecimal::{MODE_HALF_UP, MY_DECIMAL_STRUCT_SIZE, MyDecimal, new_max_or_min_dec};
use crate::set_type::{Set, parse_set, parse_set_value};
use crate::time::{
    Duration, MaxDatetime, MaxDuration, MaxTime, MaxTimestamp, MinDatetime, MinTime, MinTimestamp,
    Time, ZeroDate, adjust_year_checked, from_date, new_time, parse_datetime_str, parse_duration,
    parse_time, parse_time_from_float_string, parse_time_from_num,
};
use crate::vector::{VectorFloat32, parse_vector_float32, zero_copy_deserialize_vector_float32};

/// Kind constants.
pub const KindNull: u8 = 0;
pub const KindInt64: u8 = 1;
pub const KindUint64: u8 = 2;
pub const KindFloat32: u8 = 3;
pub const KindFloat64: u8 = 4;
pub const KindString: u8 = 5;
pub const KindBytes: u8 = 6;
/// Used for BIT / HEX literals.
pub const KindBinaryLiteral: u8 = 7;
pub const KindMysqlDecimal: u8 = 8;
pub const KindMysqlDuration: u8 = 9;
pub const KindMysqlEnum: u8 = 10;
/// Used for BIT table column values.
pub const KindMysqlBit: u8 = 11;
pub const KindMysqlSet: u8 = 12;
pub const KindMysqlTime: u8 = 13;
pub const KindInterface: u8 = 14;
pub const KindMinNotNull: u8 = 15;
pub const KindMaxValue: u8 = 16;
pub const KindRaw: u8 = 17;
pub const KindMysqlJSON: u8 = 18;
pub const KindVectorFloat32: u8 = 19;

/// Go's `any` field `x` of a Datum.
#[derive(Clone, Debug, Default)]
pub enum DatumX {
    /// `nil`.
    #[default]
    Nil,
    /// `*MyDecimal`.
    Decimal(Box<MyDecimal>),
    /// `Time`.
    Time(Time),
    /// Any other Go value stored with `SetInterface` (boxed: a thin pointer keeps the
    /// Datum at 80 bytes).
    Interface(Box<Arc<dyn Any + Send + Sync>>),
}

/// Datum is a data box holds different kind of data.
/// It has better performance and is easier to use than `interface{}`.
#[derive(Clone, Debug, Default)]
pub struct Datum {
    /// datum kind.
    k: u8,
    /// decimal can hold uint16 values.
    decimal: u16,
    /// length can hold uint32 values.
    length: u32,
    /// i can hold int64 uint64 float64 values.
    i: i64,
    /// collation hold the collation information for string value.
    collation: Cow<'static, str>,
    /// b can hold string or []byte values.
    b: Option<Vec<u8>>,
    /// x hold all other types.
    x: DatumX,
}

/// EmptyDatumSize is the size of empty datum.
/// Go: 72 = 1 + 1 (byte) + 2 (uint16) + 4 (uint32) + 8 (int64) + 16 (string) + 24 ([]byte) + 16 (interface{})
///
/// The Go value: memory accounting (`MemUsage`, `EstimatedMemUsage`) reports Go's numbers,
/// which the statistics and the memory tracker compare against configured quotas.
pub const EmptyDatumSize: i64 = 72;

/// A Go dynamic value given to or returned by the `any` functions of this file
/// (`GetValue`, `SetValue`, `NewDatum`, ...).
#[derive(Debug, Default)]
pub enum GoValue {
    /// `nil`.
    #[default]
    Nil,
    /// `bool`.
    Bool(bool),
    /// `int`.
    Int(i64),
    /// `int64`.
    Int64(i64),
    /// `uint64`.
    Uint64(u64),
    /// `float32`.
    Float32(f32),
    /// `float64`.
    Float64(f64),
    /// `string`.
    String(Vec<u8>),
    /// `[]byte` (`None` is a nil slice).
    Bytes(Option<Vec<u8>>),
    /// `*MyDecimal`.
    Decimal(Box<MyDecimal>),
    /// `Duration`.
    Duration(Duration),
    /// `Enum`.
    Enum(Enum),
    /// `BinaryLiteral`.
    BinaryLiteral(BinaryLiteral),
    /// `BitLiteral`.
    BitLiteral(BitLiteral),
    /// `HexLiteral`.
    HexLiteral(HexLiteral),
    /// `Set`.
    Set(Set),
    /// `BinaryJSON`.
    BinaryJSON(BinaryJSON<'static>),
    /// `Time`.
    Time(Time),
    /// `VectorFloat32`.
    VectorFloat32(VectorFloat32<'static>),
    /// `[]any` (only `NewDatum` looks into it).
    Slice(Vec<GoValue>),
    /// `[]Datum`.
    Datums(Vec<Datum>),
    /// Any other Go value.
    Other(Arc<dyn Any + Send + Sync>),
}

impl Clone for GoValue {
    // glue: copy of a Go dynamic value (VectorFloat32 has no derived Clone)
    fn clone(&self) -> GoValue {
        match self {
            GoValue::Nil => GoValue::Nil,
            GoValue::Bool(v) => GoValue::Bool(*v),
            GoValue::Int(v) => GoValue::Int(*v),
            GoValue::Int64(v) => GoValue::Int64(*v),
            GoValue::Uint64(v) => GoValue::Uint64(*v),
            GoValue::Float32(v) => GoValue::Float32(*v),
            GoValue::Float64(v) => GoValue::Float64(*v),
            GoValue::String(v) => GoValue::String(v.clone()),
            GoValue::Bytes(v) => GoValue::Bytes(v.clone()),
            GoValue::Decimal(v) => GoValue::Decimal(v.clone()),
            GoValue::Duration(v) => GoValue::Duration(*v),
            GoValue::Enum(v) => GoValue::Enum(v.clone()),
            GoValue::BinaryLiteral(v) => GoValue::BinaryLiteral(v.clone()),
            GoValue::BitLiteral(v) => GoValue::BitLiteral(v.clone()),
            GoValue::HexLiteral(v) => GoValue::HexLiteral(v.clone()),
            GoValue::Set(v) => GoValue::Set(v.clone()),
            GoValue::BinaryJSON(v) => GoValue::BinaryJSON(v.clone()),
            GoValue::Time(v) => GoValue::Time(*v),
            GoValue::VectorFloat32(v) => GoValue::VectorFloat32(v.clone()),
            GoValue::Slice(v) => GoValue::Slice(v.clone()),
            GoValue::Datums(v) => GoValue::Datums(v.clone()),
            GoValue::Other(v) => GoValue::Other(v.clone()),
        }
    }
}

/// `time.Duration.Seconds`.
// go-std: time.Duration.Seconds L1
fn duration_seconds(d: i64) -> f64 {
    let sec = d / 1_000_000_000;
    let nsec = d % 1_000_000_000;
    sec as f64 + nsec as f64 / 1e9
}

impl GoValue {
    /// The value as an argument of Go's `fmt` (`%v`, `%T`, ...).
    // glue: a Go dynamic value passed to fmt
    pub fn arg(&self) -> Arg {
        match self {
            GoValue::Nil => Arg::Nil,
            GoValue::Bool(b) => Arg::Bool(*b),
            GoValue::Int(i) => Arg::Int(*i, "int"),
            GoValue::Int64(i) => Arg::Int(*i, "int64"),
            GoValue::Uint64(u) => Arg::Uint(*u, "uint64"),
            GoValue::Float32(f) => Arg::Float(f64::from(*f), 32),
            GoValue::Float64(f) => Arg::Float(*f, 64),
            GoValue::String(s) => Arg::go_string(s.clone()),
            GoValue::Bytes(b) => Arg::Bytes(b.clone().unwrap_or_default()),
            GoValue::Decimal(d) => Arg::Stringer(d.string().into_bytes(), "*types.MyDecimal"),
            GoValue::Duration(d) => Arg::Stringer(d.string(), "types.Duration"),
            GoValue::Enum(e) => Arg::Stringer(e.string(), "types.Enum"),
            GoValue::BinaryLiteral(b) => {
                Arg::Stringer(b.string().into_bytes(), "types.BinaryLiteral")
            }
            GoValue::BitLiteral(b) => Arg::Stringer(fmt_byte_list(&b.0), "types.BitLiteral"),
            GoValue::HexLiteral(b) => Arg::Stringer(fmt_byte_list(&b.0), "types.HexLiteral"),
            GoValue::Set(s) => Arg::Stringer(s.string(), "types.Set"),
            GoValue::BinaryJSON(j) => Arg::Stringer(j.string(), "types.BinaryJSON"),
            GoValue::Time(t) => Arg::Stringer(t.string(), "types.Time"),
            GoValue::VectorFloat32(v) => {
                Arg::Stringer(v.string().into_bytes(), "types.VectorFloat32")
            }
            GoValue::Slice(s) => {
                // %v of a []any: the elements' %v separated by spaces.
                let mut out = b"[".to_vec();
                for (i, v) in s.iter().enumerate() {
                    if i > 0 {
                        out.push(b' ');
                    }
                    out.extend(sprintf("%v", &[v.arg()]));
                }
                out.push(b']');
                Arg::Stringer(out, "[]interface {}")
            }
            GoValue::Datums(ds) => {
                // %v of a []Datum: Datum's String method on every element.
                let mut out = b"[".to_vec();
                for (i, d) in ds.iter().enumerate() {
                    if i > 0 {
                        out.push(b' ');
                    }
                    out.extend(d.string());
                }
                out.push(b']');
                Arg::Stringer(out, "[]types.Datum")
            }
            GoValue::Other(_) => Arg::Stringer(b"<other>".to_vec(), "interface {}"),
        }
    }
}

/// `hack.Slice(s)`: the bytes of a Go string as a slice. The empty string gives a nil
/// slice (`unsafe.Slice(unsafe.StringData(""), 0)`), which `GetRaw` and `Copy` observe.
// go-std: pkg/util/hack.Slice L2 (an owned copy; nil for the empty string)
fn hack_slice(s: Vec<u8>) -> Option<Vec<u8>> {
    if s.is_empty() { None } else { Some(s) }
}

/// `%v` of a Go byte-slice type without a `String` method (`BitLiteral`, `HexLiteral`):
/// the elements in decimal, `[1 2 3]`.
// go-std: fmt %v of a []byte-kind named type L1
fn fmt_byte_list(b: &[u8]) -> Vec<u8> {
    let mut out = b"[".to_vec();
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(x.to_string().as_bytes());
    }
    out.push(b']');
    out
}

/// `cmp.Compare` for floats: NaN is less than any other value and equal to NaN.
// glue: cmp.Compare[float64]
fn cmp_f64(x: f64, y: f64) -> isize {
    let xnan = x.is_nan();
    let ynan = y.is_nan();
    if xnan {
        return if ynan { 0 } else { -1 };
    }
    if ynan {
        return 1;
    }
    if x < y {
        -1
    } else if x > y {
        1
    } else {
        0
    }
}

/// `cmp.Compare` for ordered integers.
// glue: cmp.Compare[int64 | uint64]
fn cmp_ord<T: Ord>(x: T, y: T) -> isize {
    match x.cmp(&y) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// The first error of two (`if err == nil { err = err1 }`).
// glue: Go's `if err == nil { err = err1 }`
fn first_err(err: Result<(), Error>, err1: Result<(), Error>) -> Result<(), Error> {
    if err.is_ok() { err1 } else { err }
}

impl Datum {
    /// The only writer of the byte payload `b` (Go's `d.b`). Every constructor and setter
    /// goes through here or [`Datum::set_b_string`], so a later change of `b` to a shared or
    /// borrowed buffer (`Cow<[u8]>` / `Bytes`, e.g. fed zero-copy from chunk rows; D01.1
    /// review) only touches these two functions.
    // glue: single writer of Datum.b
    #[inline]
    fn set_b(&mut self, b: Option<Vec<u8>>) {
        self.b = b;
    }

    /// `d.b = hack.Slice(s)`: the bytes of a Go string (the empty string gives a nil slice).
    // glue: single writer of Datum.b from a Go string
    #[inline]
    fn set_b_string(&mut self, s: Vec<u8>) {
        self.set_b(hack_slice(s));
    }
}

impl Datum {
    /// Clone create a deep copy of the Datum.
    // go: Datum.Clone L1
    pub fn clone_datum(&self) -> Datum {
        let mut ret = Datum::default();
        self.copy(&mut ret);
        ret
    }

    /// Copy deep copies a Datum into destination.
    // go: Datum.Copy L2 (the derived Clone copies b, the decimal and the time)
    pub fn copy(&self, dst: &mut Datum) {
        *dst = self.clone();
    }

    /// Kind gets the kind of the datum.
    // go: Datum.Kind L1
    #[inline]
    pub fn kind(&self) -> u8 {
        self.k
    }

    /// Collation gets the collation of the datum.
    // go: Datum.Collation L1
    #[inline]
    pub fn collation(&self) -> &str {
        &self.collation
    }

    /// SetCollation sets the collation of the datum.
    // go: Datum.SetCollation L1
    pub fn set_collation(&mut self, collation: impl Into<Cow<'static, str>>) {
        self.collation = collation.into();
    }

    /// Frac gets the frac of the datum.
    // go: Datum.Frac L1
    #[inline]
    pub fn frac(&self) -> isize {
        self.decimal as isize
    }

    /// SetFrac sets the frac of the datum.
    // go: Datum.SetFrac L1
    #[inline]
    pub fn set_frac(&mut self, frac: isize) {
        self.decimal = frac as u16;
    }

    /// Length gets the length of the datum.
    // go: Datum.Length L1
    #[inline]
    pub fn length(&self) -> isize {
        self.length as isize
    }

    /// SetLength sets the length of the datum.
    // go: Datum.SetLength L1
    #[inline]
    pub fn set_length(&mut self, l: isize) {
        self.length = l as u32;
    }

    /// IsNull checks if datum is null.
    // go: Datum.IsNull L1
    #[inline]
    pub fn is_null(&self) -> bool {
        self.k == KindNull
    }

    /// GetInt64 gets int64 value.
    // go: Datum.GetInt64 L1
    #[inline]
    pub fn get_int64(&self) -> i64 {
        self.i
    }

    /// SetInt64 sets int64 value.
    // go: Datum.SetInt64 L1
    #[inline]
    pub fn set_int64(&mut self, i: i64) {
        self.k = KindInt64;
        self.i = i;
    }

    /// GetUint64 gets uint64 value.
    // go: Datum.GetUint64 L1
    #[inline]
    pub fn get_uint64(&self) -> u64 {
        self.i as u64
    }

    /// SetUint64 sets uint64 value.
    // go: Datum.SetUint64 L1
    #[inline]
    pub fn set_uint64(&mut self, i: u64) {
        self.k = KindUint64;
        self.i = i as i64;
    }

    /// GetFloat64 gets float64 value.
    // go: Datum.GetFloat64 L1
    #[inline]
    pub fn get_float64(&self) -> f64 {
        f64::from_bits(self.i as u64)
    }

    /// SetFloat64 sets float64 value.
    // go: Datum.SetFloat64 L1
    #[inline]
    pub fn set_float64(&mut self, f: f64) {
        self.k = KindFloat64;
        self.i = f.to_bits() as i64;
    }

    /// GetFloat32 gets float32 value.
    // go: Datum.GetFloat32 L1
    #[inline]
    pub fn get_float32(&self) -> f32 {
        f64::from_bits(self.i as u64) as f32
    }

    /// SetFloat32 sets float32 value.
    // go: Datum.SetFloat32 L1
    #[inline]
    pub fn set_float32(&mut self, f: f32) {
        self.k = KindFloat32;
        self.i = f64::from(f).to_bits() as i64;
    }

    /// SetFloat32FromF64 sets float32 values from f64
    // go: Datum.SetFloat32FromF64 L1
    #[inline]
    pub fn set_float32_from_f64(&mut self, f: f64) {
        self.k = KindFloat32;
        self.i = f.to_bits() as i64;
    }

    /// GetString gets string value.
    // go: Datum.GetString L1
    #[inline]
    pub fn get_string(&self) -> &[u8] {
        self.b.as_deref().unwrap_or(&[])
    }

    /// GetBinaryStringEncoded gets the string value encoded with given charset.
    // go: Datum.GetBinaryStringEncoded L1
    pub fn get_binary_string_encoded(&self) -> Vec<u8> {
        let coll = match gocharset::get_collation_by_name(self.collation()) {
            Ok(c) => c,
            Err(err) => {
                log::warn!("unknown collation, error: {err}");
                return self.get_string().to_vec();
            }
        };
        let enc = genc::find_encoding_take_utf8_as_noop(&coll.charset_name);
        let (replace, _) = enc.transform(self.get_bytes(), genc::OpEncodeNoErr);
        replace.into_owned()
    }

    /// GetBinaryStringDecoded gets the string value decoded with given charset.
    // go: Datum.GetBinaryStringDecoded L1
    pub fn get_binary_string_decoded(
        &self,
        flags: Flags,
        chs: &str,
    ) -> (Vec<u8>, Result<(), Error>) {
        let (enc, skip) = find_encoding(flags, chs);
        if skip {
            return (self.get_string().to_vec(), Ok(()));
        }
        let (trim, err) = enc.transform(self.get_bytes(), genc::OpDecode);
        (trim.into_owned(), err)
    }

    /// GetStringWithCheck gets the string and checks if it is valid in a given charset.
    // go: Datum.GetStringWithCheck L1
    pub fn get_string_with_check(&self, flags: Flags, chs: &str) -> (Vec<u8>, Result<(), Error>) {
        let (enc, skip) = find_encoding(flags, chs);
        if skip {
            return (self.get_string().to_vec(), Ok(()));
        }
        let str = self.get_bytes();
        if !enc.is_valid(str) {
            let (replace, err) = enc.transform(str, genc::OpReplace);
            return (replace.into_owned(), err);
        }
        (self.get_string().to_vec(), Ok(()))
    }

    /// SetString sets string value.
    // go: Datum.SetString L1
    pub fn set_string(&mut self, s: impl Into<Vec<u8>>, collation: impl Into<Cow<'static, str>>) {
        self.k = KindString;
        self.set_b_string(s.into());
        self.collation = collation.into();
    }

    /// GetBytes gets bytes value.
    // go: Datum.GetBytes L1
    #[inline]
    pub fn get_bytes(&self) -> &[u8] {
        self.b.as_deref().unwrap_or(&[])
    }

    /// The `b` field itself (`None` is Go's nil slice).
    // glue: access to the nil-ness of Go's d.b
    #[inline]
    pub fn raw_b(&self) -> Option<&[u8]> {
        self.b.as_deref()
    }

    /// SetBytes sets bytes value to datum.
    // go: Datum.SetBytes L1
    pub fn set_bytes(&mut self, b: impl Into<Option<Vec<u8>>>) {
        self.k = KindBytes;
        self.set_b(b.into());
        self.collation = Cow::Borrowed(CollationBin);
    }

    /// SetBytesAsString sets bytes value to datum as string type.
    // go: Datum.SetBytesAsString L1
    pub fn set_bytes_as_string(
        &mut self,
        b: impl Into<Option<Vec<u8>>>,
        collation: impl Into<Cow<'static, str>>,
        length: u32,
    ) {
        self.k = KindString;
        self.set_b(b.into());
        self.length = length;
        self.collation = collation.into();
    }

    /// GetInterface gets interface value.
    // go: Datum.GetInterface L2 (the any field as a GoValue)
    pub fn get_interface(&self) -> GoValue {
        match &self.x {
            DatumX::Nil => GoValue::Nil,
            DatumX::Decimal(d) => GoValue::Decimal(d.clone()),
            DatumX::Time(t) => GoValue::Time(*t),
            DatumX::Interface(v) => {
                if let Some(ds) = v.downcast_ref::<Vec<Datum>>() {
                    GoValue::Datums(ds.clone())
                } else if let Some(g) = v.downcast_ref::<GoValue>() {
                    g.clone()
                } else {
                    GoValue::Other(Arc::clone(v))
                }
            }
        }
    }

    /// SetInterface sets interface to datum.
    // go: Datum.SetInterface L2 (the any field as a GoValue)
    pub fn set_interface(&mut self, x: GoValue) {
        self.k = KindInterface;
        self.x = match x {
            GoValue::Nil => DatumX::Nil,
            GoValue::Decimal(d) => DatumX::Decimal(d),
            GoValue::Time(t) => DatumX::Time(t),
            GoValue::Datums(ds) => DatumX::Interface(Box::new(Arc::new(ds))),
            GoValue::Other(v) => DatumX::Interface(Box::new(v)),
            other => DatumX::Interface(Box::new(Arc::new(other))),
        };
    }

    /// SetNull sets datum to nil.
    // go: Datum.SetNull L1
    pub fn set_null(&mut self) {
        self.k = KindNull;
        self.x = DatumX::Nil;
    }

    /// SetMinNotNull sets datum to minNotNull value.
    // go: Datum.SetMinNotNull L1
    pub fn set_min_not_null(&mut self) {
        self.k = KindMinNotNull;
        self.x = DatumX::Nil;
    }

    /// GetBinaryLiteral4Cmp gets Bit value, and remove it's prefix 0 for comparison.
    // go: Datum.GetBinaryLiteral4Cmp L1
    pub fn get_binary_literal4_cmp(&self) -> BinaryLiteral {
        let b = self.get_bytes();
        let bit_len = b.len();
        if bit_len == 0 {
            return BinaryLiteral(b.to_vec());
        }
        for i in 0..bit_len {
            // Remove the prefix 0 in the bit array.
            if b[i] != 0 {
                return BinaryLiteral(b[i..].to_vec());
            }
        }
        // The result is 0x000...00, we just the return 0x00.
        BinaryLiteral(b[bit_len - 1..].to_vec())
    }

    /// GetBinaryLiteral gets Bit value
    // go: Datum.GetBinaryLiteral L1
    pub fn get_binary_literal(&self) -> BinaryLiteral {
        BinaryLiteral(self.get_bytes().to_vec())
    }

    /// GetMysqlBit gets MysqlBit value
    // go: Datum.GetMysqlBit L1
    pub fn get_mysql_bit(&self) -> BinaryLiteral {
        self.get_binary_literal()
    }

    /// SetBinaryLiteral sets Bit value
    // go: Datum.SetBinaryLiteral L1
    pub fn set_binary_literal(&mut self, b: BinaryLiteral) {
        self.k = KindBinaryLiteral;
        self.set_b(Some(b.0));
        self.collation = Cow::Borrowed(CollationBin);
    }

    /// SetMysqlBit sets MysqlBit value
    // go: Datum.SetMysqlBit L1
    pub fn set_mysql_bit(&mut self, b: BinaryLiteral) {
        self.k = KindMysqlBit;
        self.set_b(Some(b.0));
    }

    /// GetMysqlDecimal gets decimal value
    ///
    /// Panics like Go's type assertion when the datum holds no decimal.
    // go: Datum.GetMysqlDecimal L1
    pub fn get_mysql_decimal(&self) -> &MyDecimal {
        match &self.x {
            DatumX::Decimal(d) => d,
            _ => panic!("interface conversion: interface {{}} is not *types.MyDecimal"),
        }
    }

    /// The decimal for writing (Go callers write through the `*MyDecimal` pointer).
    // glue: Go's *MyDecimal held in d.x, mutable
    pub fn get_mysql_decimal_mut(&mut self) -> &mut MyDecimal {
        match &mut self.x {
            DatumX::Decimal(d) => d,
            _ => panic!("interface conversion: interface {{}} is not *types.MyDecimal"),
        }
    }

    /// SetMysqlDecimal sets decimal value
    // go: Datum.SetMysqlDecimal L1
    pub fn set_mysql_decimal(&mut self, b: MyDecimal) {
        self.k = KindMysqlDecimal;
        self.x = DatumX::Decimal(Box::new(b));
    }

    /// GetMysqlDuration gets Duration value
    // go: Datum.GetMysqlDuration L1
    #[inline]
    pub fn get_mysql_duration(&self) -> Duration {
        Duration {
            duration: self.i,
            fsp: (self.decimal as i8) as isize,
        }
    }

    /// SetMysqlDuration sets Duration value
    // go: Datum.SetMysqlDuration L1
    #[inline]
    pub fn set_mysql_duration(&mut self, b: Duration) {
        self.k = KindMysqlDuration;
        self.i = b.duration;
        self.decimal = b.fsp as u16;
    }

    /// GetMysqlEnum gets Enum value
    // go: Datum.GetMysqlEnum L1
    pub fn get_mysql_enum(&self) -> Enum {
        Enum {
            value: self.i as u64,
            name: self.get_string().to_vec(),
        }
    }

    /// SetMysqlEnum sets Enum value
    // go: Datum.SetMysqlEnum L1
    pub fn set_mysql_enum(&mut self, b: Enum, collation: impl Into<Cow<'static, str>>) {
        self.k = KindMysqlEnum;
        self.i = b.value as i64;
        self.collation = collation.into();
        self.set_b_string(b.name);
    }

    /// GetMysqlSet gets Set value
    // go: Datum.GetMysqlSet L1
    pub fn get_mysql_set(&self) -> Set {
        Set {
            value: self.i as u64,
            name: self.get_string().to_vec(),
        }
    }

    /// SetMysqlSet sets Set value
    // go: Datum.SetMysqlSet L1
    pub fn set_mysql_set(&mut self, b: Set, collation: impl Into<Cow<'static, str>>) {
        self.k = KindMysqlSet;
        self.i = b.value as i64;
        self.collation = collation.into();
        self.set_b_string(b.name);
    }

    /// GetMysqlJSON gets json.BinaryJSON value
    // go: Datum.GetMysqlJSON L1
    pub fn get_mysql_json(&self) -> BinaryJSON<'_> {
        BinaryJSON::borrowed(self.i as u8, self.get_bytes())
    }

    /// SetMysqlJSON sets json.BinaryJSON value
    // go: Datum.SetMysqlJSON L1
    pub fn set_mysql_json(&mut self, b: BinaryJSON<'_>) {
        self.k = KindMysqlJSON;
        self.i = i64::from(b.type_code);
        self.set_b(Some(b.value.into_owned()));
    }

    /// SetVectorFloat32 sets VectorFloat32 value
    // go: Datum.SetVectorFloat32 L1
    pub fn set_vector_float32(&mut self, vec: VectorFloat32<'_>) {
        self.k = KindVectorFloat32;
        self.set_b(Some(vec.zero_copy_serialize().to_vec()));
    }

    /// GetVectorFloat32 gets VectorFloat32 value
    // go: Datum.GetVectorFloat32 L1
    pub fn get_vector_float32(&self) -> VectorFloat32<'_> {
        match zero_copy_deserialize_vector_float32(self.get_bytes()) {
            Ok((v, _)) => v,
            Err(err) => panic!("{err}"),
        }
    }

    /// GetMysqlTime gets types.Time value
    ///
    /// Panics like Go's type assertion when the datum holds no time.
    // go: Datum.GetMysqlTime L1
    pub fn get_mysql_time(&self) -> Time {
        match &self.x {
            DatumX::Time(t) => *t,
            _ => panic!("interface conversion: interface {{}} is not types.Time"),
        }
    }

    /// SetMysqlTime sets types.Time value
    // go: Datum.SetMysqlTime L1
    pub fn set_mysql_time(&mut self, b: Time) {
        self.k = KindMysqlTime;
        self.x = DatumX::Time(b);
    }

    /// SetRaw sets raw value.
    // go: Datum.SetRaw L1
    pub fn set_raw(&mut self, b: impl Into<Option<Vec<u8>>>) {
        self.k = KindRaw;
        self.set_b(b.into());
    }

    /// GetRaw gets raw value.
    // go: Datum.GetRaw L1
    pub fn get_raw(&self) -> &[u8] {
        self.get_bytes()
    }

    /// SetAutoID set the auto increment ID according to its int flag.
    /// Don't use it directly, useless wrapped with setDatumAutoIDAndCast.
    // go: Datum.SetAutoID L1
    pub fn set_auto_id(&mut self, id: i64, flag: usize) {
        if mysql::has_unsigned_flag(flag) {
            self.set_uint64(id as u64);
        } else {
            self.set_int64(id);
        }
    }

    /// String returns a human-readable description of Datum. It is intended only for debugging.
    // go: Datum.String L1
    pub fn string(&self) -> Vec<u8> {
        let t = match self.k {
            KindNull => "KindNull",
            KindInt64 => "KindInt64",
            KindUint64 => "KindUint64",
            KindFloat32 => "KindFloat32",
            KindFloat64 => "KindFloat64",
            KindString => "KindString",
            KindBytes => "KindBytes",
            KindBinaryLiteral => "KindBinaryLiteral",
            KindMysqlDecimal => "KindMysqlDecimal",
            KindMysqlDuration => "KindMysqlDuration",
            KindMysqlEnum => "KindMysqlEnum",
            KindMysqlBit => "KindMysqlBit",
            KindMysqlSet => "KindMysqlSet",
            KindMysqlTime => "KindMysqlTime",
            KindInterface => "KindInterface",
            KindMinNotNull => "KindMinNotNull",
            KindMaxValue => "KindMaxValue",
            KindRaw => "KindRaw",
            KindMysqlJSON => "KindMysqlJSON",
            KindVectorFloat32 => "KindVectorFloat32",
            _ => "Unknown",
        };
        let v = self.get_value();
        let arg = match &v {
            GoValue::Bytes(_) | GoValue::String(_) => {
                // We only need the escape functionality of %q, the quoting is not needed,
                // so we trim the \" prefix and suffix here.
                let q = sprintf("%q", &[v.arg()]);
                let q = q.strip_prefix(b"\"").unwrap_or(&q);
                let q = q.strip_suffix(b"\"").unwrap_or(q);
                Arg::go_string(q.to_vec())
            }
            _ => v.arg(),
        };
        sprintf("%v %v", &[Arg::from(t), arg])
    }

    /// GetValue gets the value of the datum of any kind.
    // go: Datum.GetValue L1
    pub fn get_value(&self) -> GoValue {
        match self.k {
            KindInt64 => GoValue::Int64(self.get_int64()),
            KindUint64 => GoValue::Uint64(self.get_uint64()),
            KindFloat32 => GoValue::Float32(self.get_float32()),
            KindFloat64 => GoValue::Float64(self.get_float64()),
            KindString => GoValue::String(self.get_string().to_vec()),
            KindBytes => GoValue::Bytes(Some(self.get_bytes().to_vec())),
            KindMysqlDecimal => GoValue::Decimal(Box::new(*self.get_mysql_decimal())),
            KindMysqlDuration => GoValue::Duration(self.get_mysql_duration()),
            KindMysqlEnum => GoValue::Enum(self.get_mysql_enum()),
            KindBinaryLiteral | KindMysqlBit => GoValue::BinaryLiteral(self.get_binary_literal()),
            KindMysqlSet => GoValue::Set(self.get_mysql_set()),
            KindMysqlJSON => GoValue::BinaryJSON(self.get_mysql_json().into_owned()),
            KindMysqlTime => GoValue::Time(self.get_mysql_time()),
            KindVectorFloat32 => GoValue::VectorFloat32(self.get_vector_float32().clone()),
            _ => self.get_interface(),
        }
    }

    /// TruncatedStringify returns the %v representation of the datum
    /// but truncated (for example, for strings, only first 64 bytes is printed).
    /// This function is useful in contexts like EXPLAIN.
    // go: Datum.TruncatedStringify L1
    pub fn truncated_stringify(&self) -> Vec<u8> {
        match self.k {
            KindString | KindBytes => truncate_string_if_needed(self.get_string()),
            KindMysqlJSON => truncate_string_if_needed(&self.get_mysql_json().string()),
            // Vector supports native efficient truncation.
            KindVectorFloat32 => self.get_vector_float32().truncated_string().into_bytes(),
            KindInt64 => self.get_int64().to_string().into_bytes(),
            KindUint64 => self.get_uint64().to_string().into_bytes(),
            // For other types, no truncation is needed.
            _ => sprintf("%v", &[self.get_value().arg()]),
        }
    }

    /// SetValueWithDefaultCollation sets any kind of value.
    // go: Datum.SetValueWithDefaultCollation L1
    pub fn set_value_with_default_collation(&mut self, val: GoValue) {
        self.set_value_inner(val, mysql::DefaultCollationName);
    }

    /// SetValue sets any kind of value.
    // go: Datum.SetValue L1
    pub fn set_value(&mut self, val: GoValue, tp: &FieldType) {
        let collation = tp.get_collate().to_owned();
        self.set_value_inner(val, collation);
    }

    /// The common body of `SetValue` and `SetValueWithDefaultCollation`.
    // glue: shared switch of SetValue / SetValueWithDefaultCollation
    fn set_value_inner(&mut self, val: GoValue, collation: impl Into<Cow<'static, str>>) {
        match val {
            GoValue::Nil => self.set_null(),
            GoValue::Bool(x) => self.set_int64(i64::from(x)),
            GoValue::Int(x) | GoValue::Int64(x) => self.set_int64(x),
            GoValue::Uint64(x) => self.set_uint64(x),
            GoValue::Float32(x) => self.set_float32(x),
            GoValue::Float64(x) => self.set_float64(x),
            GoValue::String(x) => self.set_string(x, collation),
            GoValue::Bytes(x) => self.set_bytes(x),
            GoValue::Decimal(x) => {
                self.k = KindMysqlDecimal;
                self.x = DatumX::Decimal(x);
            }
            GoValue::Duration(x) => self.set_mysql_duration(x),
            GoValue::Enum(x) => self.set_mysql_enum(x, collation),
            GoValue::BinaryLiteral(x) => self.set_binary_literal(x),
            // Store as BinaryLiteral for Bit and Hex literals
            GoValue::BitLiteral(x) => self.set_binary_literal(BinaryLiteral(x.0)),
            GoValue::HexLiteral(x) => self.set_binary_literal(BinaryLiteral(x.0)),
            GoValue::Set(x) => self.set_mysql_set(x, collation),
            GoValue::BinaryJSON(x) => self.set_mysql_json(x),
            GoValue::Time(x) => self.set_mysql_time(x),
            GoValue::VectorFloat32(x) => self.set_vector_float32(x),
            other => self.set_interface(other),
        }
    }

    /// Hash64 implements base.HashEquals<0th> interface.
    ///
    /// Panics if the codec package did not set [`Hash64ForDatum`] (Go: nil function call).
    // go: Datum.Hash64 L1
    pub fn hash64(&self, h: &mut dyn IHasher) {
        let f = Hash64ForDatum
            .get()
            .expect("invalid memory address or nil pointer dereference");
        f(h, self);
    }

    /// Equals implements base.HashEquals.<1st> interface.
    // go: Datum.Equals L2 (other is *Datum or Datum; None is nil or another type)
    pub fn equals(&self, other: Option<&Datum>) -> bool {
        let Some(d2) = other else {
            return false;
        };
        let ok = self.k == d2.k
            && self.decimal == d2.decimal
            && self.length == d2.length
            && self.i == d2.i
            && self.collation == d2.collation
            && self.get_bytes() == d2.get_bytes();
        if !ok {
            return false;
        }
        // compare x
        match self.k {
            KindMysqlDecimal => self.get_mysql_decimal().compare(d2.get_mysql_decimal()) == 0,
            KindMysqlTime => self.get_mysql_time().compare(&d2.get_mysql_time()) == 0,
            _ => true,
        }
    }

    /// Compare compares datum to another datum.
    /// Notes: don't rely on datum.collation to get the collator, it's tend to buggy.
    // go: Datum.Compare L1
    pub fn compare(
        &self,
        ctx: &Context,
        ad: &Datum,
        comparer: &dyn Collator,
    ) -> (isize, Result<(), Error>) {
        if self.k == KindMysqlJSON && ad.k != KindMysqlJSON {
            let (cmp, err) = ad.compare(ctx, self, comparer);
            return (-cmp, err);
        }
        match ad.k {
            KindNull => {
                if self.k == KindNull {
                    return (0, Ok(()));
                }
                (1, Ok(()))
            }
            KindMinNotNull => {
                if self.k == KindNull {
                    return (-1, Ok(()));
                } else if self.k == KindMinNotNull {
                    return (0, Ok(()));
                }
                (1, Ok(()))
            }
            KindMaxValue => {
                if self.k == KindMaxValue {
                    return (0, Ok(()));
                }
                (-1, Ok(()))
            }
            KindInt64 => self.compare_int64(ctx, ad.get_int64()),
            KindUint64 => self.compare_uint64(ctx, ad.get_uint64()),
            KindFloat32 | KindFloat64 => self.compare_float64(ctx, ad.get_float64()),
            KindString => self.compare_string(ctx, ad.get_string(), comparer),
            KindBytes => self.compare_string(ctx, ad.get_string(), comparer),
            KindMysqlDecimal => self.compare_mysql_decimal(ctx, ad.get_mysql_decimal()),
            KindMysqlDuration => self.compare_mysql_duration(ctx, ad.get_mysql_duration()),
            KindMysqlEnum => self.compare_mysql_enum(ctx, &ad.get_mysql_enum(), comparer),
            KindBinaryLiteral | KindMysqlBit => {
                self.compare_binary_literal(ctx, &ad.get_binary_literal4_cmp(), comparer)
            }
            KindMysqlSet => self.compare_mysql_set(ctx, &ad.get_mysql_set(), comparer),
            KindMysqlJSON => self.compare_mysql_json(ad.get_mysql_json()),
            KindMysqlTime => self.compare_mysql_time(ctx, ad.get_mysql_time()),
            KindVectorFloat32 => self.compare_vector_float32(ctx, &ad.get_vector_float32()),
            _ => (0, Ok(())),
        }
    }

    // go: Datum.compareInt64 L1
    fn compare_int64(&self, ctx: &Context, i: i64) -> (isize, Result<(), Error>) {
        match self.k {
            KindMaxValue => (1, Ok(())),
            KindInt64 => (cmp_ord(self.i, i), Ok(())),
            KindUint64 => {
                if i < 0 || self.get_uint64() > i64::MAX as u64 {
                    return (1, Ok(()));
                }
                (cmp_ord(self.i, i), Ok(()))
            }
            _ => self.compare_float64(ctx, i as f64),
        }
    }

    // go: Datum.compareUint64 L1
    fn compare_uint64(&self, ctx: &Context, u: u64) -> (isize, Result<(), Error>) {
        match self.k {
            KindMaxValue => (1, Ok(())),
            KindInt64 => {
                if self.i < 0 || u > i64::MAX as u64 {
                    return (-1, Ok(()));
                }
                (cmp_ord(self.i, u as i64), Ok(()))
            }
            KindUint64 => (cmp_ord(self.get_uint64(), u), Ok(())),
            _ => self.compare_float64(ctx, u as f64),
        }
    }

    // go: Datum.compareFloat64 L1
    fn compare_float64(&self, ctx: &Context, f: f64) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindInt64 => (cmp_f64(self.i as f64, f), Ok(())),
            KindUint64 => (cmp_f64(self.get_uint64() as f64, f), Ok(())),
            KindFloat32 | KindFloat64 => (cmp_f64(self.get_float64(), f), Ok(())),
            KindString | KindBytes => {
                let (f_val, err) = str_to_float(ctx, self.get_string(), false);
                (cmp_f64(f_val, f), err)
            }
            KindMysqlDecimal => {
                let (f_val, err) = self.get_mysql_decimal().to_float64();
                (cmp_f64(f_val, f), err)
            }
            KindMysqlDuration => {
                let f_val = duration_seconds(self.get_mysql_duration().duration);
                (cmp_f64(f_val, f), Ok(()))
            }
            KindMysqlEnum => {
                let f_val = self.get_mysql_enum().to_number();
                (cmp_f64(f_val, f), Ok(()))
            }
            KindBinaryLiteral | KindMysqlBit => {
                let (val, err) = self.get_binary_literal4_cmp().to_int(ctx);
                let f_val = val as f64;
                (cmp_f64(f_val, f), err)
            }
            KindMysqlSet => {
                let f_val = self.get_mysql_set().to_number();
                (cmp_f64(f_val, f), Ok(()))
            }
            KindMysqlTime => {
                let (f_val, err) = self.get_mysql_time().to_number().to_float64();
                (cmp_f64(f_val, f), err)
            }
            _ => (-1, Ok(())),
        }
    }

    // go: Datum.compareString L1
    fn compare_string(
        &self,
        ctx: &Context,
        s: &[u8],
        comparer: &dyn Collator,
    ) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindString | KindBytes => (comparer.compare(self.get_string(), s), Ok(())),
            KindMysqlDecimal => {
                let mut dec = MyDecimal::default();
                let err = ctx.handle_truncate(dec.from_string(s));
                (self.get_mysql_decimal().compare(&dec), err)
            }
            KindMysqlTime => {
                let (dt, err) = parse_datetime_str(ctx, s);
                (self.get_mysql_time().compare(&dt), err)
            }
            KindMysqlDuration => {
                let (dur, _, err) = parse_duration(ctx, s, MaxFsp);
                (self.get_mysql_duration().compare(dur), err)
            }
            KindMysqlSet => (comparer.compare(&self.get_mysql_set().string(), s), Ok(())),
            KindMysqlEnum => (comparer.compare(&self.get_mysql_enum().string(), s), Ok(())),
            KindBinaryLiteral | KindMysqlBit => (
                comparer.compare(&self.get_binary_literal4_cmp().to_string(), s),
                Ok(()),
            ),
            _ => {
                let (f_val, err) = str_to_float(ctx, s, false);
                if err.is_err() {
                    return (0, err);
                }
                self.compare_float64(ctx, f_val)
            }
        }
    }

    // go: Datum.compareMysqlDecimal L1
    fn compare_mysql_decimal(&self, ctx: &Context, dec: &MyDecimal) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindMysqlDecimal => (self.get_mysql_decimal().compare(dec), Ok(())),
            KindString | KindBytes => {
                let mut d_dec = MyDecimal::default();
                let err = ctx.handle_truncate(d_dec.from_string(self.get_bytes()));
                (d_dec.compare(dec), err)
            }
            _ => {
                // The target is DECIMAL: Go's d.ConvertTo does not write to d here.
                let (d_val, err) =
                    self.convert_to_non_bit(ctx, &new_field_type(mysql::TypeNewDecimal));
                if err.is_err() {
                    return (0, err);
                }
                (d_val.get_mysql_decimal().compare(dec), Ok(()))
            }
        }
    }

    // go: Datum.compareMysqlDuration L1
    fn compare_mysql_duration(&self, ctx: &Context, dur: Duration) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindMysqlDuration => (self.get_mysql_duration().compare(dur), Ok(())),
            KindString | KindBytes => {
                let (d_dur, _, err) = parse_duration(ctx, self.get_string(), MaxFsp);
                (d_dur.compare(dur), err)
            }
            _ => self.compare_float64(ctx, duration_seconds(dur.duration)),
        }
    }

    // go: Datum.compareMysqlEnum L1
    fn compare_mysql_enum(
        &self,
        sc: &Context,
        enum_: &Enum,
        comparer: &dyn Collator,
    ) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindString | KindBytes | KindMysqlEnum | KindMysqlSet => {
                (comparer.compare(self.get_string(), &enum_.string()), Ok(()))
            }
            _ => self.compare_float64(sc, enum_.to_number()),
        }
    }

    // go: Datum.compareBinaryLiteral L1
    fn compare_binary_literal(
        &self,
        ctx: &Context,
        b: &BinaryLiteral,
        comparer: &dyn Collator,
    ) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            // in the String / Bytes case, d is converted to Binary and then compared with b
            KindString | KindBytes | KindBinaryLiteral | KindMysqlBit => (
                comparer.compare(&self.get_binary_literal4_cmp().to_string(), &b.to_string()),
                Ok(()),
            ),
            _ => {
                let (val, err) = b.to_int(ctx);
                if err.is_err() {
                    return (0, err);
                }
                self.compare_float64(ctx, val as f64)
            }
        }
    }

    // go: Datum.compareMysqlSet L1
    fn compare_mysql_set(
        &self,
        ctx: &Context,
        set: &Set,
        comparer: &dyn Collator,
    ) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindString | KindBytes | KindMysqlEnum | KindMysqlSet => {
                (comparer.compare(self.get_string(), &set.string()), Ok(()))
            }
            _ => self.compare_float64(ctx, set.to_number()),
        }
    }

    // go: Datum.compareMysqlJSON L1
    fn compare_mysql_json(&self, target: BinaryJSON<'_>) -> (isize, Result<(), Error>) {
        // json is not equal with NULL
        if self.k == KindNull {
            return (1, Ok(()));
        }
        match self.to_mysql_json() {
            Ok(origin) => (compare_binary_json(origin, target), Ok(())),
            Err(err) => (0, Err(err)),
        }
    }

    // go: Datum.compareMysqlTime L1
    fn compare_mysql_time(&self, ctx: &Context, time: Time) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindString | KindBytes => {
                let (dt, err) = parse_datetime_str(ctx, self.get_string());
                (dt.compare(&time), err)
            }
            KindMysqlTime => (self.get_mysql_time().compare(&time), Ok(())),
            _ => {
                let (f_val, err) = time.to_number().to_float64();
                if err.is_err() {
                    return (0, err);
                }
                self.compare_float64(ctx, f_val)
            }
        }
    }

    // go: Datum.compareVectorFloat32 L1
    fn compare_vector_float32(
        &self,
        _ctx: &Context,
        vec: &VectorFloat32<'_>,
    ) -> (isize, Result<(), Error>) {
        match self.k {
            KindNull | KindMinNotNull => (-1, Ok(())),
            KindMaxValue => (1, Ok(())),
            KindVectorFloat32 => (self.get_vector_float32().compare(vec), Ok(())),
            // Note: We expect cast is applied before compare, when comparing with String and other vector types.
            _ => (
                0,
                Err(tidb_base::errors::new(
                    "cannot compare vector and non-vector, cast is required",
                )),
            ),
        }
    }
}

impl Datum {
    /// ConvertTo converts a datum to the target field type.
    /// change this method need sync modification to type2Kind in rowcodec/types.go
    ///
    /// Panics on a target type Go does not convert to ("should never happen").
    ///
    /// Takes `&mut self` like Go's pointer receiver: converting a `KindInt64` datum to BIT
    /// changes its kind to `KindUint64` (`convertToMysqlBit`), and callers see that
    /// (upstream-pitfall tidb-rs#34: an index join probing a BIT key with `-1` reports
    /// `ErrDataTooLong`, not the `ErrOverflow` it would skip).
    // go: Datum.ConvertTo L1
    pub fn convert_to(&mut self, ctx: &Context, target: &FieldType) -> (Datum, Result<(), Error>) {
        if target.get_type() == mysql::TypeBit && self.k != KindNull {
            return self.convert_to_mysql_bit(ctx, target);
        }
        self.convert_to_non_bit(ctx, target)
    }

    /// `ConvertTo` for every target but BIT: no other conversion writes to the receiver, so
    /// it reads `self` only. Callers that hold `&self` and convert to a fixed non-BIT type
    /// (`compareMysqlDecimal`) use it directly.
    // glue: the shared-receiver part of Datum.ConvertTo
    fn convert_to_non_bit(&self, ctx: &Context, target: &FieldType) -> (Datum, Result<(), Error>) {
        if self.k == KindNull {
            return (Datum::default(), Ok(()));
        }
        match target.get_type() {
            mysql::TypeTiny
            | mysql::TypeShort
            | mysql::TypeInt24
            | mysql::TypeLong
            | mysql::TypeLonglong => {
                let unsigned = mysql::has_unsigned_flag(target.get_flag());
                if unsigned {
                    return self.convert_to_uint(ctx, target);
                }
                self.convert_to_int(ctx, target)
            }
            mysql::TypeFloat | mysql::TypeDouble => self.convert_to_float(ctx, target),
            mysql::TypeBlob
            | mysql::TypeTinyBlob
            | mysql::TypeMediumBlob
            | mysql::TypeLongBlob
            | mysql::TypeString
            | mysql::TypeVarchar
            | mysql::TypeVarString => self.convert_to_string(ctx, target),
            mysql::TypeTimestamp => self.convert_to_mysql_timestamp(ctx, target),
            mysql::TypeDatetime | mysql::TypeDate => self.convert_to_mysql_time(ctx, target),
            mysql::TypeDuration => self.convert_to_mysql_duration(ctx, target),
            mysql::TypeNewDecimal => self.convert_to_mysql_decimal(ctx, target),
            mysql::TypeYear => self.convert_to_mysql_year(ctx, target),
            mysql::TypeEnum => self.convert_to_mysql_enum(ctx, target),
            mysql::TypeBit => unreachable!("convert_to handles BIT before"),
            mysql::TypeSet => self.convert_to_mysql_set(ctx, target),
            mysql::TypeJSON => self.convert_to_mysql_json(target),
            mysql::TypeTiDBVectorFloat32 => self.convert_to_vector_float32(ctx, target),
            mysql::TypeNull => (Datum::default(), Ok(())),
            _ => panic!("should never happen"),
        }
    }

    // go: Datum.convertToFloat L1
    fn convert_to_float(&self, ctx: &Context, target: &FieldType) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let (f, err) = match self.k {
            KindNull => return (ret, Ok(())),
            KindInt64 => (self.get_int64() as f64, Ok(())),
            KindUint64 => (self.get_uint64() as f64, Ok(())),
            KindFloat32 | KindFloat64 => (self.get_float64(), Ok(())),
            KindString | KindBytes => str_to_float(ctx, self.get_string(), false),
            KindMysqlTime => self.get_mysql_time().to_number().to_float64(),
            KindMysqlDuration => self.get_mysql_duration().to_number().to_float64(),
            KindMysqlDecimal => self.get_mysql_decimal().to_float64(),
            KindMysqlSet => (self.get_mysql_set().to_number(), Ok(())),
            KindMysqlEnum => (self.get_mysql_enum().to_number(), Ok(())),
            KindBinaryLiteral | KindMysqlBit => {
                let (val, err1) = self.get_binary_literal().to_int(ctx);
                (val as f64, err1)
            }
            KindMysqlJSON => convert_json_to_float(ctx, &self.get_mysql_json()),
            _ => return invalid_conv(self, target.get_type()),
        };
        let (f, err1) = produce_float_with_specified_tp(f, target);
        let err = first_err(err, err1);
        if target.get_type() == mysql::TypeFloat {
            ret.set_float32(f as f32);
        } else {
            ret.set_float64(f);
        }
        (ret, err)
    }

    // go: Datum.convertToString L1
    fn convert_to_string(&self, ctx: &Context, target: &FieldType) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let mut err: Result<(), Error> = Ok(());
        let s: Vec<u8> = match self.k {
            KindInt64 => self.get_int64().to_string().into_bytes(),
            KindUint64 => self.get_uint64().to_string().into_bytes(),
            KindFloat32 => format_float(self.get_float64(), b'f', -1, 32).into_bytes(),
            KindFloat64 => format_float(self.get_float64(), b'f', -1, 64).into_bytes(),
            KindString | KindBytes => {
                let from_binary = self.collation() == CollationBin;
                let to_binary = target.get_charset() == CharsetBin;
                if from_binary && to_binary {
                    self.get_string().to_vec()
                } else if from_binary {
                    let (s, e) = self.get_binary_string_decoded(ctx.flags(), target.get_charset());
                    err = e;
                    s
                } else if to_binary {
                    self.get_binary_string_encoded()
                } else {
                    let (s, e) = self.get_string_with_check(ctx.flags(), target.get_charset());
                    err = e;
                    s
                }
            }
            KindMysqlTime => self.get_mysql_time().string(),
            KindMysqlDuration => self.get_mysql_duration().string(),
            KindMysqlDecimal => self.get_mysql_decimal().string().into_bytes(),
            KindMysqlEnum => self.get_mysql_enum().string(),
            KindMysqlSet => self.get_mysql_set().string(),
            KindBinaryLiteral => {
                let (s, e) = self.get_binary_string_decoded(ctx.flags(), target.get_charset());
                err = e;
                s
            }
            KindMysqlBit => {
                // https://github.com/pingcap/tidb/issues/31124.
                // Consider converting to uint first.
                let (val, e) = self.get_binary_literal().to_int(ctx);
                // The length of BIT is limited to 64, so this function will never fail / truncated.
                // (Go: intest.AssertNoError(err); the shadowed err does not reach the result.)
                if e.is_err() {
                    self.get_binary_literal().to_string()
                } else {
                    val.to_string().into_bytes()
                }
            }
            KindMysqlJSON => self.get_mysql_json().string(),
            KindVectorFloat32 => self.get_vector_float32().string().into_bytes(),
            _ => return invalid_conv(self, target.get_type()),
        };
        let mut s = s;
        if err.is_ok() {
            let (s1, e) = produce_str_with_specified_tp(s, target, ctx, true);
            s = s1;
            err = e;
        }
        ret.set_string(s, target.get_collate().to_owned());
        if target.get_charset() == CharsetBin {
            ret.k = KindBytes;
        }
        (ret, err)
    }

    // go: Datum.convertToInt L1
    fn convert_to_int(&self, ctx: &Context, target: &FieldType) -> (Datum, Result<(), Error>) {
        let (i64_, err) = self.to_signed_integer(ctx, target.get_type());
        (new_int_datum(i64_), err)
    }

    // go: Datum.convertToUint L1
    fn convert_to_uint(&self, ctx: &Context, target: &FieldType) -> (Datum, Result<(), Error>) {
        let tp = target.get_type();
        let upper_bound = integer_unsigned_upper_bound(tp);
        let mut ret = Datum::default();
        let (val, err) = match self.k {
            KindInt64 => convert_int_to_uint(ctx.flags(), self.get_int64(), upper_bound, tp),
            KindUint64 => convert_uint_to_uint(self.get_uint64(), upper_bound, tp),
            KindFloat32 | KindFloat64 => {
                convert_float_to_uint(ctx.flags(), self.get_float64(), upper_bound, tp)
            }
            KindString | KindBytes => {
                let (val, err1) = str_to_uint(ctx, self.get_string(), false);
                let (val, err) = convert_uint_to_uint(val, upper_bound, tp);
                (val, first_err(err, err1))
            }
            KindMysqlTime => {
                let mut dec = self.get_mysql_time().to_number();
                let err = dec.round_in_place(0, MODE_HALF_UP);
                let (ival, err1) = dec.to_int();
                let err = first_err(err, err1);
                let (val, err1) = convert_int_to_uint(ctx.flags(), ival, upper_bound, tp);
                (val, first_err(err, err1))
            }
            KindMysqlDuration => {
                let mut dec = self.get_mysql_duration().to_number();
                let err = dec.round_in_place(0, MODE_HALF_UP);
                let (val, err1) = convert_decimal_to_uint(&dec, upper_bound, tp);
                (val, first_err(err, err1))
            }
            KindMysqlDecimal => convert_decimal_to_uint(self.get_mysql_decimal(), upper_bound, tp),
            KindMysqlEnum => convert_float_to_uint(
                ctx.flags(),
                self.get_mysql_enum().to_number(),
                upper_bound,
                tp,
            ),
            KindMysqlSet => convert_float_to_uint(
                ctx.flags(),
                self.get_mysql_set().to_number(),
                upper_bound,
                tp,
            ),
            KindBinaryLiteral | KindMysqlBit => {
                let (val, err) = self.get_binary_literal().to_int(ctx);
                if err.is_ok() {
                    convert_uint_to_uint(val, upper_bound, tp)
                } else {
                    (val, err)
                }
            }
            KindMysqlJSON => {
                let (i64_, err) = convert_json_to_int(ctx, &self.get_mysql_json(), true, tp);
                (i64_ as u64, err)
            }
            _ => return invalid_conv(self, target.get_type()),
        };
        ret.set_uint64(val);
        (ret, err)
    }

    // go: Datum.convertToMysqlTimestamp L1
    fn convert_to_mysql_timestamp(
        &self,
        ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let mut fsp = DefaultFsp;
        if target.get_decimal() != UnspecifiedLength {
            fsp = target.get_decimal();
        }
        let (mut t, err) = match self.k {
            KindMysqlTime => {
                let (t, err) = self.get_mysql_time().convert(ctx, target.get_type());
                if err.is_err() {
                    // t might be an invalid Timestamp, but should still be comparable, since same representation (KindMysqlTime)
                    ret.set_mysql_time(t);
                    return (
                        ret,
                        Err(ErrWrongValue.gen_with_stack_by_args(vec![
                            Arg::from(TimestampStr),
                            Arg::go_string(t.string()),
                        ])),
                    );
                }
                t.round_frac(ctx, fsp)
            }
            KindMysqlDuration => {
                let (t, err) = self
                    .get_mysql_duration()
                    .convert_to_time(ctx, mysql::TypeTimestamp);
                if err.is_err() {
                    ret.set_mysql_time(t);
                    return (ret, err);
                }
                t.round_frac(ctx, fsp)
            }
            KindString | KindBytes => parse_time(ctx, self.get_string(), mysql::TypeTimestamp, fsp),
            KindInt64 => parse_time_from_num(ctx, self.get_int64(), mysql::TypeTimestamp, fsp),
            KindMysqlDecimal => parse_time_from_float_string(
                ctx,
                &self.get_mysql_decimal().to_string(),
                mysql::TypeTimestamp,
                fsp,
            ),
            KindMysqlJSON => {
                let j = self.get_mysql_json();
                match j.unquote() {
                    Ok(s) => parse_time(ctx, &s, mysql::TypeTimestamp, fsp),
                    Err(err) => {
                        ret.set_mysql_time(Time::default());
                        return (ret, Err(err));
                    }
                }
            }
            _ => return invalid_conv(self, mysql::TypeTimestamp),
        };
        t.set_type(mysql::TypeTimestamp);
        ret.set_mysql_time(t);
        (ret, err)
    }

    // go: Datum.convertToMysqlTime L1
    fn convert_to_mysql_time(
        &self,
        ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let tp = target.get_type();
        let mut fsp = DefaultFsp;
        if target.get_decimal() != UnspecifiedLength {
            fsp = target.get_decimal();
        }
        let mut ret = Datum::default();
        let (mut t, err) = match self.k {
            KindMysqlTime => {
                let (t, err) = self.get_mysql_time().convert(ctx, tp);
                if err.is_err() {
                    ret.set_mysql_time(t);
                    return (ret, err);
                }
                t.round_frac(ctx, fsp)
            }
            KindMysqlDuration => {
                let (t, err) = self.get_mysql_duration().convert_to_time(ctx, tp);
                if err.is_err() {
                    ret.set_mysql_time(t);
                    return (ret, err);
                }
                t.round_frac(ctx, fsp)
            }
            KindMysqlDecimal => {
                parse_time_from_float_string(ctx, &self.get_mysql_decimal().to_string(), tp, fsp)
            }
            KindString | KindBytes => parse_time(ctx, self.get_string(), tp, fsp),
            KindInt64 => parse_time_from_num(ctx, self.get_int64(), tp, fsp),
            KindUint64 => {
                let int_overflow64 = self.get_int64() < 0;
                if int_overflow64 {
                    let u_num = self.get_uint64().to_string();
                    (
                        *ZeroDate,
                        Err(ErrWrongValue
                            .gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::from(u_num)])),
                    )
                } else {
                    parse_time_from_num(ctx, self.get_int64(), tp, fsp)
                }
            }
            KindMysqlJSON => {
                let j = self.get_mysql_json();
                match j.unquote() {
                    Ok(s) => parse_time(ctx, &s, tp, fsp),
                    Err(err) => {
                        ret.set_mysql_time(Time::default());
                        return (ret, Err(err));
                    }
                }
            }
            _ => return invalid_conv(self, tp),
        };
        if tp == mysql::TypeDate {
            // Truncate hh:mm:ss part if the type is Date.
            t.set_core_time(from_date(t.year(), t.month(), t.day(), 0, 0, 0, 0));
        }
        ret.set_mysql_time(t);
        (ret, err)
    }

    // go: Datum.convertToMysqlDuration L1
    fn convert_to_mysql_duration(
        &self,
        type_ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let tp = target.get_type();
        let mut fsp = DefaultFsp;
        if target.get_decimal() != UnspecifiedLength {
            fsp = target.get_decimal();
        }
        let mut ret = Datum::default();
        match self.k {
            KindMysqlTime => {
                let dur = match self.get_mysql_time().convert_to_duration() {
                    Ok(d) => d,
                    Err(err) => {
                        // Go: ret.SetMysqlDuration(dur) with the zero Duration of the error path.
                        ret.set_mysql_duration(Duration::default());
                        return (ret, Err(err));
                    }
                };
                let (dur, err) = dur.round_frac(fsp, &type_ctx.location());
                ret.set_mysql_duration(dur);
                if err.is_err() {
                    return (ret, err);
                }
            }
            KindMysqlDuration => {
                let (dur, err) = self
                    .get_mysql_duration()
                    .round_frac(fsp, &type_ctx.location());
                ret.set_mysql_duration(dur);
                if err.is_err() {
                    return (ret, err);
                }
            }
            KindInt64 | KindUint64 | KindFloat32 | KindFloat64 | KindMysqlDecimal => {
                // TODO: We need a ParseDurationFromNum to avoid the cost of converting a num to string.
                let time_str = match self.to_string() {
                    Ok(s) => s,
                    Err(err) => return (ret, Err(err)),
                };
                let (time_num, err) = self.to_int64(type_ctx);
                if err.is_err() {
                    return (ret, err);
                }
                // For huge numbers(>'0001-00-00 00-00-00') try full DATETIME in ParseDuration.
                if time_num > MaxDuration && time_num < 10000000000 {
                    // mysql return max in no strict sql mode.
                    ret.set_mysql_duration(Duration {
                        duration: MaxTime,
                        fsp: 0,
                    });
                    return (
                        ret,
                        Err(ErrWrongValue.gen_with_stack_by_args(vec![
                            Arg::from(TimeStr),
                            Arg::go_string(time_str),
                        ])),
                    );
                }
                if time_num < -MaxDuration {
                    return (
                        ret,
                        Err(ErrWrongValue.gen_with_stack_by_args(vec![
                            Arg::from(TimeStr),
                            Arg::go_string(time_str),
                        ])),
                    );
                }
                let (t, _, err) = parse_duration(type_ctx, &time_str, fsp);
                ret.set_mysql_duration(t);
                if err.is_err() {
                    return (ret, err);
                }
            }
            KindString | KindBytes => {
                let (t, _, err) = parse_duration(type_ctx, self.get_string(), fsp);
                ret.set_mysql_duration(t);
                if err.is_err() {
                    return (ret, err);
                }
            }
            KindMysqlJSON => {
                let j = self.get_mysql_json();
                let s = match j.unquote() {
                    Ok(s) => s,
                    Err(err) => return (ret, Err(err)),
                };
                let (t, _, err) = parse_duration(type_ctx, &s, fsp);
                ret.set_mysql_duration(t);
                if err.is_err() {
                    return (ret, err);
                }
            }
            _ => return invalid_conv(self, tp),
        }
        (ret, Ok(()))
    }

    // go: Datum.convertToMysqlDecimal L1
    fn convert_to_mysql_decimal(
        &self,
        ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        ret.set_length(target.get_flen());
        ret.set_frac(target.get_decimal());
        let mut dec = MyDecimal::default();
        let mut err: Result<(), Error> = Ok(());
        match self.k {
            KindInt64 => {
                dec.from_int(self.get_int64());
            }
            KindUint64 => {
                dec.from_uint(self.get_uint64());
            }
            KindFloat32 | KindFloat64 => err = dec.from_float64(self.get_float64()),
            KindString | KindBytes => err = dec.from_string(self.get_bytes()),
            KindMysqlDecimal => dec = *self.get_mysql_decimal(),
            KindMysqlTime => dec = self.get_mysql_time().to_number(),
            KindMysqlDuration => dec = self.get_mysql_duration().to_number(),
            KindMysqlEnum => err = dec.from_float64(self.get_mysql_enum().to_number()),
            KindMysqlSet => err = dec.from_float64(self.get_mysql_set().to_number()),
            KindBinaryLiteral | KindMysqlBit => {
                let (val, err1) = self.get_binary_literal().to_int(ctx);
                err = err1;
                dec.from_uint(val);
            }
            KindMysqlJSON => {
                let (f, err1) = convert_json_to_decimal(ctx, &self.get_mysql_json());
                if err1.is_err() {
                    return (ret, err1);
                }
                dec = f;
            }
            _ => return invalid_conv(self, target.get_type()),
        }
        let (dec1, err1) = produce_dec_with_specified_tp(ctx, dec, target);
        // If there is a error, dec1 may be nil.
        if let Some(dec1) = dec1 {
            dec = dec1;
        }
        err = first_err(err, err1);
        if dec.is_negative() && mysql::has_unsigned_flag(target.get_flag()) {
            dec = MyDecimal::default();
            if err.is_ok() {
                err = Err(ErrOverflow.gen_with_stack_by_args(vec![
                    Arg::from("DECIMAL"),
                    Arg::from(format!("({}, {})", target.get_flen(), target.get_decimal())),
                ]));
            }
        }
        ret.set_mysql_decimal(dec);
        (ret, err)
    }

    /// ConvertToMysqlYear converts a datum to MySQLYear.
    // go: Datum.ConvertToMysqlYear L1
    pub fn convert_to_mysql_year(
        &self,
        ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let mut adjust = false;
        let (mut y, mut err) = match self.k {
            KindString | KindBytes => {
                let s = self.get_string();
                let trim_s = crate::gostd::trim_space(s);
                let (y, err) = str_to_int(ctx, trim_s, false);
                if err.is_err() {
                    ret.set_int64(0);
                    return (ret, err);
                }
                // condition:
                // parsed to 0, not a string of length 4, the first valid char is a 0 digit
                if s.len() != 4 && y == 0 && trim_s.starts_with(b"0") {
                    adjust = true;
                }
                (y, err)
            }
            KindMysqlTime => (self.get_mysql_time().year() as i64, Ok(())),
            KindMysqlDuration => self.get_mysql_duration().convert_to_year(ctx),
            KindMysqlJSON => {
                let (y, err) = convert_json_to_int64(ctx, &self.get_mysql_json(), false);
                if err.is_err() {
                    ret.set_int64(0);
                    return (ret, err);
                }
                (y, err)
            }
            _ => {
                let (r, err) = self.convert_to_int(ctx, &new_field_type(mysql::TypeLonglong));
                ret = r;
                if err.is_err() {
                    let (_, err) = invalid_conv(self, target.get_type());
                    ret.set_int64(0);
                    return (ret, err);
                }
                (ret.get_int64(), err)
            }
        };

        // Duration has been adjusted in `Duration.ConvertToYear()`
        if self.k != KindMysqlDuration {
            (y, err) = adjust_year_checked(y, adjust);
        }
        ret.set_int64(y);
        (ret, err)
    }

    // go: Datum.convertToMysqlBit L1
    fn convert_to_mysql_bit(
        &mut self,
        ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let (mut uint_value, mut err) = match self.k {
            KindString | KindBytes => BinaryLiteral(self.get_bytes().to_vec()).to_int(ctx),
            KindInt64 => {
                // if input kind is int64 (signed), when trans to bit, we need to treat it as unsigned
                self.k = KindUint64;
                let (uint_datum, err1) = self.convert_to_uint(ctx, target);
                (uint_datum.get_uint64(), err1)
            }
            _ => {
                let (uint_datum, err1) = self.convert_to_uint(ctx, target);
                (uint_datum.get_uint64(), err1)
            }
        };
        // Avoid byte size panic, never goto this branch.
        if target.get_flen() <= 0 || target.get_flen() >= 128 {
            return (
                Datum::default(),
                Err(ErrDataTooLong.gen_with_stack(
                    "Data Too Long, field len %d",
                    vec![Arg::Int(target.get_flen() as i64, "int")],
                )),
            );
        }
        if target.get_flen() < 64 && uint_value >= 1u64 << (target.get_flen() as u64) {
            uint_value = (1u64 << (target.get_flen() as u64)) - 1;
            err = Err(ErrDataTooLong.gen_with_stack(
                "Data Too Long, field len %d",
                vec![Arg::Int(target.get_flen() as i64, "int")],
            ));
        }
        let byte_size = (target.get_flen() + 7) >> 3;
        ret.set_mysql_bit(new_binary_literal_from_uint(uint_value, byte_size));
        (ret, err)
    }

    // go: Datum.convertToMysqlEnum L1 [M-ERR]
    fn convert_to_mysql_enum(
        &self,
        ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let elems = target.get_elems().unwrap_or(&[]);
        let (e, err) = match self.k {
            KindString | KindBytes | KindBinaryLiteral => {
                split(parse_enum(elems, self.get_string(), target.get_collate()))
            }
            KindMysqlEnum => {
                if self.i == 0 {
                    // MySQL enum zero value has an empty string name(Enum{Name: '', Value: 0}). It is
                    // different from the normal enum string value(Enum{Name: '', Value: n}, n > 0).
                    (Enum::default(), Ok(()))
                } else {
                    split(parse_enum(
                        elems,
                        &self.get_mysql_enum().name,
                        target.get_collate(),
                    ))
                }
            }
            KindMysqlSet => split(parse_enum(
                elems,
                &self.get_mysql_set().name,
                target.get_collate(),
            )),
            _ => {
                let (uint_datum, err) = self.convert_to_uint(ctx, target);
                if err.is_ok() {
                    split(parse_enum_value(elems, uint_datum.get_uint64()))
                } else {
                    // errors.Wrap(ErrTruncated, "convert to MySQL enum failed: "+err.Error()):
                    // the cause (M-ERR).
                    (Enum::default(), Err(ErrTruncated.clone()))
                }
            }
        };
        ret.set_mysql_enum(e, target.get_collate().to_owned());
        (ret, err)
    }

    // go: Datum.convertToMysqlSet L1 [M-ERR]
    fn convert_to_mysql_set(
        &self,
        ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let elems = target.get_elems().unwrap_or(&[]);
        let (s, err) = match self.k {
            KindString | KindBytes | KindBinaryLiteral => {
                split(parse_set(elems, self.get_string(), target.get_collate()))
            }
            KindMysqlEnum => split(parse_set(
                elems,
                &self.get_mysql_enum().name,
                target.get_collate(),
            )),
            KindMysqlSet => split(parse_set(
                elems,
                &self.get_mysql_set().name,
                target.get_collate(),
            )),
            KindVectorFloat32 => return invalid_conv(self, mysql::TypeSet),
            _ => {
                let (uint_datum, err) = self.convert_to_uint(ctx, target);
                if err.is_ok() {
                    split(parse_set_value(elems, uint_datum.get_uint64()))
                } else {
                    (Set::default(), err)
                }
            }
        };
        // errors.Wrap(ErrTruncated, "convert to MySQL set failed: "+err.Error()): the cause (M-ERR).
        let err = match err {
            Err(_) => Err(ErrTruncated.clone()),
            Ok(()) => Ok(()),
        };
        ret.set_mysql_set(s, target.get_collate().to_owned());
        (ret, err)
    }

    // go: Datum.convertToMysqlJSON L1
    fn convert_to_mysql_json(&self, _target: &FieldType) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        let mut err: Result<(), Error> = Ok(());
        match self.k {
            KindString | KindBytes => match parse_binary_json_from_string(self.get_string()) {
                Ok(j) => ret.set_mysql_json(j),
                Err(e) => err = Err(e),
            },
            KindMysqlSet | KindMysqlEnum => match self.to_string() {
                Ok(s) => match parse_binary_json_from_string(&s) {
                    Ok(j) => ret.set_mysql_json(j),
                    Err(e) => err = Err(e),
                },
                Err(e) => err = Err(e),
            },
            KindInt64 => ret.set_mysql_json(create_binary_json(JSONValue::Int64(self.get_int64()))),
            KindUint64 => {
                ret.set_mysql_json(create_binary_json(JSONValue::Uint64(self.get_uint64())))
            }
            KindFloat32 | KindFloat64 => {
                ret.set_mysql_json(create_binary_json(JSONValue::Float64(self.get_float64())))
            }
            KindMysqlDecimal => {
                let (f64_, e) = self.get_mysql_decimal().to_float64();
                match e {
                    Ok(()) => ret.set_mysql_json(create_binary_json(JSONValue::Float64(f64_))),
                    Err(e) => err = Err(e),
                }
            }
            KindMysqlJSON => ret = self.clone(),
            KindMysqlTime => {
                let tm = self.get_mysql_time();
                ret.set_mysql_json(create_binary_json(JSONValue::Time(tm)));
            }
            KindMysqlDuration => {
                let dur = self.get_mysql_duration();
                ret.set_mysql_json(create_binary_json(JSONValue::Duration(dur)));
            }
            KindBinaryLiteral => {
                err = Err(ErrInvalidJSONCharset.gen_with_stack_by_args(vec![Arg::from(CharsetBin)]))
            }
            _ => match self.to_string() {
                // TODO: fix precision of MysqlTime. For example,
                // On MySQL 5.7 CAST(NOW() AS JSON) -> "2011-11-11 11:11:11.111111",
                // But now we can only return "2011-11-11 11:11:11".
                Ok(s) => ret.set_mysql_json(create_binary_json(JSONValue::String(s))),
                Err(e) => err = Err(e),
            },
        }
        (ret, err)
    }

    // go: Datum.convertToVectorFloat32 L1
    fn convert_to_vector_float32(
        &self,
        _ctx: &Context,
        target: &FieldType,
    ) -> (Datum, Result<(), Error>) {
        let mut ret = Datum::default();
        match self.k {
            KindVectorFloat32 => {
                let v = self.get_vector_float32();
                if let Err(err) = v.check_dims_fit_column(target.get_flen()) {
                    return (ret, Err(err));
                }
                ret = self.clone();
            }
            KindString | KindBytes => {
                let v = match parse_vector_float32(self.get_string()) {
                    Ok(v) => v,
                    Err(err) => return (ret, Err(err)),
                };
                if let Err(err) = v.check_dims_fit_column(target.get_flen()) {
                    return (ret, Err(err));
                }
                ret.set_vector_float32(v);
            }
            _ => return invalid_conv(self, mysql::TypeTiDBVectorFloat32),
        }
        (ret, Ok(()))
    }

    /// ToBool converts to a bool.
    /// We will use 1 for true, and 0 for false.
    // go: Datum.ToBool L1
    pub fn to_bool(&self, ctx: &Context) -> (i64, Result<(), Error>) {
        let mut err: Result<(), Error> = Ok(());
        let is_zero = match self.kind() {
            KindInt64 => self.get_int64() == 0,
            KindUint64 => self.get_uint64() == 0,
            KindFloat32 => self.get_float64() == 0.0,
            KindFloat64 => self.get_float64() == 0.0,
            KindString | KindBytes => {
                let (i_val, err1) = str_to_float(ctx, self.get_string(), false);
                err = err1;
                i_val == 0.0
            }
            KindMysqlTime => self.get_mysql_time().is_zero(),
            KindMysqlDuration => self.get_mysql_duration().duration == 0,
            KindMysqlDecimal => self.get_mysql_decimal().is_zero(),
            KindMysqlEnum => self.get_mysql_enum().to_number() == 0.0,
            KindMysqlSet => self.get_mysql_set().to_number() == 0.0,
            KindBinaryLiteral | KindMysqlBit => {
                let (val, err1) = self.get_binary_literal().to_int(ctx);
                err = err1;
                val == 0
            }
            KindMysqlJSON => {
                let val = self.get_mysql_json();
                val.is_zero()
            }
            KindVectorFloat32 => self.get_vector_float32().is_zero_value(),
            _ => {
                let v = self.get_value().arg();
                return (
                    0,
                    Err(errorf(
                        "cannot convert %v(type %T) to bool",
                        &[v.clone(), v],
                    )),
                );
            }
        };
        let ret = if is_zero { 0 } else { 1 };
        (ret, err)
    }

    /// ToDecimal converts to a decimal.
    // go: Datum.ToDecimal L1
    pub fn to_decimal(&self, ctx: &Context) -> (MyDecimal, Result<(), Error>) {
        match self.kind() {
            KindMysqlTime => (self.get_mysql_time().to_number(), Ok(())),
            KindMysqlDuration => (self.get_mysql_duration().to_number(), Ok(())),
            _ => {
                let (dec, err) = convert_datum_to_decimal(ctx, self);
                (dec.unwrap_or_default(), err)
            }
        }
    }

    /// ToInt64 converts to a int64.
    // go: Datum.ToInt64 L1
    pub fn to_int64(&self, ctx: &Context) -> (i64, Result<(), Error>) {
        if self.kind() == KindMysqlBit {
            let (uint_val, err) = self.get_binary_literal().to_int(ctx);
            return (uint_val as i64, err);
        }
        self.to_signed_integer(ctx, mysql::TypeLonglong)
    }

    // go: Datum.toSignedInteger L1
    fn to_signed_integer(&self, ctx: &Context, tp: u8) -> (i64, Result<(), Error>) {
        let lower_bound = integer_signed_lower_bound(tp);
        let upper_bound = integer_signed_upper_bound(tp);
        match self.kind() {
            KindInt64 => convert_int_to_int(self.get_int64(), lower_bound, upper_bound, tp),
            KindUint64 => convert_uint_to_int(self.get_uint64(), upper_bound, tp),
            KindFloat32 => {
                convert_float_to_int(f64::from(self.get_float32()), lower_bound, upper_bound, tp)
            }
            KindFloat64 => convert_float_to_int(self.get_float64(), lower_bound, upper_bound, tp),
            KindString | KindBytes => {
                let (i_val, err) = str_to_int(ctx, self.get_string(), false);
                let (i_val, err2) = convert_int_to_int(i_val, lower_bound, upper_bound, tp);
                (i_val, first_err(err, err2))
            }
            KindMysqlTime => {
                // 2011-11-10 11:11:11.999999 -> 20111110111112
                // 2011-11-10 11:59:59.999999 -> 20111110120000
                let (t, err) = self.get_mysql_time().round_frac(ctx, DefaultFsp);
                if err.is_err() {
                    return (0, err);
                }
                let (ival, err) = t.to_number().to_int();
                let (ival, err2) = convert_int_to_int(ival, lower_bound, upper_bound, tp);
                (ival, first_err(err, err2))
            }
            KindMysqlDuration => {
                // 11:11:11.999999 -> 111112
                // 11:59:59.999999 -> 120000
                let (dur, err) = self
                    .get_mysql_duration()
                    .round_frac(DefaultFsp, &ctx.location());
                if err.is_err() {
                    return (0, err);
                }
                let (ival, err) = dur.to_number().to_int();
                let (ival, err2) = convert_int_to_int(ival, lower_bound, upper_bound, tp);
                (ival, first_err(err, err2))
            }
            KindMysqlDecimal => {
                let mut to = MyDecimal::default();
                let err = self.get_mysql_decimal().round(&mut to, 0, MODE_HALF_UP);
                let (ival, err1) = to.to_int();
                let err = first_err(err, err1);
                let (ival, err2) = convert_int_to_int(ival, lower_bound, upper_bound, tp);
                (ival, first_err(err, err2))
            }
            KindMysqlEnum => {
                let fval = self.get_mysql_enum().to_number();
                convert_float_to_int(fval, lower_bound, upper_bound, tp)
            }
            KindMysqlSet => {
                let fval = self.get_mysql_set().to_number();
                convert_float_to_int(fval, lower_bound, upper_bound, tp)
            }
            KindMysqlJSON => convert_json_to_int(ctx, &self.get_mysql_json(), false, tp),
            KindBinaryLiteral | KindMysqlBit => {
                let (val, err) = self.get_binary_literal().to_int(ctx);
                if err.is_err() {
                    return (0, err);
                }
                convert_uint_to_int(val, upper_bound, tp)
            }
            _ => {
                let v = self.get_value().arg();
                (
                    0,
                    Err(errorf(
                        "cannot convert %v(type %T) to int64",
                        &[v.clone(), v],
                    )),
                )
            }
        }
    }

    /// ToFloat64 converts to a float64
    // go: Datum.ToFloat64 L1
    pub fn to_float64(&self, ctx: &Context) -> (f64, Result<(), Error>) {
        match self.kind() {
            KindInt64 => (self.get_int64() as f64, Ok(())),
            KindUint64 => (self.get_uint64() as f64, Ok(())),
            KindFloat32 => (f64::from(self.get_float32()), Ok(())),
            KindFloat64 => (self.get_float64(), Ok(())),
            KindString => str_to_float(ctx, self.get_string(), false),
            KindBytes => str_to_float(ctx, self.get_bytes(), false),
            KindMysqlTime => self.get_mysql_time().to_number().to_float64(),
            KindMysqlDuration => self.get_mysql_duration().to_number().to_float64(),
            KindMysqlDecimal => self.get_mysql_decimal().to_float64(),
            KindMysqlEnum => (self.get_mysql_enum().to_number(), Ok(())),
            KindMysqlSet => (self.get_mysql_set().to_number(), Ok(())),
            KindBinaryLiteral | KindMysqlBit => {
                let (val, err) = self.get_binary_literal().to_int(ctx);
                (val as f64, err)
            }
            KindMysqlJSON => convert_json_to_float(ctx, &self.get_mysql_json()),
            _ => {
                let v = self.get_value().arg();
                (
                    0.0,
                    Err(errorf(
                        "cannot convert %v(type %T) to float64",
                        &[v.clone(), v],
                    )),
                )
            }
        }
    }

    /// ToString gets the string representation of the datum.
    // go: Datum.ToString L1
    pub fn to_string(&self) -> Result<Vec<u8>, Error> {
        match self.kind() {
            KindInt64 => Ok(self.get_int64().to_string().into_bytes()),
            KindUint64 => Ok(self.get_uint64().to_string().into_bytes()),
            KindFloat32 => {
                Ok(format_float(f64::from(self.get_float32()), b'f', -1, 32).into_bytes())
            }
            KindFloat64 => Ok(format_float(self.get_float64(), b'f', -1, 64).into_bytes()),
            KindString => Ok(self.get_string().to_vec()),
            KindBytes => Ok(self.get_string().to_vec()),
            KindMysqlTime => Ok(self.get_mysql_time().string()),
            KindMysqlDuration => Ok(self.get_mysql_duration().string()),
            KindMysqlDecimal => Ok(self.get_mysql_decimal().string().into_bytes()),
            KindMysqlEnum => Ok(self.get_mysql_enum().string()),
            KindMysqlSet => Ok(self.get_mysql_set().string()),
            KindMysqlJSON => Ok(self.get_mysql_json().string()),
            KindBinaryLiteral | KindMysqlBit => Ok(self.get_binary_literal().to_string()),
            KindVectorFloat32 => Ok(self.get_vector_float32().string().into_bytes()),
            KindNull => Ok(Vec::new()),
            _ => {
                let v = self.get_value().arg();
                Err(errorf(
                    "cannot convert %v(type %T) to string",
                    &[v.clone(), v],
                ))
            }
        }
    }

    /// ToBytes gets the bytes representation of the datum.
    // go: Datum.ToBytes L1
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        match self.k {
            KindString | KindBytes => Ok(self.get_bytes().to_vec()),
            _ => self.to_string(),
        }
    }

    /// ToHashKey gets the bytes representation of the datum considering collation.
    // go: Datum.ToHashKey L1
    pub fn to_hash_key(&self) -> Result<Vec<u8>, Error> {
        match self.k {
            KindString | KindBytes => {
                Ok(collate::get_collator(self.collation()).key(self.get_string()))
            }
            _ => {
                let str = self.to_string()?;
                Ok(collate::get_collator(self.collation()).key(&str))
            }
        }
    }

    /// ToMysqlJSON is similar to convertToMysqlJSON, except the
    /// latter parses from string, but the former uses it as primitive.
    // go: Datum.ToMysqlJSON L1
    pub fn to_mysql_json(&self) -> Result<BinaryJSON<'_>, Error> {
        let in_ = match self.kind() {
            KindMysqlJSON => return Ok(self.get_mysql_json()),
            KindInt64 => JSONValue::Int64(self.get_int64()),
            KindUint64 => JSONValue::Uint64(self.get_uint64()),
            KindFloat32 | KindFloat64 => JSONValue::Float64(self.get_float64()),
            KindMysqlDecimal => {
                let (f, err) = self.get_mysql_decimal().to_float64();
                err?;
                JSONValue::Float64(f)
            }
            KindString | KindBytes => JSONValue::String(self.get_string().to_vec()),
            KindBinaryLiteral | KindMysqlBit => {
                JSONValue::String(self.get_binary_literal().to_string())
            }
            KindNull => JSONValue::Nil,
            KindMysqlTime => JSONValue::Time(self.get_mysql_time()),
            KindMysqlDuration => JSONValue::Duration(self.get_mysql_duration()),
            _ => JSONValue::String(self.to_string()?),
        };
        Ok(create_binary_json(in_))
    }

    /// MemUsage gets the memory usage of datum.
    // go: Datum.MemUsage L2 (cap(d.b) is the Vec capacity)
    pub fn mem_usage(&self) -> i64 {
        // d.x is not considered now since MemUsage is now only used by analyze samples which is bytesDatum
        EmptyDatumSize
            + self.b.as_ref().map_or(0, |b| b.capacity()) as i64
            + self.collation.len() as i64
    }

    /// MarshalJSON implements Marshaler.MarshalJSON interface.
    // go: Datum.MarshalJSON L2 (the jsonDatum struct written by hand in field order)
    pub fn marshal_json(&self) -> Result<Vec<u8>, Error> {
        let time = match self.k {
            KindMysqlTime => self.get_mysql_time(),
            _ => Time::default(),
        };
        let mydecimal = match self.k {
            KindMysqlDecimal => Some(self.get_mysql_decimal()),
            KindMysqlTime => None,
            _ => {
                if !matches!(self.x, DatumX::Nil) {
                    return Err(errorf(
                        "unsupported type: %d",
                        &[Arg::Uint(u64::from(self.k), "uint8")],
                    ));
                }
                None
            }
        };
        let mut out = String::new();
        out.push_str("{\"k\":");
        out.push_str(&self.k.to_string());
        if self.decimal != 0 {
            out.push_str(",\"decimal\":");
            out.push_str(&self.decimal.to_string());
        }
        if self.length != 0 {
            out.push_str(",\"length\":");
            out.push_str(&self.length.to_string());
        }
        if self.i != 0 {
            out.push_str(",\"i\":");
            out.push_str(&self.i.to_string());
        }
        if !self.collation.is_empty() {
            out.push_str(",\"collation\":");
            gojson::append_string(&mut out, &self.collation);
        }
        if let Some(b) = &self.b
            && !b.is_empty()
        {
            out.push_str(",\"b\":\"");
            out.push_str(&gojson::base64_std_encode(b));
            out.push('"');
        }
        // `Time` is a struct: omitempty does not drop it.
        out.push_str(",\"time\":");
        out.push_str(&String::from_utf8_lossy(&time.marshal_json()?));
        if let Some(dec) = mydecimal {
            out.push_str(",\"mydecimal\":");
            out.push_str(&String::from_utf8_lossy(&dec.marshal_json()?));
        }
        out.push('}');
        Ok(out.into_bytes())
    }

    /// UnmarshalJSON implements Unmarshaler.UnmarshalJSON interface.
    // go: Datum.UnmarshalJSON L2 (decodes jsonDatum with the encoding/json rules of gostd::json)
    pub fn unmarshal_json(&mut self, data: &[u8]) -> Result<(), Error> {
        const NAMES: &[&str] = &[
            "k",
            "decimal",
            "length",
            "i",
            "collation",
            "b",
            "time",
            "mydecimal",
        ];
        let raw = gojson::unmarshal_raw(data).map_err(tidb_base::errors::new)?;
        let mut k: u64 = 0;
        let mut decimal: u64 = 0;
        let mut length: u64 = 0;
        let mut i: i64 = 0;
        let mut collation = String::new();
        let mut b: Option<Vec<u8>> = None;
        let mut time = Time::default();
        let mut mydecimal: Option<MyDecimal> = None;
        let mut d = gojson::Decoder::with_source(data);
        d.object(
            &raw,
            "jsonDatum",
            "types.jsonDatum",
            NAMES,
            |d, idx, v| match idx {
                0 => d.uint(v, &mut k, u64::from(u8::MAX), "uint8"),
                1 => d.uint(v, &mut decimal, u64::from(u16::MAX), "uint16"),
                2 => d.uint(v, &mut length, u64::from(u32::MAX), "uint32"),
                3 => d.int(v, &mut i, i64::MIN, i64::MAX, "int64"),
                4 => d.string(v, &mut collation),
                5 => d.bytes(v, &mut b, "[]uint8", "uint8"),
                6 => {
                    // Time is a struct with an UnmarshalJSON method: called even for null.
                    let mut bytes = None;
                    d.raw_message(v, &mut bytes);
                    if let Err(err) = time.unmarshal_json(bytes.as_deref().unwrap_or(b"null")) {
                        d.fail(nested_json_error(&err));
                    }
                }
                7 => {
                    if matches!(v, gojson::Raw::Null) {
                        mydecimal = None;
                        return;
                    }
                    let mut bytes = None;
                    d.raw_message(v, &mut bytes);
                    let dec = mydecimal.get_or_insert_with(MyDecimal::default);
                    if let Err(err) = dec.unmarshal_json(bytes.as_deref().unwrap_or(b"null")) {
                        d.fail(nested_json_error(&err));
                    }
                }
                _ => {}
            },
        );
        d.finish()
            .map_err(|e| tidb_base::errors::new(e.to_string()))?;
        self.k = k as u8;
        self.decimal = decimal as u16;
        self.length = length as u32;
        self.i = i;
        self.collation = Cow::Owned(collation);
        self.set_b(b);

        match self.k {
            KindMysqlTime => self.set_mysql_time(time),
            KindMysqlDecimal => match mydecimal {
                Some(dec) => self.set_mysql_decimal(dec),
                // A nil *MyDecimal: GetMysqlDecimal would dereference nil in Go.
                None => {
                    self.k = KindMysqlDecimal;
                    self.x = DatumX::Nil;
                }
            },
            _ => {}
        }
        Ok(())
    }

    /// EstimatedMemUsage returns the estimated bytes consumed of a Datum.
    // go: Datum.EstimatedMemUsage L1
    pub fn estimated_mem_usage(&self) -> i64 {
        let mut bytes_consumed = sizeOfEmptyDatum;
        match self.kind() {
            KindMysqlDecimal => bytes_consumed += sizeOfMyDecimal,
            KindMysqlTime => bytes_consumed += sizeOfMysqlTime,
            KindVectorFloat32 => {
                bytes_consumed += self.get_vector_float32().estimated_mem_usage() as isize
            }
            _ => bytes_consumed += self.get_bytes().len() as isize,
        }
        bytes_consumed as i64
    }
}

/// The error of a nested `UnmarshalJSON` (Time, MyDecimal) as the decoding error Go's
/// `addErrorContext` rewrites: an `*UnmarshalTypeError` gets the enclosing struct and the
/// field path prepended; any other error is returned as is.
// go-std: encoding/json.decodeState.addErrorContext L2 (the type error parsed back from its message)
fn nested_json_error(err: &Error) -> gojson::DecodeError {
    let msg = String::from_utf8_lossy(&err.error()).into_owned();
    let parse = || -> Option<gojson::DecodeError> {
        let rest = msg.strip_prefix("json: cannot unmarshal ")?;
        let (value, rest) = rest.split_once(" into Go ")?;
        if let Some(t) = rest.strip_prefix("value of type ") {
            return Some(gojson::DecodeError::Type {
                value: value.to_owned(),
                ty: t.to_owned(),
                struct_name: String::new(),
                field: String::new(),
            });
        }
        let rest = rest.strip_prefix("struct field ")?;
        let (path, t) = rest.split_once(" of type ")?;
        let (_st, field) = path.split_once('.')?;
        Some(gojson::DecodeError::Type {
            value: value.to_owned(),
            ty: t.to_owned(),
            struct_name: String::new(),
            field: field.to_owned(),
        })
    };
    parse().unwrap_or(gojson::DecodeError::Other(msg))
}

/// `(T, error)` of a `Result<T, Error>` whose error result carries Go's zero T.
// glue: split a Result into Go's (value, error) pair
fn split<T: Default>(r: Result<T, Error>) -> (T, Result<(), Error>) {
    match r {
        Ok(v) => (v, Ok(())),
        Err(e) => (T::default(), Err(e)),
    }
}

/// `utf8.DecodeLastRuneInString`: the size of the last rune, 0 for an empty string and 1
/// (with an error flag) for an invalid encoding.
// go-std: unicode/utf8.DecodeLastRuneInString L2 (only the size and the RuneError test)
fn decode_last_rune(s: &[u8]) -> (bool, usize) {
    let end = s.len();
    if end == 0 {
        return (true, 0);
    }
    let mut start = end - 1;
    let lim = end.saturating_sub(4);
    while start > lim && (s[start] & 0xC0) == 0x80 {
        start -= 1;
    }
    let (r, size) = tidb_base::gostd::decode_rune(&s[start..end]);
    if start + size != end {
        return (true, 1);
    }
    (r == char::REPLACEMENT_CHARACTER && size == 1, size)
}

/// ProduceFloatWithSpecifiedTp produces a new float64 according to `flen` and `decimal`.
// go: ProduceFloatWithSpecifiedTp L1
pub fn produce_float_with_specified_tp(f: f64, target: &FieldType) -> (f64, Result<(), Error>) {
    if f.is_nan() {
        return (0.0, Err(overflow(Arg::Float(f, 64), target.get_type())));
    }
    if f.is_infinite() {
        return (f, Err(overflow(Arg::Float(f, 64), target.get_type())));
    }
    let mut f = f;
    let mut err: Result<(), Error> = Ok(());
    // For float and following double type, we will only truncate it for float(M, D) format.
    // If no D is set, we will handle it like origin float whether M is set or not.
    if target.get_flen() != UnspecifiedLength && target.get_decimal() != UnspecifiedLength {
        (f, err) = truncate_float(f, target.get_flen(), target.get_decimal());
    }
    if mysql::has_unsigned_flag(target.get_flag()) && f < 0.0 {
        return (0.0, Err(overflow(Arg::Float(f, 64), target.get_type())));
    }

    if err.is_err() {
        // We must return the error got from TruncateFloat after checking whether the target is unsigned to make sure
        // the returned float is not negative when the target type is unsigned.
        return (f, err);
    }

    if target.get_type() == mysql::TypeFloat
        && (f > f64::from(f32::MAX) || f < -f64::from(f32::MAX))
    {
        if f > 0.0 {
            return (
                f64::from(f32::MAX),
                Err(overflow(Arg::Float(f, 64), target.get_type())),
            );
        }
        return (
            -f64::from(f32::MAX),
            Err(overflow(Arg::Float(f, 64), target.get_type())),
        );
    }
    (f, err)
}

/// ProduceStrWithSpecifiedTp produces a new string according to `flen` and `chs`. Param `padZero` indicates
/// whether we should pad `\0` for `binary(flen)` type.
// go: ProduceStrWithSpecifiedTp L1
pub fn produce_str_with_specified_tp(
    s: Vec<u8>,
    tp: &FieldType,
    ctx: &Context,
    pad_zero: bool,
) -> (Vec<u8>, Result<(), Error>) {
    let mut s = s;
    let mut err: Result<(), Error> = Ok(());
    let flen = tp.get_flen();
    let chs = tp.get_charset();
    if flen >= 0 {
        let flen_u = flen as usize;
        // overflowed stores the part of the string that is out of the length constraint, it is later checked to see if the
        // overflowed part is all whitespaces
        let mut overflowed: Vec<u8> = Vec::new();
        let mut character_len: usize = 0;
        let mut need_calculate_len = false;

        // For  mysql.TypeTinyBlob, mysql.TypeMediumBlob, mysql.TypeLongBlob, mysql.TypeBlob(defined in tidb)
        // and tinytext, text, mediumtext, longtext(not explicitly defined in tidb, corresponding to blob(s) in tidb) flen is the store length limit regardless of charset.
        if chs != CharsetBin {
            match tp.get_type() {
                mysql::TypeTinyBlob
                | mysql::TypeMediumBlob
                | mysql::TypeLongBlob
                | mysql::TypeBlob => {
                    character_len = s.len();
                    // We need to truncate the value to a proper length that contains complete word.
                    if character_len > flen_u {
                        // Find the truncate position.
                        let mut truncate_len = flen;
                        while truncate_len > 0 {
                            let temp_str = truncate_str(&s, truncate_len);
                            let (bad, size) = decode_last_rune(temp_str);
                            if bad && (size == 0 || size == 1) {
                                // Empty string or invalid string
                                truncate_len -= 1;
                                continue;
                            }
                            // Get the truncate position
                            break;
                        }
                        overflowed = s[truncate_len as usize..].to_vec();
                        s = truncate_str(&s, truncate_len).to_vec();
                    }
                }
                _ => {
                    if s.len() > flen_u {
                        character_len = tidb_base::gostd::rune_count(&s);
                        if character_len > flen_u {
                            // 1. If len(s) is 0 and flen is 0, truncateLen will be 0, don't truncate s.
                            //    CREATE TABLE t (a char(0));
                            //    INSERT INTO t VALUES (``);
                            // 2. If len(s) is 10 and flen is 0, truncateLen will be 0 too, but we still need to truncate s.
                            //    SELECT 1, CAST(1234 AS CHAR(0));
                            // So truncateLen is not a suitable variable to determine to do truncate or not.
                            let mut rune_count = 0usize;
                            let mut truncate_len = 0usize;
                            let mut i = 0usize;
                            while i < s.len() {
                                if rune_count == flen_u {
                                    truncate_len = i;
                                    break;
                                }
                                rune_count += 1;
                                let (_, size) = tidb_base::gostd::decode_rune(&s[i..]);
                                i += size;
                            }
                            overflowed = s[truncate_len..].to_vec();
                            s = truncate_str(&s, truncate_len as isize).to_vec();
                        } else {
                            need_calculate_len = true;
                        }
                    }
                }
            }
        } else if s.len() > flen_u {
            character_len = s.len();
            overflowed = s[flen_u..].to_vec();
            s = truncate_str(&s, flen).to_vec();
        }

        if !overflowed.is_empty() {
            let trimmed_len = overflowed
                .iter()
                .rposition(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
                .map_or(0, |p| p + 1);
            if trimmed_len == 0 && !is_binary_str(tp) && is_type_char(tp.get_type()) {
                if tp.get_type() == mysql::TypeVarchar {
                    if need_calculate_len {
                        character_len = tidb_base::gostd::rune_count(&s);
                    }
                    ctx.append_warning(ErrTruncated.fast_gen(
                        "Data truncated, field len %d, data len %d",
                        vec![
                            Arg::Int(flen as i64, "int"),
                            Arg::Int(character_len as i64, "int"),
                        ],
                    ));
                }
            } else {
                if need_calculate_len {
                    character_len = tidb_base::gostd::rune_count(&s);
                }
                err = Err(ErrDataTooLong.fast_gen(
                    "Data Too Long, field len %d, data len %d",
                    vec![
                        Arg::Int(flen as i64, "int"),
                        Arg::Int(character_len as i64, "int"),
                    ],
                ));
            }
        }

        if tp.get_type() == mysql::TypeString && is_binary_str(tp) && s.len() < flen_u && pad_zero {
            s.resize(flen_u, 0);
        }
    }
    let err = ctx.handle_truncate(err);
    (s, err)
}

/// ProduceDecWithSpecifiedTp produces a new decimal according to `flen` and `decimal`.
///
/// Go returns a nil `*MyDecimal` with `ErrMBiggerThanD`: `None` here.
// go: ProduceDecWithSpecifiedTp L1
pub fn produce_dec_with_specified_tp(
    ctx: &Context,
    dec: MyDecimal,
    tp: &FieldType,
) -> (Option<MyDecimal>, Result<(), Error>) {
    let mut dec = dec;
    let mut err: Result<(), Error> = Ok(());
    let flen = tp.get_flen();
    let decimal = tp.get_decimal();
    if flen != UnspecifiedLength && decimal != UnspecifiedLength {
        if flen < decimal {
            return (
                None,
                Err(ErrMBiggerThanD.gen_with_stack_by_args(vec![Arg::from("")])),
            );
        }

        let mut old: Option<MyDecimal> = None;
        if dec.get_digits_frac() as isize > decimal {
            old = Some(dec);
        }
        if dec.get_digits_frac() as isize != decimal {
            // Error doesn't matter because the following code will check the new decimal
            // and set error if any.
            let _ = dec.round_in_place(decimal, MODE_HALF_UP);
        }

        let (_, digits_int) = dec.remove_leading_zeros();
        // After rounding decimal, the new decimal may have a longer integer length which may be longer than expected.
        // So the check of integer length must be after rounding.
        // E.g. "99.9999", flen 5, decimal 3, Round("99.9999", 3, ModelHalfUp) -> "100.000".
        if flen - decimal < digits_int {
            // Integer length is longer, choose the max or min decimal.
            dec = new_max_or_min_dec(dec.is_negative(), flen, decimal);
            // select cast(111 as decimal(1)) causes a warning in MySQL.
            err = Err(ErrOverflow.gen_with_stack_by_args(vec![
                Arg::from("DECIMAL"),
                Arg::from(format!("({flen}, {decimal})")),
            ]));
        } else if let Some(old) = old
            && dec.compare(&old) != 0
        {
            ctx.append_warning(ErrTruncatedWrongVal.fast_gen_by_args(vec![
                Arg::from("DECIMAL"),
                Arg::Stringer(old.string().into_bytes(), "*types.MyDecimal"),
            ]));
        }
    }

    let unsigned = mysql::has_unsigned_flag(tp.get_flag());
    if unsigned && dec.is_negative() {
        dec.from_uint(0);
    }
    (Some(dec), err)
}

/// ConvertDatumToDecimal converts datum to decimal.
///
/// Go returns a nil `*MyDecimal` with an error from `ConvertJSONToDecimal`: `None` here.
// go: ConvertDatumToDecimal L1
pub fn convert_datum_to_decimal(
    ctx: &Context,
    d: &Datum,
) -> (Option<MyDecimal>, Result<(), Error>) {
    let mut dec = MyDecimal::default();
    let mut err: Result<(), Error> = Ok(());
    match d.kind() {
        KindInt64 => {
            dec.from_int(d.get_int64());
        }
        KindUint64 => {
            dec.from_uint(d.get_uint64());
        }
        KindFloat32 => err = dec.from_float64(f64::from(d.get_float32())),
        KindFloat64 => err = dec.from_float64(d.get_float64()),
        KindString => err = ctx.handle_truncate(dec.from_string(d.get_bytes())),
        KindMysqlDecimal => dec = *d.get_mysql_decimal(),
        KindMysqlEnum => {
            dec.from_uint(d.get_mysql_enum().value);
        }
        KindMysqlSet => {
            dec.from_uint(d.get_mysql_set().value);
        }
        KindBinaryLiteral | KindMysqlBit => {
            let (val, err1) = d.get_binary_literal().to_int(ctx);
            dec.from_uint(val);
            err = err1;
        }
        KindMysqlJSON => {
            let (f, err1) = convert_json_to_decimal(ctx, &d.get_mysql_json());
            if err1.is_err() {
                return (None, err1);
            }
            dec = f;
        }
        _ => {
            err = Err(errorf(
                "can't convert %v to decimal",
                &[d.get_value().arg()],
            ));
        }
    }
    (Some(dec), err)
}

// go: invalidConv L1
fn invalid_conv(d: &Datum, tp: u8) -> (Datum, Result<(), Error>) {
    (
        Datum::default(),
        Err(errorf(
            "cannot convert datum from %s to type %s",
            &[Arg::from(kind_str(d.kind())), Arg::from(type_str(tp))],
        )),
    )
}

/// NewDatum creates a new Datum from an interface{}.
// go: NewDatum L1
pub fn new_datum(in_: GoValue) -> Datum {
    let mut d = Datum::default();
    match in_ {
        GoValue::Slice(x) => d.set_value_with_default_collation(GoValue::Datums(make_datums(x))),
        other => d.set_value_with_default_collation(other),
    }
    d
}

/// NewIntDatum creates a new Datum from an int64 value.
// go: NewIntDatum L1
pub fn new_int_datum(i: i64) -> Datum {
    let mut d = Datum::default();
    d.set_int64(i);
    d
}

/// NewUintDatum creates a new Datum from an uint64 value.
// go: NewUintDatum L1
pub fn new_uint_datum(i: u64) -> Datum {
    let mut d = Datum::default();
    d.set_uint64(i);
    d
}

/// NewBytesDatum creates a new Datum from a byte slice.
// go: NewBytesDatum L1
pub fn new_bytes_datum(b: impl Into<Option<Vec<u8>>>) -> Datum {
    let mut d = Datum::default();
    d.set_bytes(b);
    d
}

/// NewStringDatum creates a new Datum from a string.
// go: NewStringDatum L1
pub fn new_string_datum(s: impl Into<Vec<u8>>) -> Datum {
    let mut d = Datum::default();
    d.set_string(s, mysql::DefaultCollationName);
    d
}

/// NewCollationStringDatum creates a new Datum from a string with collation.
// go: NewCollationStringDatum L1
pub fn new_collation_string_datum(
    s: impl Into<Vec<u8>>,
    collation: impl Into<Cow<'static, str>>,
) -> Datum {
    let mut d = Datum::default();
    d.set_string(s, collation);
    d
}

/// NewFloat64Datum creates a new Datum from a float64 value.
// go: NewFloat64Datum L1
pub fn new_float64_datum(f: f64) -> Datum {
    let mut d = Datum::default();
    d.set_float64(f);
    d
}

/// NewFloat32Datum creates a new Datum from a float32 value.
// go: NewFloat32Datum L1
pub fn new_float32_datum(f: f32) -> Datum {
    let mut d = Datum::default();
    d.set_float32(f);
    d
}

/// NewDurationDatum creates a new Datum from a Duration value.
// go: NewDurationDatum L1
pub fn new_duration_datum(dur: Duration) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_duration(dur);
    d
}

/// NewTimeDatum creates a new Time from a Time value.
// go: NewTimeDatum L1
pub fn new_time_datum(t: Time) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_time(t);
    d
}

/// NewDecimalDatum creates a new Datum from a MyDecimal value.
// go: NewDecimalDatum L1
pub fn new_decimal_datum(dec: MyDecimal) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_decimal(dec);
    d
}

/// NewJSONDatum creates a new Datum from a BinaryJSON value
// go: NewJSONDatum L1
pub fn new_json_datum(j: BinaryJSON<'_>) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_json(j);
    d
}

/// NewVectorFloat32Datum creates a new Datum from a VectorFloat32 value
// go: NewVectorFloat32Datum L1
pub fn new_vector_float32_datum(v: VectorFloat32<'_>) -> Datum {
    let mut d = Datum::default();
    d.set_vector_float32(v);
    d
}

/// NewBinaryLiteralDatum creates a new BinaryLiteral Datum for a BinaryLiteral value.
// go: NewBinaryLiteralDatum L1
pub fn new_binary_literal_datum(b: BinaryLiteral) -> Datum {
    let mut d = Datum::default();
    d.set_binary_literal(b);
    d
}

/// NewMysqlBitDatum creates a new MysqlBit Datum for a BinaryLiteral value.
// go: NewMysqlBitDatum L1
pub fn new_mysql_bit_datum(b: BinaryLiteral) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_bit(b);
    d
}

/// NewMysqlEnumDatum creates a new MysqlEnum Datum for a Enum value.
// go: NewMysqlEnumDatum L1
pub fn new_mysql_enum_datum(e: Enum) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_enum(e, mysql::DefaultCollationName);
    d
}

/// NewCollateMysqlEnumDatum create a new MysqlEnum Datum for a Enum value with collation information.
// go: NewCollateMysqlEnumDatum L1
pub fn new_collate_mysql_enum_datum(e: Enum, collation: impl Into<Cow<'static, str>>) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_enum(e, collation);
    d
}

/// NewMysqlSetDatum creates a new MysqlSet Datum for a Enum value.
// go: NewMysqlSetDatum L1
pub fn new_mysql_set_datum(e: Set, collation: impl Into<Cow<'static, str>>) -> Datum {
    let mut d = Datum::default();
    d.set_mysql_set(e, collation);
    d
}

/// MakeDatums creates datum slice from interfaces.
// go: MakeDatums L1
pub fn make_datums(args: Vec<GoValue>) -> Vec<Datum> {
    args.into_iter().map(new_datum).collect()
}

/// MinNotNullDatum returns a datum represents minimum not null value.
// go: MinNotNullDatum L1
pub fn min_not_null_datum() -> Datum {
    Datum {
        k: KindMinNotNull,
        ..Datum::default()
    }
}

/// MaxValueDatum returns a datum represents max value.
// go: MaxValueDatum L1
pub fn max_value_datum() -> Datum {
    Datum {
        k: KindMaxValue,
        ..Datum::default()
    }
}

/// SortDatums sorts a slice of datum.
///
/// Go's `sort.Sort` (pattern-defeating quicksort, not stable); `Less` records the first
/// comparison error and reports `true`. The same algorithm is used so that the order of
/// equal elements and the comparisons made (and so the error seen) match Go.
// go: SortDatums L2 (sort.Sort via gostd::sort::sort with datumsSorter's Less)
// go: datumsSorter.Len L2 (the slice length gostd::sort::sort reads)
// go: datumsSorter.Swap L2 (the slice swap inside gostd::sort::sort)
pub fn sort_datums(ctx: &Context, datums: &mut [Datum]) -> Result<(), Error> {
    let mut err: Option<Error> = None;
    tidb_base::gostd::sort::sort(datums, |a: &Datum, b: &Datum| {
        datums_sorter_less(ctx, a, b, &mut err)
    });
    match err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

// go: datumsSorter.Less L2 (the sorter's err field is the closure's err)
fn datums_sorter_less(ctx: &Context, a: &Datum, b: &Datum, err_out: &mut Option<Error>) -> bool {
    let (cmp, err) = a.compare(ctx, b, collate::get_collator(a.collation()));
    if let Err(e) = err {
        *err_out = Some(e);
        return true;
    }
    cmp < 0
}

/// Check if a string is considered printable
///
/// Checks
/// 1. Must be valid UTF-8
/// 2. Must not contain control characters like NUL (0x0) and backspace (0x8)
// go: isPrintable L1
fn is_printable(s: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(s) else {
        return false;
    };
    for r in s.chars() {
        // unicode.IsControl: C0, DEL and C1 (Latin-1 only).
        let c = r as u32;
        if c < 0x20 || (0x7f..=0x9f).contains(&c) {
            return false;
        }
    }
    true
}

/// DatumsToString converts several datums to formatted string.
// go: DatumsToString L1
pub fn datums_to_string(datums: &[Datum], handle_special_value: bool) -> Result<Vec<u8>, Error> {
    datums_to_string_impl(datums, handle_special_value, false)
}

/// DatumsToStringSmart is like DatumsToString, but with smart detection of non-printable data
// go: DatumsToStringSmart L1
pub fn datums_to_string_smart(
    datums: &[Datum],
    handle_special_value: bool,
) -> Result<Vec<u8>, Error> {
    datums_to_string_impl(datums, handle_special_value, true)
}

// go: datumsToString L2 (no strings.Builder pool)
fn datums_to_string_impl(
    datums: &[Datum],
    handle_special_value: bool,
    binary_as_hex: bool,
) -> Result<Vec<u8>, Error> {
    let n = datums.len();
    let mut builder: Vec<u8> = Vec::new();
    if n > 1 {
        builder.extend_from_slice(b"(");
    }
    for (i, datum) in datums.iter().enumerate() {
        if i > 0 {
            builder.extend_from_slice(b", ");
        }
        if handle_special_value {
            match datum.kind() {
                KindNull => {
                    builder.extend_from_slice(b"NULL");
                    continue;
                }
                KindMinNotNull => {
                    builder.extend_from_slice(b"-inf");
                    continue;
                }
                KindMaxValue => {
                    builder.extend_from_slice(b"+inf");
                    continue;
                }
                _ => {}
            }
        }
        let mut str = datum.to_string()?;
        const LOG_DATUM_LEN: usize = 2048;
        let mut original_len: isize = -1;
        if str.len() > LOG_DATUM_LEN {
            original_len = str.len() as isize;
            str.truncate(LOG_DATUM_LEN);
        }
        if datum.kind() == KindString {
            if !binary_as_hex || is_printable(&str) {
                builder.push(b'"');
                builder.extend_from_slice(&str);
                builder.push(b'"');
            } else {
                // Print as hex-literal instead
                builder.extend(sprintf("0x%X", &[Arg::go_string(str)]));
            }
        } else {
            builder.extend_from_slice(&str);
        }
        if original_len != -1 {
            builder.extend_from_slice(b" len(");
            builder.extend_from_slice(original_len.to_string().as_bytes());
            builder.extend_from_slice(b")");
        }
    }
    if n > 1 {
        builder.extend_from_slice(b")");
    }
    Ok(builder)
}

/// DatumsToStrNoErr converts some datums to a formatted string.
/// If an error occurs, it will print a log instead of returning an error.
// go: DatumsToStrNoErr L1
pub fn datums_to_str_no_err(datums: &[Datum]) -> Vec<u8> {
    match datums_to_string(datums, true) {
        Ok(s) => s,
        Err(err) => {
            crate::errors::terror_log(Err(err));
            Vec::new()
        }
    }
}

/// DatumsToStrNoErrSmart converts some datums to a formatted string.
/// If an error occurs, it will print a log instead of returning an error.
/// It also enables detection of non-pritable arguments
// go: DatumsToStrNoErrSmart L1
pub fn datums_to_str_no_err_smart(datums: &[Datum]) -> Vec<u8> {
    match datums_to_string_smart(datums, true) {
        Ok(s) => s,
        Err(err) => {
            crate::errors::terror_log(Err(err));
            Vec::new()
        }
    }
}

/// CloneRow deep copies a Datum slice.
// go: CloneRow L1
pub fn clone_row(dr: &[Datum]) -> Vec<Datum> {
    dr.to_vec()
}

/// GetMaxValue returns the max value datum for each type.
// go: GetMaxValue L1
pub fn get_max_value(ft: &FieldType) -> Datum {
    let mut max_val = Datum::default();
    match ft.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong => {
            if mysql::has_unsigned_flag(ft.get_flag()) {
                max_val.set_uint64(integer_unsigned_upper_bound(ft.get_type()));
            } else {
                max_val.set_int64(integer_signed_upper_bound(ft.get_type()));
            }
        }
        mysql::TypeFloat => {
            max_val.set_float32(get_max_float(ft.get_flen(), ft.get_decimal()) as f32);
        }
        mysql::TypeDouble => max_val.set_float64(get_max_float(ft.get_flen(), ft.get_decimal())),
        mysql::TypeString
        | mysql::TypeVarString
        | mysql::TypeVarchar
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob => {
            // codec.Encode KindMaxValue, to avoid import circle
            max_val.set_string(vec![250u8], ft.get_collate().to_owned());
        }
        mysql::TypeNewDecimal => {
            max_val.set_mysql_decimal(new_max_or_min_dec(false, ft.get_flen(), ft.get_decimal()));
        }
        mysql::TypeDuration => max_val.set_mysql_duration(Duration {
            duration: MaxTime,
            fsp: 0,
        }),
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
            if ft.get_type() == mysql::TypeDate || ft.get_type() == mysql::TypeDatetime {
                max_val.set_mysql_time(new_time(MaxDatetime, ft.get_type(), 0));
            } else {
                max_val.set_mysql_time(*MaxTimestamp);
            }
        }
        _ => {}
    }
    max_val
}

/// GetMinValue returns the min value datum for each type.
// go: GetMinValue L1
pub fn get_min_value(ft: &FieldType) -> Datum {
    let mut min_val = Datum::default();
    match ft.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong => {
            if mysql::has_unsigned_flag(ft.get_flag()) {
                min_val.set_uint64(0);
            } else {
                min_val.set_int64(integer_signed_lower_bound(ft.get_type()));
            }
        }
        mysql::TypeFloat => {
            min_val.set_float32(-get_max_float(ft.get_flen(), ft.get_decimal()) as f32);
        }
        mysql::TypeDouble => min_val.set_float64(-get_max_float(ft.get_flen(), ft.get_decimal())),
        mysql::TypeString
        | mysql::TypeVarString
        | mysql::TypeVarchar
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob => {
            // codec.Encode KindMinNotNull, to avoid import circle
            min_val.set_string(vec![1u8], ft.get_collate().to_owned());
        }
        mysql::TypeNewDecimal => {
            min_val.set_mysql_decimal(new_max_or_min_dec(true, ft.get_flen(), ft.get_decimal()));
        }
        mysql::TypeDuration => min_val.set_mysql_duration(Duration {
            duration: MinTime,
            fsp: 0,
        }),
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
            if ft.get_type() == mysql::TypeDate || ft.get_type() == mysql::TypeDatetime {
                min_val.set_mysql_time(new_time(MinDatetime, ft.get_type(), 0));
            } else {
                min_val.set_mysql_time(*MinTimestamp);
            }
        }
        _ => {}
    }
    min_val
}

/// Go `unsafe.Sizeof(Datum{})`.
const sizeOfEmptyDatum: isize = EmptyDatumSize as isize;
/// Go `unsafe.Sizeof(ZeroTime)` (`Time` is one `CoreTime` uint64).
const sizeOfMysqlTime: isize = 8;
const sizeOfMyDecimal: isize = MY_DECIMAL_STRUCT_SIZE as isize;

/// EstimatedMemUsage returns the estimated bytes consumed of a one-dimensional
/// or two-dimensional datum array.
// go: EstimatedMemUsage L1
pub fn estimated_mem_usage(array: &[Datum], num_of_rows: isize) -> i64 {
    if num_of_rows == 0 {
        return 0;
    }
    let mut bytes_consumed: i64 = 0;
    for d in array {
        bytes_consumed += d.estimated_mem_usage();
    }
    bytes_consumed * num_of_rows as i64
}

/// DatumsContainNull return true if any value is null
// go: DatumsContainNull L1
pub fn datums_contain_null(vals: &[Datum]) -> bool {
    vals.iter().any(Datum::is_null)
}

/// `types.NewFieldType` (field_type.go), used by the conversions of this file.
// glue: NewFieldType of field_type.go
fn new_field_type(tp: u8) -> FieldType {
    crate::field_type::new_field_type(tp)
}

// go: findEncoding L1
fn find_encoding(flags: Flags, chs: &str) -> (Encoding, bool) {
    let mut enc = genc::find_encoding(chs);
    if enc.tp() == genc::EncodingTpUTF8 && flags.skip_utf8_check()
        || enc.tp() == genc::EncodingTpASCII && flags.skip_ascii_check()
    {
        return (enc, true);
    }
    if chs == CharsetUTF8 && !flags.skip_utf8mb4_check() {
        enc = genc::EncodingUTF8MB3StrictImpl;
    }
    (enc, false)
}

// go: truncateStringIfNeeded L1
fn truncate_string_if_needed(str: &[u8]) -> Vec<u8> {
    const MAX_LEN: usize = 64;
    if str.len() > MAX_LEN {
        const SUFFIX: &[u8] = b"...(len:";
        let len_str = str.len().to_string();
        let mut buf = Vec::with_capacity(MAX_LEN + SUFFIX.len() + len_str.len() + 1);
        buf.extend_from_slice(&str[..MAX_LEN]);
        buf.extend_from_slice(SUFFIX);
        buf.extend_from_slice(len_str.as_bytes());
        buf.push(b')');
        return buf;
    }
    str.to_vec()
}

/// Hash64ForDatum is a hash function for initialized by codec package.
pub static Hash64ForDatum: OnceLock<fn(&mut dyn IHasher, &Datum)> = OnceLock::new();
