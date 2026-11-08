//! go-file: pkg/types/field_type.go
//!
//! FieldType helpers of `pkg/types` (`FieldType` itself is the parser's type, D02.3).
//!
//! Mapping: Go's `*FieldType` results are owned `FieldType` values; `flag *uint` is
//! `&mut usize`; the `any` arguments are [`GoValue`].

#![allow(non_upper_case_globals)]

use tidb_base::dbterror::ErrUnsupportedModifyColumn;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::strconv::format_float;
use tidb_base::mysql;
use tidb_parser::gocharset::{self, CharsetBin, CharsetUTF8MB4, CollationBin, CollationUTF8MB4};
use tidb_parser::types::{ETDecimal, ETInt, ETReal, ETString, EvalType};
pub use tidb_parser::types::{UnspecifiedLength, VarStorageLen};

use crate::collate;
use crate::datum::{Datum, GoValue};
use crate::errors::{ErrTooBigFieldLength, Error};
use crate::etc::{
    FieldType, is_binary_str, is_string, is_string_kind, is_type_blob, is_type_char,
    is_type_integer, is_type_time, is_type_varchar,
};
use crate::field_type_builder::new_field_type_builder;
use crate::fsp::DefaultFsp;

/// ErrorLength is error length for blob or text.
pub const ErrorLength: isize = 0;

/// NewFieldType returns a FieldType,
/// with a type and other information about field type.
// go: NewFieldType L1
pub fn new_field_type(tp: u8) -> FieldType {
    let (charset1, collate1) = default_charset_for_type(tp);
    let (flen, decimal) = min_flen_and_decimal_for_type(tp);
    new_field_type_builder()
        .set_type(tp)
        .set_charset(charset1)
        .set_collate(collate1)
        .set_flen(flen)
        .set_decimal(decimal)
        .build_p()
}

/// NewFieldTypeWithCollation returns a FieldType,
/// with a type and other information about field type.
// go: NewFieldTypeWithCollation L2 (an unknown collation gives the empty charset; Go dereferences a nil *Collation)
pub fn new_field_type_with_collation(tp: u8, collation: &str, length: isize) -> FieldType {
    let charset_name = match gocharset::get_collation_by_name(collation) {
        Ok(coll) => coll.charset_name.clone(),
        Err(_) => panic!("invalid memory address or nil pointer dereference"),
    };
    new_field_type_builder()
        .set_type(tp)
        .set_flen(length)
        .set_charset(&charset_name)
        .set_collate(collation)
        .set_decimal(UnspecifiedLength)
        .build_p()
}

/// AggFieldType aggregates field types for a multi-argument function like `IF`, `IFNULL`, `COALESCE`
/// whose return type is determined by the arguments' FieldTypes.
/// Aggregation is performed by MergeFieldType function.
// go: AggFieldType L1
pub fn agg_field_type(tps: &[&FieldType]) -> FieldType {
    let mut curr_type = FieldType::default();
    let mut is_mixed_sign = false;
    for (i, t) in tps.iter().enumerate() {
        if i == 0 && curr_type.get_type() == mysql::TypeUnspecified {
            curr_type = t.deep_copy();
            continue;
        }
        let mtp = merge_field_type(curr_type.get_type(), t.get_type());
        is_mixed_sign = is_mixed_sign
            || (mysql::has_unsigned_flag(curr_type.get_flag())
                != mysql::has_unsigned_flag(t.get_flag()));
        curr_type.set_type(mtp);
        curr_type.set_flag(merge_type_flag(curr_type.get_flag(), t.get_flag()));
    }
    // integral promotion when tps contains signed and unsigned
    if is_mixed_sign && is_type_integer(curr_type.get_type()) {
        let mut bump_range = false; // indicate one of tps bump currType range
        for t in tps {
            bump_range = bump_range
                || (mysql::has_unsigned_flag(t.get_flag())
                    && (t.get_type() == curr_type.get_type() || t.get_type() == mysql::TypeBit));
        }
        if bump_range {
            match curr_type.get_type() {
                mysql::TypeTiny => curr_type.set_type(mysql::TypeShort),
                mysql::TypeShort => curr_type.set_type(mysql::TypeInt24),
                mysql::TypeInt24 => curr_type.set_type(mysql::TypeLong),
                mysql::TypeLong => curr_type.set_type(mysql::TypeLonglong),
                mysql::TypeLonglong => curr_type.set_type(mysql::TypeNewDecimal),
                _ => {}
            }
        }
    }

    if mysql::has_unsigned_flag(curr_type.get_flag()) && !is_mixed_sign {
        curr_type.add_flag(mysql::UnsignedFlag);
    }

    curr_type
}

