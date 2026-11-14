//! go-file: pkg/meta/model/db.go

use std::sync::Arc;

use crate::{DBInfo, DBInfoDeprecated, TableInfo};

impl Clone for DBInfo {
    /// Clone clones DBInfo (each table of the deprecated list is cloned; the list is never
    /// nil).
    // go: DBInfo.Clone L1
    fn clone(&self) -> DBInfo {
        let mut new_info = self.struct_copy();
        new_info.deprecated = DBInfoDeprecated {
            tables: Some(
                self.deprecated
                    .tables
                    .iter()
                    .flatten()
                    .map(|t| Arc::new(TableInfo::clone(t)))
                    .collect(),
            ),
        };
        new_info
    }
}

impl DBInfo {
    /// Copy shallow copies DBInfo: a new deprecated table list sharing the `TableInfo`s
    /// (spec N13).
    // go: DBInfo.Copy L1 [M-SHARE]
    pub fn copy(&self) -> DBInfo {
        let mut new_info = self.struct_copy();
        new_info.deprecated.tables = Some(
            self.deprecated
                .tables
                .iter()
                .flatten()
                .map(Arc::clone)
                .collect(),
        );
        new_info
    }
}

/// LessDBInfo is used for sorting DBInfo by DBInfo.Name.
// go: LessDBInfo L1
pub fn less_db_info(a: &DBInfo, b: &DBInfo) -> isize {
    match a.name.l.cmp(&b.name.l) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}
