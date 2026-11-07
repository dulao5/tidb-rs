//! go-file: pkg/parser/mysql/privs.go
//!
//! Privilege constants, the maps and the privilege lists are in `privs_gen.rs` (generated).

use std::ops::{BitAnd, BitOr, Not, Sub};

use super::*;

/// PrivilegeType privilege
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrivilegeType(pub u64);

impl BitAnd for PrivilegeType {
    type Output = PrivilegeType;
    // glue: Go integer & on the named type PrivilegeType
    fn bitand(self, rhs: Self) -> Self {
        PrivilegeType(self.0 & rhs.0)
    }
}
impl BitOr for PrivilegeType {
    type Output = PrivilegeType;
    // glue: Go integer | on the named type PrivilegeType
    fn bitor(self, rhs: Self) -> Self {
        PrivilegeType(self.0 | rhs.0)
    }
}
impl Not for PrivilegeType {
    type Output = PrivilegeType;
    // glue: Go integer ^ (bitwise not) on the named type PrivilegeType
    fn not(self) -> Self {
        PrivilegeType(!self.0)
    }
}
impl Sub for PrivilegeType {
    type Output = PrivilegeType;
    // glue: Go integer - on the named type PrivilegeType (wraps, M-INT)
    fn sub(self, rhs: Self) -> Self {
        PrivilegeType(self.0.wrapping_sub(rhs.0))
    }
}

/// NewPrivFromColumn constructs priv from a column name. False means invalid priv column name.
// go: NewPrivFromColumn L1
pub fn new_priv_from_column(col: &str) -> (PrivilegeType, bool) {
    match Col2PrivType.get(col) {
        Some(p) => (*p, true),
        None => (PrivilegeType(0), false),
    }
}

/// NewPrivFromSetEnum constructs priv from a set enum. False means invalid priv enum.
// go: NewPrivFromSetEnum L1
pub fn new_priv_from_set_enum(e: &str) -> (PrivilegeType, bool) {
    match SetStr2Priv.get(e) {
        Some(p) => (*p, true),
        None => (PrivilegeType(0), false),
    }
}

impl PrivilegeType {
    /// String returns the corresponding identifier in SQLs.
    // go: PrivilegeType.String L1 [M-NAME]
    pub fn string(self) -> &'static str {
        if let Some(s) = Priv2Str.get(&self) {
            return s;
        }
        ""
    }

    /// ColumnString returns the corresponding name of columns in mysql.user/mysql.db.
    // go: PrivilegeType.ColumnString L1
    pub fn column_string(self) -> &'static str {
        if let Some(s) = Priv2UserCol.get(&self) {
            return s;
        }
        ""
    }

    /// SetString returns the corresponding set enum string in Table_priv/Column_priv of
    /// mysql.tables_priv/mysql.columns_priv.
    // go: PrivilegeType.SetString L1
    pub fn set_string(self) -> &'static str {
        if let Some(s) = Priv2SetStr.get(&self) {
            return s;
        }
        ""
    }
}

/// Privileges is the list of all privileges (Go `type Privileges []PrivilegeType`).
pub type Privileges = [PrivilegeType];

/// Has checks whether PrivilegeType has the privilege.
// go: Privileges.Has L2 (method on a slice type alias: free function)
pub fn privileges_has(privs: &Privileges, p: PrivilegeType) -> bool {
    for cp in privs {
        if *cp == p {
            return true;
        }
    }
    false
}