/// TryToFixFlenOfDatetime try to fix flen of Datetime for specific func or other field merge cases
// go: TryToFixFlenOfDatetime L1
pub fn try_to_fix_flen_of_datetime(result_tp: &mut FieldType) {
    if result_tp.get_type() == mysql::TypeDatetime {
        result_tp.set_flen(mysql::MaxDatetimeWidthNoFsp);
        if result_tp.get_decimal() > 0 {
            result_tp.set_flen(result_tp.get_flen() + result_tp.get_decimal() + 1);
        }
    }
}

/// AggregateEvalType aggregates arguments' EvalType of a multi-argument function.
// go: AggregateEvalType L1
pub fn aggregate_eval_type(fts: &[&FieldType], flag: &mut usize) -> EvalType {
    let mut aggregated_eval_type = ETString;
    let mut unsigned = false;
    let mut got_first = false;
    let mut got_bin_string = false;
    let mut lft = fts[0];
    for ft in fts {
        if ft.get_type() == mysql::TypeNull {
            continue;
        }
        let et = ft.eval_type();
        let rft = *ft;
        if (is_type_blob(ft.get_type())
            || is_type_varchar(ft.get_type())
            || is_type_char(ft.get_type()))
            && mysql::has_binary_flag(ft.get_flag())
        {
            got_bin_string = true;
        }
        if !got_first {
            got_first = true;
            aggregated_eval_type = et;
            unsigned = mysql::has_unsigned_flag(ft.get_flag());
        } else {
            aggregated_eval_type = merge_eval_type(
                aggregated_eval_type,
                et,
                lft,
                rft,
                unsigned,
                mysql::has_unsigned_flag(ft.get_flag()),
            );
            unsigned = unsigned && mysql::has_unsigned_flag(ft.get_flag());
        }
        lft = rft;
    }
    set_type_flag(flag, mysql::UnsignedFlag, unsigned);
    set_type_flag(
        flag,
        mysql::BinaryFlag,
        !aggregated_eval_type.is_string_kind() || got_bin_string,
    );
    aggregated_eval_type
}

// go: mergeEvalType L1
fn merge_eval_type(
    mut lhs: EvalType,
    mut rhs: EvalType,
    lft: &FieldType,
    rft: &FieldType,
    is_lhs_unsigned: bool,
    is_rhs_unsigned: bool,
) -> EvalType {
    if lft.get_type() == mysql::TypeUnspecified || rft.get_type() == mysql::TypeUnspecified {
        if lft.get_type() == rft.get_type() {
            return ETString;
        }
        if lft.get_type() == mysql::TypeUnspecified {
            lhs = rhs;
        } else {
            rhs = lhs;
        }
    }
    if lhs.is_string_kind() || rhs.is_string_kind() {
        return ETString;
    } else if lhs == ETReal || rhs == ETReal {
        return ETReal;
    } else if lhs == ETDecimal || rhs == ETDecimal || is_lhs_unsigned != is_rhs_unsigned {
        return ETDecimal;
    }
    ETInt
}

/// SetTypeFlag turns the flagItem on or off.
// go: SetTypeFlag L1
pub fn set_type_flag(flag: &mut usize, flag_item: usize, on: bool) {
    if on {
        *flag |= flag_item;
    } else {
        *flag &= !flag_item;
    }
}

/// InferParamTypeFromDatum is used for plan cache to infer the type of a parameter from its datum.
// go: InferParamTypeFromDatum L1
pub fn infer_param_type_from_datum(d: &Datum, tp: &mut FieldType) {
    infer_param_type_from_underlying_value(&d.get_value(), tp);
    if is_string_kind(d.kind()) {
        // consider charset and collation here
        let Ok(c) = collate::get_collation_by_name(d.collation()) else {
            return; // use default charset and collation
        };
        tp.set_charset(&c.charset_name);
        tp.set_collate(d.collation());
    }
}

/// InferParamTypeFromUnderlyingValue is used for plan cache to infer the type of a parameter from its underlying value.
// go: InferParamTypeFromUnderlyingValue L1
pub fn infer_param_type_from_underlying_value(value: &GoValue, tp: &mut FieldType) {
    match value {
        GoValue::Nil => {
            tp.set_type(mysql::TypeVarString);
            tp.set_flen(UnspecifiedLength);
            tp.set_decimal(UnspecifiedLength);
            // Also set the `charset` and `collation` for it, because some function (e.g. `json_object`) will return error
            // if the argument collation is `binary`.
            tp.set_charset(mysql::DefaultCharset);
            tp.set_collate(mysql::DefaultCollationName);
        }
        _ => {
            default_type_for_value(
                value,
                tp,
                mysql::DefaultCharset,
                mysql::DefaultCollationName,
            );
            if has_variant_field_length(tp) {
                tp.set_flen(UnspecifiedLength);
            }
            if tp.get_type() == mysql::TypeUnspecified {
                tp.set_type(mysql::TypeVarString);
            }
        }
    }
}

