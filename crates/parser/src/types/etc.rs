//! go-file: pkg/parser/types/etc.go

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::mysql;
use tidb_base::terror::ClassTypes;

/// IsTypeBlob returns a boolean indicating whether the tp is a blob type.
// go: IsTypeBlob L1
pub fn is_type_blob(tp: u8) -> bool {
    matches!(
        tp,
        mysql::TypeTinyBlob | mysql::TypeMediumBlob | mysql::TypeBlob | mysql::TypeLongBlob
    )
}

/// IsTypeChar returns a boolean indicating whether the tp is the char type like a string
/// type or a varchar type.
// go: IsTypeChar L1
pub fn is_type_char(tp: u8) -> bool {
    tp == mysql::TypeString || tp == mysql::TypeVarchar
}

/// IsTypeVector returns whether tp is a vector type.
// go: IsTypeVector L1
pub fn is_type_vector(tp: u8) -> bool {
    tp == mysql::TypeTiDBVectorFloat32
}

/// `type2Str` (a Go map literal; looked up by key, so its order does not matter).
static type2Str: &[(u8, &str)] = &[
    (mysql::TypeBit, "bit"),
    (mysql::TypeBlob, "text"),
    (mysql::TypeDate, "date"),
    (mysql::TypeDatetime, "datetime"),
    (mysql::TypeUnspecified, "unspecified"),
    (mysql::TypeNewDecimal, "decimal"),
    (mysql::TypeDouble, "double"),
    (mysql::TypeEnum, "enum"),
    (mysql::TypeFloat, "float"),
    (mysql::TypeGeometry, "geometry"),
    (mysql::TypeTiDBVectorFloat32, "vector"),
    (mysql::TypeInt24, "mediumint"),
    (mysql::TypeJSON, "json"),
    (mysql::TypeLong, "int"),
    (mysql::TypeLonglong, "bigint"),
    (mysql::TypeLongBlob, "longtext"),
    (mysql::TypeMediumBlob, "mediumtext"),
    (mysql::TypeNull, "null"),
    (mysql::TypeSet, "set"),
    (mysql::TypeShort, "smallint"),
    (mysql::TypeString, "char"),
    (mysql::TypeDuration, "time"),
    (mysql::TypeTimestamp, "timestamp"),
    (mysql::TypeTiny, "tinyint"),
    (mysql::TypeTinyBlob, "tinytext"),
    (mysql::TypeVarchar, "varchar"),
    (mysql::TypeVarString, "var_string"),
    (mysql::TypeYear, "year"),
];

/// `type2Str[tp]` (the zero value "" for a missing key).
// glue: Go map index of the type2Str literal
fn type_to_str_map(tp: u8) -> &'static str {
    type2Str
        .iter()
        .find(|(t, _)| *t == tp)
        .map_or("", |(_, s)| s)
}

/// TypeStr converts tp to a string.
// go: TypeStr L1
pub fn type_str(tp: u8) -> &'static str {
    type_to_str_map(tp)
}

/// TypeToStr converts a field to a string. It is used for converting Text to Blob, or
/// converting Char to Binary.
// go: TypeToStr L1
pub fn type_to_str(tp: u8, cs: &str) -> String {
    let ts = type_to_str_map(tp);
    if cs != "binary" {
        return ts.to_owned();
    }
    if is_type_blob(tp) {
        ts.replacen("text", "blob", 1)
    } else if is_type_char(tp) {
        ts.replacen("char", "binary", 1)
    } else if tp == mysql::TypeNull {
        "binary".to_owned()
    } else {
        ts.to_owned()
    }
}

/// StrToType convert a string to type enum.
// go: StrToType L1
pub fn str_to_type(ts: &str) -> u8 {
    let ts = ts.replacen("blob", "text", 1).replacen("binary", "char", 1);
    // str2Type is the inverse of type2Str.
    type2Str
        .iter()
        .find(|(_, s)| *s == ts)
        .map_or(mysql::TypeUnspecified, |(t, _)| *t)
}

/// `dig2bytes`.
pub(crate) const dig2bytes: [isize; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];

/// A word holds 9 digits.
pub(crate) const digitsPerWord: isize = 9;
/// A word is 4 bytes int32.
pub(crate) const wordSize: isize = 4;

/// ErrInvalidDefault is returned when meet a invalid default value.
pub static ErrInvalidDefault: LazyLock<Error> =
    LazyLock::new(|| ClassTypes.new_std(mysql::ErrInvalidDefault));
/// ErrDataOutOfRange is returned when meet a value out of range.
pub static ErrDataOutOfRange: LazyLock<Error> =
    LazyLock::new(|| ClassTypes.new_std(mysql::ErrDataOutOfRange));
/// ErrTruncatedWrongValue is returned when meet a value bigger than
/// 99999999999999999999999999999999999999999999999999999999999999999 during parsing.
pub static ErrTruncatedWrongValue: LazyLock<Error> =
    LazyLock::new(|| ClassTypes.new_std(mysql::ErrTruncatedWrongValue));
/// ErrIllegalValueForType is returned when strconv.ParseFloat meet strconv.ErrRange during
/// parsing.
pub static ErrIllegalValueForType: LazyLock<Error> =
    LazyLock::new(|| ClassTypes.new_std(mysql::ErrIllegalValueForType));
