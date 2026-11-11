//! go-file: pkg/parser/mysql/type.go
//!
//! Type and flag constants are in `type_gen.rs` (generated).

use super::*;

/// HasDropColumnWithIndexFlag checks if DropColumnIndexFlag is set.
// go: HasDropColumnWithIndexFlag L1 [M-INT]
pub fn has_drop_column_with_index_flag(flag: usize) -> bool {
    (flag & DropColumnIndexFlag) > 0
}

/// HasNotNullFlag checks if NotNullFlag is set.
// go: HasNotNullFlag L1 [M-INT]
pub fn has_not_null_flag(flag: usize) -> bool {
    (flag & NotNullFlag) > 0
}

/// HasNoDefaultValueFlag checks if NoDefaultValueFlag is set.
// go: HasNoDefaultValueFlag L1 [M-INT]
pub fn has_no_default_value_flag(flag: usize) -> bool {
    (flag & NoDefaultValueFlag) > 0
}

/// HasAutoIncrementFlag checks if AutoIncrementFlag is set.
// go: HasAutoIncrementFlag L1 [M-INT]
pub fn has_auto_increment_flag(flag: usize) -> bool {
    (flag & AutoIncrementFlag) > 0
}

/// HasUnsignedFlag checks if UnsignedFlag is set.
// go: HasUnsignedFlag L1 [M-INT]
#[inline]
pub fn has_unsigned_flag(flag: usize) -> bool {
    (flag & UnsignedFlag) > 0
}

/// HasZerofillFlag checks if ZerofillFlag is set.
// go: HasZerofillFlag L1 [M-INT]
pub fn has_zerofill_flag(flag: usize) -> bool {
    (flag & ZerofillFlag) > 0
}

/// HasBinaryFlag checks if BinaryFlag is set.
// go: HasBinaryFlag L1 [M-INT]
pub fn has_binary_flag(flag: usize) -> bool {
    (flag & BinaryFlag) > 0
}

/// HasPriKeyFlag checks if PriKeyFlag is set.
// go: HasPriKeyFlag L1 [M-INT]
pub fn has_pri_key_flag(flag: usize) -> bool {
    (flag & PriKeyFlag) > 0
}

/// HasUniKeyFlag checks if UniqueKeyFlag is set.
// go: HasUniKeyFlag L1 [M-INT]
pub fn has_uni_key_flag(flag: usize) -> bool {
    (flag & UniqueKeyFlag) > 0
}

/// HasMultipleKeyFlag checks if MultipleKeyFlag is set.
// go: HasMultipleKeyFlag L1 [M-INT]
pub fn has_multiple_key_flag(flag: usize) -> bool {
    (flag & MultipleKeyFlag) > 0
}

/// HasTimestampFlag checks if a field is a timestamp field.
// go: HasTimestampFlag L1 [M-INT]
pub fn has_timestamp_flag(flag: usize) -> bool {
    (flag & TimestampFlag) > 0
}

/// HasOnUpdateNowFlag checks if a field is on_update_now.
// go: HasOnUpdateNowFlag L1 [M-INT]
pub fn has_on_update_now_flag(flag: usize) -> bool {
    (flag & OnUpdateNowFlag) > 0
}

/// HasParseToJSONFlag checks if ParseToJSONFlag is set.
// go: HasParseToJSONFlag L1 [M-INT]
pub fn has_parse_to_json_flag(flag: usize) -> bool {
    (flag & ParseToJSONFlag) > 0
}

/// HasIsBooleanFlag checks if IsBooleanFlag is set.
// go: HasIsBooleanFlag L1 [M-INT]
pub fn has_is_boolean_flag(flag: usize) -> bool {
    (flag & IsBooleanFlag) > 0
}

/// HasPreventNullInsertFlag checks if PreventNullInsertFlag is set.
// go: HasPreventNullInsertFlag L1 [M-INT]
pub fn has_prevent_null_insert_flag(flag: usize) -> bool {
    (flag & PreventNullInsertFlag) > 0
}

/// HasEnumSetAsIntFlag checks if EnumSetAsIntFlag is set.
// go: HasEnumSetAsIntFlag L1 [M-INT]
pub fn has_enum_set_as_int_flag(flag: usize) -> bool {
    (flag & EnumSetAsIntFlag) > 0
}
