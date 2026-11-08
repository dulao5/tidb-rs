//! go-file: pkg/types/field_name.go
//!
//! `NameSlice.FindAstColName` takes `*ast.ColumnName`, a parser AST node that is not yet
//! faithfully ported (the PoC AST is a stand-in): it is exempted, blocked by D02.2.

#![allow(non_upper_case_globals)]

use std::sync::{Arc, LazyLock};

use tidb_parser::model::CIStr;

/// FieldName records the names used for mysql protocol.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldName {
    pub orig_tbl_name: CIStr,
    pub orig_col_name: CIStr,
    pub db_name: CIStr,
    pub tbl_name: CIStr,
    pub col_name: CIStr,

    pub hidden: bool,

    /// NotExplicitUsable is used for mark whether a column can be explicit used in SQL.
    /// update stmt can write `writeable` column implicitly but cannot use non-public columns explicit.
    /// e.g. update t set a = 10 where b = 10; which `b` is in `writeOnly` state
    pub not_explicit_usable: bool,

    pub redundant: bool,
}

const emptyName: &str = "EMPTY_NAME";

/// `size.SizeOfBool`.
const SIZE_OF_BOOL: i64 = 1;

impl FieldName {
    /// String implements Stringer interface.
    // go: FieldName.String L1
    pub fn string(&self) -> String {
        let mut builder = String::new();
        if self.hidden {
            return emptyName.to_owned();
        }
        if !self.db_name.l.is_empty() {
            builder.push_str(&self.db_name.l);
            builder.push('.');
        }
        if !self.tbl_name.l.is_empty() {
            builder.push_str(&self.tbl_name.l);
            builder.push('.');
        }
        builder.push_str(&self.col_name.l);
        builder
    }

    /// MemoryUsage return the memory usage of FieldName
    // go: FieldName.MemoryUsage L2 (a nil receiver is the caller's None)
    pub fn memory_usage(&self) -> i64 {
        self.orig_tbl_name.memory_usage()
            + self.orig_col_name.memory_usage()
            + self.db_name.memory_usage()
            + self.tbl_name.memory_usage()
            + self.col_name.memory_usage()
            + SIZE_OF_BOOL * 3
    }

    /// Clone returns a shallow copy of the FieldName struct.
    /// All fields are value types (CIStr, bool), so a shallow copy is a full copy.
    // go: FieldName.Clone L2 (a nil receiver is the caller's None)
    pub fn clone_name(&self) -> FieldName {
        self.clone()
    }
}

/// NameSlice is the slice of the *fieldName
pub type NameSlice = Vec<Arc<FieldName>>;

/// Shallow is a shallow copy, only making a new slice.
// go: NameSlice.Shallow L1
pub fn shallow(s: &[Arc<FieldName>]) -> NameSlice {
    s.to_vec()
}

/// EmptyName is to occupy the position in the name slice. If it's set, that column's name is hidden.
pub static EmptyName: LazyLock<Arc<FieldName>> = LazyLock::new(|| {
    Arc::new(FieldName {
        hidden: true,
        ..FieldName::default()
    })
});
