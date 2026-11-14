//! go-file: pkg/util/misc.go
//!
//! `ColumnsToProto` / `ColumnToProto` (`pkg/util/misc.go`, D00.3 leaf utility code; the
//! table scan of `tables.BuildTableScanFromInfos` is their first user, constitution §4.4).
//! The rest of the file (recovery helpers, TLS, process info, `SyntaxError`) has other
//! users and comes with them.

use tidb_base::mysql::{self, GeneratedColumnFlag, has_pri_key_flag};
use tidb_model::{ColumnInfo, ExtraHandleID};
use tidb_types::collate::rewrite_new_collation_id_if_needed;

/// ColumnsToProto converts a slice of model.ColumnInfo to a slice of tipb.ColumnInfo.
// go: ColumnsToProto L1
pub fn columns_to_proto(
    columns: &[&ColumnInfo],
    pk_is_handle: bool,
    for_index: bool,
    is_tiflash_store: bool,
) -> Vec<tipb::ColumnInfo> {
    let mut cols = Vec::with_capacity(columns.len());
    for c in columns {
        let mut col = column_to_proto(c, for_index, is_tiflash_store);
        // TODO: Here `PkHandle`'s meaning is changed, we will change it to `IsHandle` when
        // tikv's old select logic is abandoned.
        if (pk_is_handle && has_pri_key_flag(c.field_type.get_flag()))
            || c.id == ExtraHandleID as i64
        {
            col.set_pk_handle(true);
        } else {
            col.set_pk_handle(false);
        }
        cols.push(col);
    }
    cols
}

/// ColumnToProto converts model.ColumnInfo to tipb.ColumnInfo.
// go: ColumnToProto L1 [M-INT]
pub fn column_to_proto(
    c: &ColumnInfo,
    for_index: bool,
    is_tiflash_store: bool,
) -> tipb::ColumnInfo {
    let ft = &c.field_type;
    let mut pc = column_info_literal();
    pc.set_column_id(c.id);
    pc.set_collation(rewrite_new_collation_id_if_needed(
        mysql::CollationNames
            .get(ft.get_collate())
            .copied()
            .unwrap_or(0) as i32,
    ));
    pc.set_column_len(ft.get_flen() as i32);
    pc.set_decimal(ft.get_decimal() as i32);
    pc.set_flag(ft.get_flag() as i32);
    pc.set_elems(
        ft.get_elems()
            .unwrap_or_default()
            .iter()
            .map(|e| String::from_utf8_lossy(e).into_owned())
            .collect::<Vec<_>>()
            .into(),
    );
    if is_tiflash_store && c.is_virtual_generated() {
        pc.set_flag(pc.get_flag() | GeneratedColumnFlag as i32);
    }
    if for_index {
        // Use array type for read the multi-valued index.
        pc.set_tp(i32::from(ft.array_type().get_type()));
        if ft.is_array() {
            // Use "binary" collation for read the multi-valued index. Most of the time, the
            // `Collation` of this hidden column should already been set to "binary".
            // However, in old versions, the collation is set to the default value. See
            // https://github.com/pingcap/tidb/issues/46717
            pc.set_collation(mysql::CollationNames.get("binary").copied().unwrap_or(0) as i32);
        }
    } else {
        pc.set_tp(i32::from(ft.get_type()));
    }
    pc
}

/// A `tipb.ColumnInfo` literal: the gogoproto non-nullable fields (`column_id`, `tp`,
/// `collation`, `columnLen`, `decimal`, `flag`, `pk_handle`, `array`) are always marshaled
/// by Go, so they are present here (as `expr_to_pb.rs` does for `tipb.Expr`).
// glue: Go's &tipb.ColumnInfo{...} literal (gogoproto non-nullable fields)
fn column_info_literal() -> tipb::ColumnInfo {
    let mut pc = tipb::ColumnInfo::default();
    pc.set_column_id(0);
    pc.set_tp(0);
    pc.set_collation(0);
    pc.set_column_len(0);
    pc.set_decimal(0);
    pc.set_flag(0);
    pc.set_pk_handle(false);
    pc.set_array(false);
    pc
}
