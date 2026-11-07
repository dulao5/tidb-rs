//! go-file: pkg/parser/mysql/util.go
//!
//! The tables `defaultLengthAndDecimal(ForCast)` are in `util_gen.rs` (generated).

use super::*;

/// `lengthAndDecimal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct lengthAndDecimal {
    pub length: i64,
    pub decimal: isize,
}

/// IsIntegerType indicate whether tp is an integer type.
// go: IsIntegerType L1
pub fn is_integer_type(tp: u8) -> bool {
    matches!(
        tp,
        TypeTiny | TypeShort | TypeInt24 | TypeLong | TypeLonglong
    )
}

/// GetDefaultFieldLengthAndDecimal returns the default display length (flen) and decimal
/// length for column. Call this when no flen assigned in ddl.
/// or column value is calculated from an expression.
/// For example: "select count(*) from t;", the column type is int64 and flen in ResultField
/// will be 21.
/// See https://dev.mysql.com/doc/refman/5.7/en/storage-requirements.html
// go: GetDefaultFieldLengthAndDecimal L1 [M-INT]
pub fn get_default_field_length_and_decimal(tp: u8) -> (isize, isize) {
    if let Some(val) = defaultLengthAndDecimal.get(&tp) {
        return (val.length as isize, val.decimal);
    }
    (-1, -1)
}

/// GetDefaultFieldLengthAndDecimalForCast returns the default display length (flen) and
/// decimal length for casted column when flen or decimal is not specified.
// go: GetDefaultFieldLengthAndDecimalForCast L1 [M-INT]
pub fn get_default_field_length_and_decimal_for_cast(tp: u8) -> (isize, isize) {
    if let Some(val) = defaultLengthAndDecimalForCast.get(&tp) {
        return (val.length as isize, val.decimal);
    }
    (-1, -1)
}

/// IsAuthPluginClearText is used to indicated that the plugin need clear-text password.
// go: IsAuthPluginClearText L1
pub fn is_auth_plugin_clear_text(auth_plugin: &str) -> bool {
    auth_plugin == AuthNativePassword
        || auth_plugin == AuthTiDBSM3Password
        || auth_plugin == AuthCachingSha2Password
}
