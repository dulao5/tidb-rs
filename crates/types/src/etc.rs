//! go-file: pkg/types/etc.go
//!
//! Part of `etc.go` (block D01.1), ported ahead of the block for D06.1's
//! `ColumnNeedRestoredData` (in tidb-model, since it takes the model structs: Go's
//! `pkg/types` imports `pkg/meta/model`, the Rust crates are layered the other way). The rest
//! of the file comes with D01.1.

use tidb_base::mysql;
use tidb_parser::gocharset;
use tidb_parser::types::{FieldType, is_type_blob, is_type_char};

use crate::collate;

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
