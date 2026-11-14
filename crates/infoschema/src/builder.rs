//! go-file: pkg/infoschema/builder.go
//!
//! The InfoSchema builder. This PR has the functions infoschema v2's lazy table loading
//! shares with the builder (`tableFromMeta`, the two charset conversions,
//! `tableIDIsValid`); the builder itself (full load, `ApplyDiff`) comes in the next PR of
//! part 1. Only the v2 paths are ported (spec §1.2).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::to_lower;
use tidb_model::{ColumnInfoVersion2, TableInfo, TableInfoVersion2, TableInfoVersion3};
use tidb_parser::gocharset::{CharsetUTF8, CharsetUTF8MB4, CollationUTF8MB4};

use crate::autoid::Allocators;
use crate::table::{self, TableRef};

/// The session factory of cached tables (Go `func() (pools.Resource, error)`).
pub type Factory = Arc<dyn Fn() -> Result<Box<dyn std::any::Any + Send>, Error> + Send + Sync>;

/// ConvertCharsetCollateToLowerCaseIfNeed convert the charset / collation of table and its
/// columns to lower case, if the table's version is prior to TableInfoVersion3.
// go: ConvertCharsetCollateToLowerCaseIfNeed L1
pub fn convert_charset_collate_to_lower_case_if_need(tb_info: &mut TableInfo) {
    if tb_info.version >= TableInfoVersion3 {
        return;
    }
    tb_info.charset = to_lower(&tb_info.charset);
    tb_info.collate = to_lower(&tb_info.collate);
    for col in tb_info.columns.iter_mut().flatten() {
        let cs = to_lower(col.get_charset());
        col.set_charset(&cs);
        let co = to_lower(col.get_collate());
        col.set_collate(&co);
    }
}

/// ConvertOldVersionUTF8ToUTF8MB4IfNeed convert old version UTF8 to UTF8MB4 if
/// config.TreatOldVersionUTF8AsUTF8MB4 is enable.
// go: ConvertOldVersionUTF8ToUTF8MB4IfNeed L1
pub fn convert_old_version_utf8_to_utf8mb4_if_need(tb_info: &mut TableInfo) {
    if tb_info.version >= TableInfoVersion2
        || !tidb_base::config::get_global_config().treat_old_version_utf8_as_utf8mb4
    {
        return;
    }
    if tb_info.charset == CharsetUTF8 {
        tb_info.charset = CharsetUTF8MB4.to_owned();
        tb_info.collate = CollationUTF8MB4.to_owned();
    }
    for col in tb_info.columns.iter_mut().flatten() {
        if col.version < ColumnInfoVersion2 && col.get_charset() == CharsetUTF8 {
            col.set_charset(CharsetUTF8MB4);
            col.set_collate(CollationUTF8MB4);
        }
    }
}

/// The CachedTable branch of Go (`t.Init(factory session's SQLExecutor)`) needs the cached
/// table of D06.4; the stand-in table is never a CachedTable.
// go: tableFromMeta L1
pub(crate) fn table_from_meta(
    alloc: Allocators,
    _factory: Option<&Factory>,
    tbl_info: Arc<TableInfo>,
) -> Result<TableRef, Error> {
    let ret = table::table_from_meta(alloc, tbl_info)?;
    Ok(ret)
}

/// The function that builds a table of a schema (Go `tableFromMetaFunc`).
pub type TableFromMetaFunc =
    fn(Allocators, Option<&Factory>, Arc<TableInfo>) -> Result<TableRef, Error>;

// go: tableIDIsValid L1
pub(crate) fn table_id_is_valid(table_id: i64) -> bool {
    table_id > 0
}
