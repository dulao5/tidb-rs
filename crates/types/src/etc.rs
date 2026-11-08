//! go-file: pkg/types/etc.go
//!
//! `ColumnNeedRestoredData` takes the model structs and lives in tidb-model (Go's
//! `pkg/types` imports `pkg/meta/model`; the Rust crates are layered the other way).
//! `IsTypeBlob`, `IsTypeChar`, `IsTypeVector`, `TypeStr` and `TypeToStr` are Go variables
//! aliasing the parser's functions: re-exported here.

#![allow(non_upper_case_globals)]

use tidb_base::gofmt::Arg;
use tidb_base::mysql;
use tidb_parser::gocharset;
pub use tidb_parser::types::{
    FieldType, is_type_blob, is_type_char, is_type_vector, type_str, type_to_str,
};

use crate::collate;
use crate::datum::*;
use crate::errors::{ErrOverflow, Error};

/// IsTypeVarchar returns a boolean indicating
/// whether the tp is the varchar type like a varstring type or a varchar type.
// go: IsTypeVarchar L1
pub fn is_type_varchar(tp: u8) -> bool {
    tp == mysql::TypeVarString || tp == mysql::TypeVarchar
}

/// IsTypeUnspecified returns a boolean indicating whether the tp is the Unspecified type.
// go: IsTypeUnspecified L1
pub fn is_type_unspecified(tp: u8) -> bool {
    tp == mysql::TypeUnspecified
}

/// IsTypePrefixable returns a boolean indicating
/// whether an index on a column with the tp can be defined with a prefix.
// go: IsTypePrefixable L1
pub fn is_type_prefixable(tp: u8) -> bool {
    is_type_blob(tp) || is_type_char(tp)
}

/// IsTypeFractionable returns a boolean indicating
/// whether the tp can has time fraction.
// go: IsTypeFractionable L1
pub fn is_type_fractionable(tp: u8) -> bool {
    tp == mysql::TypeDatetime || tp == mysql::TypeDuration || tp == mysql::TypeTimestamp
}

/// IsTypeTime returns a boolean indicating
/// whether the tp is time type like datetime, date or timestamp.
// go: IsTypeTime L1
pub fn is_type_time(tp: u8) -> bool {
    tp == mysql::TypeDatetime || tp == mysql::TypeDate || tp == mysql::TypeTimestamp
}

/// IsTypeFloat indicates whether the type is TypeFloat
// go: IsTypeFloat L1
pub fn is_type_float(tp: u8) -> bool {
    tp == mysql::TypeFloat
}

/// IsTypeInteger returns a boolean indicating whether the tp is integer type.
// go: IsTypeInteger L1
pub fn is_type_integer(tp: u8) -> bool {
    matches!(
        tp,
        mysql::TypeTiny
            | mysql::TypeShort
            | mysql::TypeInt24
            | mysql::TypeLong
            | mysql::TypeLonglong
            | mysql::TypeYear
    )
}

/// IsTypeNumeric returns a boolean indicating whether the tp is numeric type.
// go: IsTypeNumeric L1
pub fn is_type_numeric(tp: u8) -> bool {
    matches!(
        tp,
        mysql::TypeBit
            | mysql::TypeTiny
            | mysql::TypeInt24
            | mysql::TypeLong
            | mysql::TypeLonglong
            | mysql::TypeNewDecimal
            | mysql::TypeFloat
            | mysql::TypeDouble
            | mysql::TypeShort
    )
}

/// IsTypeBit returns a boolean indicating whether the tp is bit type.
// go: IsTypeBit L1
pub fn is_type_bit(ft: &FieldType) -> bool {
    ft.get_type() == mysql::TypeBit
}

/// IsTemporalWithDate returns a boolean indicating
/// whether the tp is time type with date.
// go: IsTemporalWithDate L1
pub fn is_temporal_with_date(tp: u8) -> bool {
    is_type_time(tp)
}

/// IsBinaryStr returns a boolean indicating
/// whether the field type is a binary string type.
// go: IsBinaryStr L1
pub fn is_binary_str(ft: &FieldType) -> bool {
    ft.get_collate() == gocharset::CollationBin && is_string(ft.get_type())
}

/// IsNonBinaryStr returns a boolean indicating
/// whether the field type is a non-binary string type.
// go: IsNonBinaryStr L1
pub fn is_non_binary_str(ft: &FieldType) -> bool {
    if ft.get_collate() != gocharset::CollationBin && is_string(ft.get_type()) {
        return true;
    }
    false
}

/// NeedRestoredData returns if a type needs restored data.
/// If the type is char and the collation is _bin, NeedRestoredData() returns false.
// go: NeedRestoredData L1
pub fn need_restored_data(ft: &FieldType) -> bool {
    if collate::new_collation_enabled()
        && is_non_binary_str(ft)
        && (!collate::is_bin_collation(ft.get_collate()) || is_type_varchar(ft.get_type()))
        && ft.get_collate() != "utf8mb4_0900_bin"
    {
        return true;
    }
    false
}

/// IsString returns a boolean indicating
/// whether the field type is a string type.
// go: IsString L1
pub fn is_string(tp: u8) -> bool {
    is_type_char(tp) || is_type_blob(tp) || is_type_varchar(tp) || is_type_unspecified(tp)
}

/// IsStringKind returns a boolean indicating whether the tp is a string type.
// go: IsStringKind L1
pub fn is_string_kind(kind: u8) -> bool {
    kind == KindString || kind == KindBytes
}

/// `kind2Str`.
// glue: Go package-level map kind2Str as a match
fn kind2_str(kind: u8) -> Option<&'static str> {
    Some(match kind {
        KindNull => "null",
        KindInt64 => "bigint",
        KindUint64 => "unsigned bigint",
        KindFloat32 => "float",
        KindFloat64 => "double",
        KindString => "char",
        KindBytes => "bytes",
        KindBinaryLiteral => "bit/hex literal",
        KindMysqlDecimal => "decimal",
        KindMysqlDuration => "time",
        KindMysqlEnum => "enum",
        KindMysqlBit => "bit",
        KindMysqlSet => "set",
        KindMysqlTime => "datetime",
        KindInterface => "interface",
        KindMinNotNull => "min_not_null",
        KindMaxValue => "max_value",
        KindRaw => "raw",
        KindMysqlJSON => "json",
        KindVectorFloat32 => "vector",
        _ => return None,
    })
}

/// KindStr converts kind to a string.
// go: KindStr L1
pub fn kind_str(kind: u8) -> &'static str {
    kind2_str(kind).unwrap_or("")
}

/// overflow returns an overflowed error.
///
/// `v` is the Go `any` argument formatted with `%v`.
// go: overflow L1
pub(crate) fn overflow(v: Arg, tp: u8) -> Error {
    ErrOverflow.gen_with_stack("constant %v overflows %s", vec![v, Arg::from(type_str(tp))])
}

/// IsTypeTemporal checks if a type is a temporal type.
// go: IsTypeTemporal L1
pub fn is_type_temporal(tp: u8) -> bool {
    matches!(
        tp,
        mysql::TypeDuration
            | mysql::TypeDatetime
            | mysql::TypeTimestamp
            | mysql::TypeDate
            | mysql::TypeNewDate
    )
}