// go: hasVariantFieldLength L1
fn has_variant_field_length(tp: &FieldType) -> bool {
    matches!(
        tp.get_type(),
        mysql::TypeLonglong
            | mysql::TypeVarString
            | mysql::TypeDouble
            | mysql::TypeBlob
            | mysql::TypeBit
            | mysql::TypeDuration
            | mysql::TypeEnum
            | mysql::TypeSet
    )
}

/// `mathutil.StrLenOfUint64Fast` (pkg/util/mathutil): the number of decimal digits of x.
// glue: mathutil.StrLenOfUint64Fast
fn str_len_of_uint64_fast(x: u64) -> isize {
    const UINT_SIZE_TABLE: [u64; 21] = [
        0,
        9,
        99,
        999,
        9999,
        99999,
        999999,
        9999999,
        99999999,
        999999999,
        9999999999,
        99999999999,
        999999999999,
        9999999999999,
        99999999999999,
        999999999999999,
        9999999999999999,
        99999999999999999,
        999999999999999999,
        9999999999999999999,
        u64::MAX,
    ];
    let mut i = 1;
    loop {
        if x <= UINT_SIZE_TABLE[i] {
            return i as isize;
        }
        i += 1;
    }
}

/// `mathutil.StrLenOfInt64Fast` (pkg/util/mathutil); `Abs(MinInt64)` stays MinInt64, whose
/// uint64 value has 19 digits.
// glue: mathutil.StrLenOfInt64Fast
fn str_len_of_int64_fast(x: i64) -> isize {
    let mut size = 0;
    if x < 0 {
        size = 1; // add "-" sign on the length count
    }
    size + str_len_of_uint64_fast(x.wrapping_abs() as u64)
}

