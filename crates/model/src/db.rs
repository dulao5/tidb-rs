//! go-file: pkg/meta/model/db.go

use crate::{DBInfo, DBInfoDeprecated};

impl Clone for DBInfo {
    /// Clone clones DBInfo (the deprecated table list is cloned, never nil).
    // go: DBInfo.Clone L1
    fn clone(&self) -> DBInfo {
        let mut new_info = self.struct_copy();
        new_info.deprecated = DBInfoDeprecated {
            tables: Some(self.deprecated.tables.iter().flatten().cloned().collect()),
        };
        new_info
    }
}

impl DBInfo {
    /// Copy shallow copies DBInfo. Go shares the `*TableInfo` of the deprecated list; Rust
    /// copies them (no shared pointers).
    // go: DBInfo.Copy L2 (table infos copied instead of shared)
    pub fn copy(&self) -> DBInfo {
        let mut new_info = self.struct_copy();
        new_info.deprecated.tables =
            Some(self.deprecated.tables.iter().flatten().cloned().collect());
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
