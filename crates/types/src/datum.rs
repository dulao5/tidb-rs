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
use tidb_parser::gocharset::{self, CharsetBin, CharsetUTF8, CollationBin, Encoding};
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
use crate::mydecimal::{
    MY_DECIMAL_STRUCT_SIZE, MODE_HALF_UP, MyDecimal, decimal_add, new_max_or_min_dec,
};
use crate::set_type::{Set, parse_set, parse_set_value};
use crate::time::{
    Duration, MaxDatetime, MaxDuration, MaxTime, MaxTimestamp, MinDatetime, MinTime,
    MinTimestamp, Time, ZeroDate, adjust_year_checked, from_date, new_time, parse_datetime_str,
    parse_duration, parse_time, parse_time_from_float_string, parse_time_from_num,
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
    /// Any other Go value stored with `SetInterface`.
    Interface(Arc<dyn Any + Send + Sync>),
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
#[derive(Clone, Debug, Default)]
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
            GoValue::BitLiteral(b) => Arg::Stringer(b.to_string(), "types.BitLiteral"),
            GoValue::HexLiteral(b) => Arg::Stringer(b.to_string(), "types.HexLiteral"),
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
        let enc = gocharset::find_encoding_take_utf8_as_noop(&coll.charset_name);
        let (replace, _) = enc.transform(self.get_bytes(), gocharset::OpEncodeNoErr);
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
        let (trim, err) = enc.transform(self.get_bytes(), gocharset::OpDecode);
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
            let (replace, err) = enc.transform(str, gocharset::OpReplace);
            return (replace.into_owned(), err);
        }
        (self.get_string().to_vec(), Ok(()))
    }

    /// SetString sets string value.
    // go: Datum.SetString L1
    pub fn set_string(&mut self, s: impl Into<Vec<u8>>, collation: impl Into<Cow<'static, str>>) {
        self.k = KindString;
        self.b = Some(s.into());
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
        self.b = b.into();
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
        self.b = b.into();
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
                } else {
                    GoValue::Other(v.clone())
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
            GoValue::Datums(ds) => DatumX::Interface(Arc::new(ds)),
            GoValue::Other(v) => DatumX::Interface(v),
            other => DatumX::Interface(Arc::new(other)),
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
        self.b = Some(b.0);
        self.collation = Cow::Borrowed(CollationBin);
    }

    /// SetMysqlBit sets MysqlBit value
    // go: Datum.SetMysqlBit L1
    pub fn set_mysql_bit(&mut self, b: BinaryLiteral) {
        self.k = KindMysqlBit;
        self.b = Some(b.0);
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
            duration: tidb_base::gostd::time::Duration(self.i),
            fsp: (self.decimal as i8) as isize,
        }
    }

    /// SetMysqlDuration sets Duration value
    // go: Datum.SetMysqlDuration L1
    #[inline]
    pub fn set_mysql_duration(&mut self, b: Duration) {
        self.k = KindMysqlDuration;
        self.i = b.duration.0;
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
        self.b = Some(b.name);
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
        self.b = Some(b.name);
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
        self.b = Some(b.value.into_owned());
    }

    /// SetVectorFloat32 sets VectorFloat32 value
    // go: Datum.SetVectorFloat32 L1
    pub fn set_vector_float32(&mut self, vec: VectorFloat32<'_>) {
        self.k = KindVectorFloat32;
        self.b = Some(vec.zero_copy_serialize().to_vec());
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
        self.b = b.into();
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
                let f_val = self.get_mysql_duration().duration.seconds();
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
                let (d_val, err) = self.convert_to(ctx, &new_field_type(mysql::TypeNewDecimal));
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
            _ => self.compare_float64(ctx, dur.duration.seconds()),
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

/// `types.NewFieldType` (field_type.go), used by the conversions of this file.
// glue: NewFieldType of field_type.go
fn new_field_type(tp: u8) -> FieldType {
    crate::field_type::new_field_type(tp)
}

// go: findEncoding L1
fn find_encoding(flags: Flags, chs: &str) -> (Encoding, bool) {
    let mut enc = gocharset::find_encoding(chs);
    if enc.tp() == gocharset::EncodingTpUTF8 && flags.skip_utf8_check()
        || enc.tp() == gocharset::EncodingTpASCII && flags.skip_ascii_check()
    {
        return (enc, true);
    }
    if chs == CharsetUTF8 && !flags.skip_utf8mb4_check() {
        enc = gocharset::EncodingUTF8MB3StrictImpl;
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