/// DefaultTypeForValue returns the default FieldType for the value.
// go: DefaultTypeForValue L1
pub fn default_type_for_value(value: &GoValue, tp: &mut FieldType, char: &str, collate: &str) {
    if !matches!(value, GoValue::Nil) {
        tp.add_flag(mysql::NotNullFlag);
    }
    match value {
        GoValue::Nil => {
            tp.set_type(mysql::TypeNull);
            tp.set_flen(0);
            tp.set_decimal(0);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Bool(_) => {
            tp.set_type(mysql::TypeLonglong);
            tp.set_flen(1);
            tp.set_decimal(0);
            tp.add_flag(mysql::IsBooleanFlag);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Int(x) | GoValue::Int64(x) => {
            tp.set_type(mysql::TypeLonglong);
            tp.set_flen(str_len_of_int64_fast(*x));
            tp.set_decimal(0);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Uint64(x) => {
            tp.set_type(mysql::TypeLonglong);
            tp.add_flag(mysql::UnsignedFlag);
            tp.set_flen(str_len_of_uint64_fast(*x));
            tp.set_decimal(0);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::String(x) => {
            tp.set_type(mysql::TypeVarString);
            // TODO: tp.flen should be len(x) * 3 (max bytes length of CharsetUTF8)
            tp.set_flen(x.len() as isize);
            tp.set_decimal(UnspecifiedLength);
            tp.set_charset(char);
            tp.set_collate(collate);
        }
        GoValue::Float32(x) => {
            tp.set_type(mysql::TypeFloat);
            let s = format_float(f64::from(*x), b'f', -1, 32);
            tp.set_flen(s.len() as isize);
            tp.set_decimal(UnspecifiedLength);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Float64(x) => {
            tp.set_type(mysql::TypeDouble);
            let s = format_float(*x, b'f', -1, 64);
            tp.set_flen(s.len() as isize);
            tp.set_decimal(UnspecifiedLength);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Bytes(x) => {
            tp.set_type(mysql::TypeBlob);
            tp.set_flen(x.as_ref().map_or(0, Vec::len) as isize);
            tp.set_decimal(UnspecifiedLength);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::BitLiteral(x) => {
            tp.set_type(mysql::TypeVarString);
            tp.set_flen(x.0.len() as isize * 3);
            tp.set_decimal(0);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::HexLiteral(x) => {
            tp.set_type(mysql::TypeVarString);
            tp.set_flen(x.0.len() as isize * 3);
            tp.set_decimal(0);
            tp.add_flag(mysql::UnsignedFlag);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::BinaryLiteral(x) => {
            tp.set_type(mysql::TypeVarString);
            tp.set_flen(x.0.len() as isize);
            tp.set_decimal(0);
            set_bin_chs_cln_flag(tp);
            tp.del_flag(mysql::BinaryFlag);
            tp.add_flag(mysql::UnsignedFlag);
        }
        GoValue::Time(x) => {
            tp.set_type(x.tp());
            match x.tp() {
                mysql::TypeDate => {
                    tp.set_flen(mysql::MaxDateWidth);
                    tp.set_decimal(UnspecifiedLength);
                }
                mysql::TypeDatetime | mysql::TypeTimestamp => {
                    tp.set_flen(mysql::MaxDatetimeWidthNoFsp);
                    if x.fsp() > DefaultFsp {
                        // consider point('.') and the fractional part.
                        tp.set_flen(tp.get_flen() + x.fsp() + 1);
                    }
                    tp.set_decimal(x.fsp());
                }
                _ => {}
            }
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Duration(x) => {
            tp.set_type(mysql::TypeDuration);
            tp.set_flen(x.string().len() as isize);
            if x.fsp > DefaultFsp {
                // consider point('.') and the fractional part.
                tp.set_flen(x.fsp + 1);
            }
            tp.set_decimal(x.fsp);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Decimal(x) => {
            tp.set_type(mysql::TypeNewDecimal);
            tp.set_flen_under_limit(x.to_string().len() as isize);
            tp.set_decimal_under_limit(x.get_digits_frac() as isize);
            // Add the length for `.`.
            tp.set_flen_under_limit(tp.get_flen() + 1);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Enum(x) => {
            tp.set_type(mysql::TypeEnum);
            tp.set_flen(x.name.len() as isize);
            tp.set_decimal(UnspecifiedLength);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::Set(x) => {
            tp.set_type(mysql::TypeSet);
            tp.set_flen(x.name.len() as isize);
            tp.set_decimal(UnspecifiedLength);
            set_bin_chs_cln_flag(tp);
        }
        GoValue::BinaryJSON(_) => {
            tp.set_type(mysql::TypeJSON);
            tp.set_flen(UnspecifiedLength);
            tp.set_decimal(0);
            tp.set_charset(CharsetUTF8MB4);
            tp.set_collate(CollationUTF8MB4);
        }
        GoValue::VectorFloat32(_) => {
            tp.set_type(mysql::TypeTiDBVectorFloat32);
            tp.set_flen(UnspecifiedLength);
            tp.set_decimal(0);
            set_bin_chs_cln_flag(tp);
        }
        _ => {
            tp.set_type(mysql::TypeUnspecified);
            tp.set_flen(UnspecifiedLength);
            tp.set_decimal(UnspecifiedLength);
            tp.set_charset(CharsetUTF8MB4);
            tp.set_collate(CollationUTF8MB4);
        }
    }
}

/// minFlenAndDecimalForType returns the minimum flen/decimal that can hold all the data for `tp`.
// go: minFlenAndDecimalForType L1
fn min_flen_and_decimal_for_type(tp: u8) -> (isize, isize) {
    match tp {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => mysql::get_default_field_length_and_decimal(tp),
        // todo support non-integer type
        _ => (UnspecifiedLength, UnspecifiedLength),
    }
}

/// DefaultCharsetForType returns the default charset/collation for mysql type.
// go: DefaultCharsetForType L1
pub fn default_charset_for_type(tp: u8) -> (&'static str, &'static str) {
    match tp {
        // Default charset for string types is utf8mb4.
        mysql::TypeVarString | mysql::TypeString | mysql::TypeVarchar => {
            (mysql::DefaultCharset, mysql::DefaultCollationName)
        }
        _ => (CharsetBin, CollationBin),
    }
}

/// mergeFieldType merges two MySQL type to a new type.
/// This is used in hybrid field type expression.
/// For example "select case c when 1 then 2 when 2 then 'tidb' from t;"
/// The result field type of the case expression is the merged type of the two when clause.
/// See https://github.com/mysql/mysql-server/blob/8.0/sql/field.cc#L1042
///
/// This function doesn't handle the range bump: for example, when the unsigned long is merged with signed long,
/// the result should be longlong. However, this function returns long for this case. Please use `AggFieldType`
/// function if you need to handle the range bump.
// go: mergeFieldType L1
fn merge_field_type(a: u8, b: u8) -> u8 {
    let ia = get_field_type_index(a);
    let ib = get_field_type_index(b);
    fieldTypeMergeRules[ia][ib]
}

/// mergeTypeFlag merges two MySQL type flag to a new one
/// currently only NotNullFlag and UnsignedFlag is checked
/// todo more flag need to be checked
// go: mergeTypeFlag L1
fn merge_type_flag(a: usize, b: usize) -> usize {
    a & (b & mysql::NotNullFlag | !mysql::NotNullFlag)
        & (b & mysql::UnsignedFlag | !mysql::UnsignedFlag)
}

/// The `fieldTypeIndexes` map; a type missing from it is index 0 (Go's map zero value).
// go: getFieldTypeIndex L2 (the map as a match)
fn get_field_type_index(tp: u8) -> usize {
    match tp {
        mysql::TypeUnspecified => 0,
        mysql::TypeTiny => 1,
        mysql::TypeShort => 2,
        mysql::TypeLong => 3,
        mysql::TypeFloat => 4,
        mysql::TypeDouble => 5,
        mysql::TypeNull => 6,
        mysql::TypeTimestamp => 7,
        mysql::TypeLonglong => 8,
        mysql::TypeInt24 => 9,
        mysql::TypeDate => 10,
        mysql::TypeDuration => 11,
        mysql::TypeDatetime => 12,
        mysql::TypeYear => 13,
        mysql::TypeNewDate => 14,
        mysql::TypeVarchar => 15,
        mysql::TypeBit => 16,
        mysql::TypeJSON => 17,
        mysql::TypeNewDecimal => 18,
        mysql::TypeEnum => 19,
        mysql::TypeSet => 20,
        mysql::TypeTinyBlob => 21,
        mysql::TypeMediumBlob => 22,
        mysql::TypeLongBlob => 23,
        mysql::TypeBlob => 24,
        mysql::TypeVarString => 25,
        mysql::TypeString => 26,
        mysql::TypeGeometry => 27,
        mysql::TypeTiDBVectorFloat32 => 28,
        _ => 0,
    }
}

// https://github.com/mysql/mysql-server/blob/8.0/sql/field.cc#L248
// (the table is copied from Go mechanically: `mysql.` → `mysql::`, braces → brackets)
#[rustfmt::skip]
const fieldTypeMergeRules: [[u8; 29]; 29] = [
    /* mysql::TypeUnspecified -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeNewDecimal,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeNewDecimal, mysql::TypeNewDecimal,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeUnspecified, mysql::TypeUnspecified,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeTiny -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeTiny,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeShort, mysql::TypeLong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeFloat, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeTiny, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeInt24,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeTiny,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeLonglong,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeShort -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeShort,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeShort, mysql::TypeLong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeFloat, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeShort, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeInt24,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeShort,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeLonglong,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeLong -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeLong,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeLong, mysql::TypeLong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeLong, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeLong,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeLong,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeLonglong,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeFloat -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeDouble, mysql::TypeFloat,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeFloat, mysql::TypeDouble,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeFloat, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeFloat, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeFloat, mysql::TypeFloat,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeFloat,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeDouble,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeDouble, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeDouble -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeDouble, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeDouble,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeDouble,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeDouble, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeNull -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeTiny,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeShort, mysql::TypeLong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeFloat, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeNull, mysql::TypeTimestamp,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeLonglong,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeDate, mysql::TypeDuration,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeDatetime, mysql::TypeYear,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeBit,
        // mysql::TypeJSON
        mysql::TypeJSON,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeEnum,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeSet, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeGeometry,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeTiDBVectorFloat32,
    ],
    /* mysql::TypeTimestamp -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeTimestamp, mysql::TypeTimestamp,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeDatetime, mysql::TypeDatetime,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeDatetime, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeLonglong -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeLonglong,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeLonglong, mysql::TypeLonglong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeLonglong, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeLong,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeLonglong,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeLonglong,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeInt24 -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeInt24,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeInt24, mysql::TypeLong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeFloat, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeInt24, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeInt24,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeInt24,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeLonglong,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal    mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeDate -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeDate, mysql::TypeDatetime,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeDate, mysql::TypeDatetime,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeDatetime, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeTime -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeDuration, mysql::TypeDatetime,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeDatetime, mysql::TypeDuration,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeDatetime, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeDatetime -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeDatetime, mysql::TypeDatetime,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeDatetime, mysql::TypeDatetime,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeDatetime, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeYear -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeUnspecified, mysql::TypeTiny,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeShort, mysql::TypeLong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeFloat, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeYear, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeInt24,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeYear,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeLonglong,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeNewDate -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeNewDate, mysql::TypeDatetime,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeNewDate, mysql::TypeDatetime,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeDatetime, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeNewDate, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeVarchar -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeBit -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeLonglong,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeLonglong, mysql::TypeLonglong,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeBit, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLonglong, mysql::TypeLonglong,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeLonglong,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeBit,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeJSON -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewFloat     mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeJSON, mysql::TypeVarchar,
        // mysql::TypeLongLONG     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         MYSQL_TYPE_TIME
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     MYSQL_TYPE_YEAR
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeJSON,
        // mysql::TypeNewDecimal   MYSQL_TYPE_ENUM
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeLongBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeLongBlob, mysql::TypeVarchar,
        // mysql::TypeString       MYSQL_TYPE_GEOMETRY
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeNewDecimal -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeNewDecimal, mysql::TypeNewDecimal,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeNewDecimal, mysql::TypeNewDecimal,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeDouble, mysql::TypeDouble,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeNewDecimal, mysql::TypeNewDecimal,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeNewDecimal,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeNewDecimal,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeNewDecimal, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeEnum -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeEnum, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeSet -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeSet, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeTinyBlob -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeTinyBlob,
        // mysql::TypeJSON
        mysql::TypeLongBlob,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeTinyBlob,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeTinyBlob, mysql::TypeTinyBlob,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeLongBlob,
    ],
    /* mysql::TypeMediumBlob -> */
    [
        // mysql::TypeUnspecified    mysql::TypeTiny
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeMediumBlob,
        // mysql::TypeJSON
        mysql::TypeLongBlob,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeMediumBlob, mysql::TypeMediumBlob,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeLongBlob,
    ],
    /* mysql::TypeLongBlob -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeLongBlob,
        // mysql::TypeJSON
        mysql::TypeLongBlob,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeLongBlob,
    ],
    /* mysql::TypeBlob -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeBlob,
        // mysql::TypeJSON
        mysql::TypeLongBlob,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeBlob, mysql::TypeBlob,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeLongBlob,
    ],
    /* mysql::TypeVarString -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeString -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeString,
        // mysql::TypeJSON
        mysql::TypeString,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeString, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeString,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeString,
    ],
    /* mysql::TypeGeometry -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeFloat        mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeGeometry, mysql::TypeVarchar,
        // mysql::TypeLonglong     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         mysql::TypeTime
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     mysql::TypeYear
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   mysql::TypeEnum
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeTinyBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeMediumBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeBlob, mysql::TypeVarchar,
        // mysql::TypeString       mysql::TypeGeometry
        mysql::TypeString, mysql::TypeGeometry,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeVarchar,
    ],
    /* mysql::TypeTiDBVectorFloat32 -> */
    [
        // mysql::TypeUnspecified  mysql::TypeTiny
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeShort        mysql::TypeLong
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewFloat     mysql::TypeDouble
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNull         mysql::TypeTimestamp
        mysql::TypeTiDBVectorFloat32, mysql::TypeVarchar,
        // mysql::TypeLongLONG     mysql::TypeInt24
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDate         MYSQL_TYPE_TIME
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeDatetime     MYSQL_TYPE_YEAR
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeNewDate      mysql::TypeVarchar
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeBit          <16>-<244>
        mysql::TypeVarchar,
        // mysql::TypeJSON
        mysql::TypeVarchar,
        // mysql::TypeNewDecimal   MYSQL_TYPE_ENUM
        mysql::TypeVarchar, mysql::TypeVarchar,
        // mysql::TypeSet          mysql::TypeTinyBlob
        mysql::TypeVarchar, mysql::TypeLongBlob,
        // mysql::TypeMediumBlob  mysql::TypeLongBlob
        mysql::TypeLongBlob, mysql::TypeLongBlob,
        // mysql::TypeBlob         mysql::TypeVarString
        mysql::TypeLongBlob, mysql::TypeVarchar,
        // mysql::TypeString       MYSQL_TYPE_GEOMETRY
        mysql::TypeString, mysql::TypeVarchar,
        // mysql::TypeTiDBVectorFloat32
        mysql::TypeTiDBVectorFloat32,
    ],
];

/// SetBinChsClnFlag sets charset, collation as 'binary' and adds binaryFlag to FieldType.
// go: SetBinChsClnFlag L1
pub fn set_bin_chs_cln_flag(ft: &mut FieldType) {
    ft.set_charset(CharsetBin);
    ft.set_collate(CollationBin);
    ft.add_flag(mysql::BinaryFlag);
}

/// CheckModifyTypeCompatible checks whether changes column type to another is compatible and can be changed.
/// If types are compatible and can be directly changed, nil err will be returned; otherwise the types are incompatible.
/// There are two cases when types incompatible:
/// 1. returned canReorg == true: types can be changed by reorg
/// 2. returned canReorg == false: type change not supported yet
// go: CheckModifyTypeCompatible L1
pub fn check_modify_type_compatible(
    origin: &FieldType,
    to: &FieldType,
) -> (bool, Result<(), Error>) {
    let unsupported = |msg: Vec<u8>| -> Result<(), Error> {
        Err(ErrUnsupportedModifyColumn.gen_with_stack_by_args(vec![Arg::go_string(msg)]))
    };
    // Deal with the same type.
    if origin.get_type() == to.get_type() {
        if origin.get_type() == mysql::TypeEnum || origin.get_type() == mysql::TypeSet {
            let mut type_var = "set";
            if origin.get_type() == mysql::TypeEnum {
                type_var = "enum";
            }
            let to_elems = to.get_elems().unwrap_or(&[]);
            let origin_elems = origin.get_elems().unwrap_or(&[]);
            if to_elems.len() < origin_elems.len() {
                let msg = format!(
                    "the number of {type_var} column's elements is less than the original: {}",
                    origin_elems.len()
                );
                return (true, unsupported(msg.into_bytes()));
            }
            for (index, origin_elem) in origin_elems.iter().enumerate() {
                let to_elem = &to_elems[index];
                if origin_elem != to_elem {
                    let mut msg = format!("cannot modify {type_var} column value ").into_bytes();
                    msg.extend_from_slice(origin_elem);
                    msg.extend_from_slice(b" to ");
                    msg.extend_from_slice(to_elem);
                    return (true, unsupported(msg));
                }
            }
        }

        if origin.get_type() == mysql::TypeNewDecimal {
            // Floating-point and fixed-point types also can be UNSIGNED. As with integer types, this attribute prevents
            // negative values from being stored in the column. Unlike the integer types, the upper range of column values
            // remains the same.
            if to.get_flen() != origin.get_flen()
                || to.get_decimal() != origin.get_decimal()
                || mysql::has_unsigned_flag(to.get_flag())
                    != mysql::has_unsigned_flag(origin.get_flag())
            {
                let msg = format!(
                    "decimal change from decimal({}, {}) to decimal({}, {})",
                    origin.get_flen(),
                    origin.get_decimal(),
                    to.get_flen(),
                    to.get_decimal()
                );
                return (true, unsupported(msg.into_bytes()));
            }
        }

        let (need_reorg, reason) = need_reorg_to_change(origin, to);
        if !need_reorg {
            return (false, Ok(()));
        }
        return (true, unsupported(reason.into_bytes()));
    }

    // Deal with the different type.
    if !check_type_change_supported(origin, to) {
        let unsupported_msg = format!(
            "change from original type {} to {} is currently unsupported yet",
            origin.compact_str(),
            to.compact_str()
        );
        return (false, unsupported(unsupported_msg.into_bytes()));
    }

    // Check if different type can directly convert and no need to reorg.
    let string_to_string = is_string(origin.get_type()) && is_string(to.get_type());
    let integer_to_integer =
        mysql::is_integer_type(origin.get_type()) && mysql::is_integer_type(to.get_type());
    if string_to_string || integer_to_integer {
        let (need_reorg, reason) = need_reorg_to_change(origin, to);
        if !need_reorg {
            return (false, Ok(()));
        }
        return (true, unsupported(reason.into_bytes()));
    }

    let not_compatible_msg = format!(
        "type {} not match origin {}",
        to.compact_str(),
        origin.compact_str()
    );
    (true, unsupported(not_compatible_msg.into_bytes()))
}

// go: needReorgToChange L1
fn need_reorg_to_change(origin: &FieldType, to: &FieldType) -> (bool, String) {
    let mut to_flen = to.get_flen();
    let mut origin_flen = origin.get_flen();
    if mysql::is_integer_type(to.get_type()) && mysql::is_integer_type(origin.get_type()) {
        // For integers, we should ignore the potential display length represented by flen, using
        // the default flen of the type.
        (origin_flen, _) = mysql::get_default_field_length_and_decimal(origin.get_type());
        (to_flen, _) = mysql::get_default_field_length_and_decimal(to.get_type());
    }

    if convert_between_char_and_varchar(origin.get_type(), to.get_type()) {
        return (
            true,
            "conversion between char and varchar string needs reorganization".to_owned(),
        );
    }

    if to_flen > 0 && to_flen != origin_flen {
        if to_flen < origin_flen {
            return (
                true,
                format!("length {to_flen} is less than origin {origin_flen}"),
            );
        }

        // Due to the behavior of padding \x00 at binary type, we need to reorg when binary length changed
        let is_binary_type =
            |tp: &FieldType| tp.get_type() == mysql::TypeString && is_binary_str(tp);
        if is_binary_type(origin) && is_binary_type(to) {
            return (
                true,
                "can't change binary types of different length".to_owned(),
            );
        }
    }
    if to.get_decimal() > 0 && to.get_decimal() < origin.get_decimal() {
        return (
            true,
            format!(
                "decimal {} is less than origin {}",
                to.get_decimal(),
                origin.get_decimal()
            ),
        );
    }
    if mysql::has_unsigned_flag(origin.get_flag()) != mysql::has_unsigned_flag(to.get_flag()) {
        return (
            true,
            "can't change unsigned integer to signed or vice versa".to_owned(),
        );
    }
    (false, String::new())
}

// go: checkTypeChangeSupported L1
fn check_type_change_supported(origin: &FieldType, to: &FieldType) -> bool {
    let o = origin.get_type();
    let t = to.get_type();
    if (is_type_time(o)
        || o == mysql::TypeDuration
        || o == mysql::TypeYear
        || is_string(o)
        || o == mysql::TypeJSON)
        && t == mysql::TypeBit
    {
        // TODO: Currently date/datetime/timestamp/time/year/string/json data type cast to bit are not compatible with mysql, should fix here after compatible.
        return false;
    }

    if (is_type_time(o)
        || o == mysql::TypeDuration
        || o == mysql::TypeYear
        || o == mysql::TypeNewDecimal
        || o == mysql::TypeFloat
        || o == mysql::TypeDouble
        || o == mysql::TypeJSON
        || o == mysql::TypeBit)
        && (t == mysql::TypeEnum || t == mysql::TypeSet)
    {
        // TODO: Currently date/datetime/timestamp/time/year/decimal/float/double/json/bit cast to enum/set are not support yet, should fix here after supported.
        return false;
    }

    if (o == mysql::TypeEnum
        || o == mysql::TypeSet
        || o == mysql::TypeBit
        || o == mysql::TypeNewDecimal
        || o == mysql::TypeFloat
        || o == mysql::TypeDouble)
        && is_type_time(t)
    {
        // TODO: Currently enum/set/bit/decimal/float/double cast to date/datetime/timestamp type are not support yet, should fix here after supported.
        return false;
    }

    if o == mysql::TypeTiDBVectorFloat32 || t == mysql::TypeTiDBVectorFloat32 {
        // TODO: Vector type not supported.
        return false;
    }

    if (o == mysql::TypeEnum || o == mysql::TypeSet || o == mysql::TypeBit)
        && t == mysql::TypeDuration
    {
        // TODO: Currently enum/set/bit cast to time are not support yet, should fix here after supported.
        return false;
    }

    true
}

/// ConvertBetweenCharAndVarchar is that Column type conversion between varchar to char need reorganization because
/// 1. varchar -> char: char type is stored with the padding removed. All the indexes need to be rewritten.
/// 2. char -> varchar: the index value encoding of secondary index on clustered primary key tables is different.
///    These secondary indexes need to be rewritten.
// go: ConvertBetweenCharAndVarchar L1
pub fn convert_between_char_and_varchar(old_col: u8, new_col: u8) -> bool {
    (is_type_varchar(old_col) && new_col == mysql::TypeString)
        || (old_col == mysql::TypeString
            && is_type_varchar(new_col)
            && collate::new_collation_enabled())
}

/// IsVarcharTooBigFieldLength check if the varchar type column exceeds the maximum length limit.
// go: IsVarcharTooBigFieldLength L1
pub fn is_varchar_too_big_field_length(
    col_def_tp_flen: isize,
    col_def_name: &[u8],
    set_charset: &str,
) -> Result<(), Error> {
    let (desc, err) = gocharset::get_charset_info(set_charset);
    err?;
    let desc = desc.expect("charset info");
    let mut max_flen = mysql::MaxFieldVarCharLength;
    max_flen /= desc.maxlen;
    if col_def_tp_flen != UnspecifiedLength && col_def_tp_flen > max_flen {
        return Err(ErrTooBigFieldLength.gen_with_stack(
            "Column length too big for column '%s' (max = %d); use BLOB or TEXT instead",
            vec![
                Arg::go_string(col_def_name.to_vec()),
                Arg::Int(max_flen as i64, "int"),
            ],
        ));
    }
    Ok(())
}
