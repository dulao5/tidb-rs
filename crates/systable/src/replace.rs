//! `REPLACE INTO mysql.<table> VALUES (...)` of one row of a system table, as the Go executor
//! writes it in an optimistic transaction (D05.2 spec §6.2 "写", block D06.7).
//!
//! Go runs the statement through the generic executor (`executor/replace.go`,
//! `insert_common.go`, `table/tables`), which is X. This is the specialized writer the spec
//! chose instead: it handles exactly the tables the SET GLOBAL path writes —
//! `mysql.global_variables` and `mysql.tidb`: a NONCLUSTERED VARCHAR primary key, string
//! columns, no other index, no partition, no generated column — and refuses any other
//! shape. Each step names the Go code it follows; the govec `systable` records (Go's real
//! REPLACE on a bootstrapped mockstore) check the resulting memory buffer byte for byte,
//! flags included.

use std::collections::HashMap;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::utc;
use tidb_kv::key::{Handle, IntHandle};
use tidb_model::{ColumnInfo, IndexInfo, StatePublic, TableInfo};
use tidb_parser::types::FieldType;
use tidb_tablecodec::rowcodec::{Checksum, Encoder};
use tidb_tablecodec::tablecodec as tc;
use tidb_types::datum::Datum;
use tidb_types::datum::new_collation_string_datum;

use crate::memdb::{Assertion, MemBuffer, Snapshot};

/// How the writing session encodes rows. Go's SET GLOBAL writes through an internal session
/// of `sysSessionPool`, which never loads the global `tidb_row_format_version`
/// (`RowEncoder.Enable` stays false: row format 1) and has row checksums off
/// (`IsRowLevelChecksumEnabled` is false under `InRestrictedSQL`): the default is that, so the
/// rows tidb-rs writes are byte-identical to Go's (checked on a cluster, spec §6.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowEncodingConfig {
    /// 1 (`tablecodec.EncodeOldRow`) or 2 (`rowcodec.Encoder`).
    pub row_format_version: i64,
    /// Row checksum (format 2 only).
    pub row_level_checksum: bool,
}

impl Default for RowEncodingConfig {
    // glue: the settings of Go's internal session (sysSessionPool)
    fn default() -> RowEncodingConfig {
        RowEncodingConfig {
            row_format_version: 1,
            row_level_checksum: false,
        }
    }
}

/// The table shape this writer supports; anything else is refused (spec §6.4: "表结构不是
/// 预期的 … 拒绝写入并报错，不猜测").
// glue: shape check of the specialized writer (spec §6.2 step 0)
pub fn check_table_shape(tbl: &TableInfo) -> Result<(), Error> {
    let refuse = |why: &str| {
        Err(errors::new(format!(
            "SET GLOBAL refused: mysql.{} has an unexpected shape ({why})",
            tbl.name.l
        )))
    };
    if tbl.pk_is_handle || tbl.is_common_handle {
        return refuse("clustered primary key");
    }
    if tbl.partition.is_some() {
        return refuse("partitioned");
    }
    if tbl.shard_row_id_bits != 0 {
        return refuse("SHARD_ROW_ID_BITS");
    }
    if tbl.auto_id_cache != 0 {
        return refuse("AUTO_ID_CACHE");
    }
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    if cols.is_empty() {
        return refuse("no column");
    }
    for c in cols {
        if c.state != StatePublic {
            return refuse("column not public");
        }
        if !c.generated_expr_string.is_empty() {
            return refuse("generated column");
        }
        if c.field_type.get_type() != tidb_base::mysql::TypeVarchar {
            return refuse("non-VARCHAR column");
        }
    }
    let idxs = tbl.indices.as_deref().unwrap_or(&[]);
    if idxs.len() != 1 {
        return refuse("not exactly one index");
    }
    let idx = &idxs[0];
    let icols = idx.columns.as_deref().unwrap_or(&[]);
    if !idx.primary
        || !idx.unique
        || idx.state != StatePublic
        || idx.global
        || icols.len() != 1
        || icols[0].offset != 0
        || icols[0].length != -1
    {
        return refuse("primary key is not the whole first column");
    }
    Ok(())
}

/// The string datums of a row of the table, as the INSERT path casts them to the column
/// types (VARCHAR with the column's collation). A value longer than the column (in
/// characters) is refused with Go's strict-mode error.
// glue: table.CastValue for the VARCHAR columns of the system tables (strict SQL mode)
fn row_datums(tbl: &TableInfo, row: &[Option<&str>]) -> Result<Vec<Datum>, Error> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    if row.len() != cols.len() {
        return Err(errors::new(format!(
            "REPLACE INTO mysql.{}: {} values for {} columns",
            tbl.name.l,
            row.len(),
            cols.len()
        )));
    }
    let mut out = Vec::with_capacity(row.len());
    for (c, v) in cols.iter().zip(row) {
        match v {
            None => out.push(Datum::default()),
            Some(s) => {
                let flen = c.field_type.get_flen();
                if flen >= 0 && s.chars().count() > flen as usize {
                    // Strict mode: the error of table.CastValue for the column, row 1.
                    return Err(
                        tidb_types::errors::ErrDataTooLong.gen_with_stack_by_args(vec![
                            Arg::from(c.name.o.clone()),
                            Arg::from(1i64),
                        ]),
                    );
                }
                out.push(new_collation_string_datum(
                    s.as_bytes().to_vec(),
                    c.field_type.get_collate().to_owned(),
                ));
            }
        }
    }
    Ok(out)
}

/// The values of the index columns of row (`index.FetchValues`).
// glue: index.FetchValues of a plain (non-expression) index
fn index_values(idx: &IndexInfo, row: &[Datum]) -> Vec<Datum> {
    idx.columns
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|ic| row[ic.offset as usize].clone())
        .collect()
}

/// The old row of handle h, decoded with the table's columns (`getOldRow`).
// glue: getOldRow (insert_common.go) without generated columns
fn old_row(
    tbl: &TableInfo,
    mb: &MemBuffer,
    snap: &dyn Snapshot,
    h: &Handle,
) -> Result<Vec<Datum>, Error> {
    let key = tc::encode_row_key_with_handle(tbl.id, h);
    let Some(v) = mb.get(snap, &key)? else {
        // go-part: InsertValues.removeRow (insert_common.go: old row not found)
        return Err(errors::new(format!(
            "can not be duplicated row, due to old row not found. handle {}",
            handle_string(h)
        )));
    };
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    // Go `colTps[col.ID] = &col.FieldType`: borrowed for this decode, not copied (#229).
    let types: HashMap<i64, &FieldType> = cols.iter().map(|c| (c.id, &c.field_type)).collect();
    let m = tc::decode_row_to_datum_map(&v, &types, Some(&utc()))?;
    Ok(cols
        .iter()
        .map(|c| m.get(&c.id).cloned().unwrap_or_default())
        .collect())
}

// glue: kv.IntHandle.String
fn handle_string(h: &Handle) -> String {
    h.int_value().to_string()
}

/// `equalDatumsAsBinary`: the rows are equal under the binary collation (strings compare as
/// bytes; NULL equals only NULL).
// glue: InsertValues.equalDatumsAsBinary for string columns
fn equal_as_binary(a: &[Datum], b: &[Datum]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| match (x.is_null(), y.is_null()) {
                (true, true) => true,
                (false, false) => x.get_bytes() == y.get_bytes(),
                _ => false,
            })
}

/// `TableCommon.RemoveRecord` of a nonclustered table without partitions: the record key
/// (asserted to exist) and each index entry of the old row (need-locked, asserted to exist).
// glue: TableCommon.RemoveRecord for the system tables (removeRowData, removeRowIndices, index.Delete)
fn remove_record(
    tbl: &TableInfo,
    mb: &mut MemBuffer,
    h: &Handle,
    old: &[Datum],
) -> Result<(), Error> {
    // go-part: TableCommon.removeRowData (tables.go)
    let key = tc::encode_row_key_with_handle(tbl.id, h);
    mb.set_assertion(&key, Assertion::Exist);
    mb.delete_with_flags(&key, false);
    // go-part: TableCommon.removeRowIndices (tables.go) + index.Delete (index.go), distinct key
    let loc = utc();
    for idx in tbl.indices.as_deref().unwrap_or(&[]) {
        let mut vals = index_values(idx, old);
        let (key, distinct) = tc::gen_index_key(&loc, tbl, idx, tbl.id, &mut vals, Some(h), None)?;
        if distinct {
            mb.delete_with_flags(&key, true);
        } else {
            mb.delete_with_flags(&key, false);
        }
        mb.set_assertion(&key, Assertion::Exist);
    }
    Ok(())
}

/// `TableCommon.addRecord` (nonclustered, `DupKeyCheckLazy`, optimistic) and `addIndices` /
/// `index.create`: the record and each index entry; a key not in the buffer is presumed not
/// to exist (checked by the prewrite), one the statement deleted is overwritten.
// glue: TableCommon.addRecord / addIndices / index.create for the system tables (DupKeyCheckLazy, optimistic)
fn add_record(
    tbl: &TableInfo,
    mb: &mut MemBuffer,
    row: &[Datum],
    h: &Handle,
    cfg: &RowEncodingConfig,
) -> Result<(), Error> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let mut ids = Vec::with_capacity(cols.len());
    let mut vals = Vec::with_capacity(cols.len());
    for c in cols {
        let v = &row[c.offset as usize];
        if can_skip(c, v) {
            continue;
        }
        ids.push(c.id);
        vals.push(v.clone());
    }
    let key = tc::encode_row_key_with_handle(tbl.id, h);
    // go-part: TableCommon.addRecord (tables.go: DupKeyCheckLazy, GetLocal)
    let presume = match mb.get_local(&key) {
        None => true,
        Some([]) => false,
        Some(_) => {
            return Err(errors::new(format!(
                "duplicate _tidb_rowid {} in mysql.{}",
                handle_string(h),
                tbl.name.l
            )));
        }
    };
    // go-part: EncodeRowBuffer.WriteMemBufferEncoded (tblctx/buffers.go) + tablecodec.EncodeRow
    let value = if cfg.row_format_version == 2 {
        let checksum = cfg
            .row_level_checksum
            .then(|| Checksum::Raw { handle: h.clone() });
        let mut enc = Encoder::with_enable(true);
        tc::encode_row(Some(&utc()), &vals, &ids, checksum.as_ref(), Some(&mut enc))?
    } else {
        tc::encode_row(Some(&utc()), &vals, &ids, None, None)?
    };
    mb.set_with_flags(&key, value, presume);
    mb.set_assertion(
        &key,
        if presume {
            Assertion::Unknown
        } else {
            Assertion::NotExist
        },
    );
    // go-part: TableCommon.addIndices (tables.go) + index.create (index.go), lazy check
    let loc = utc();
    for idx in tbl.indices.as_deref().unwrap_or(&[]) {
        let mut ivals = index_values(idx, row);
        let (key, distinct) =
            tc::gen_index_key(&loc, tbl, idx, tbl.id, &mut ivals.clone(), Some(h), None)?;
        tc::truncate_index_values(tbl, idx, &mut ivals);
        let need_restored = idx
            .columns
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|ic| tidb_model::column_need_restored_data(ic, cols));
        let idx_val = tc::gen_index_value_portal(
            Some(&loc),
            tbl,
            idx,
            need_restored,
            distinct,
            false,
            &ivals,
            h,
            tbl.id,
            &[],
            None,
        )?;
        if !distinct {
            mb.set_with_flags(&key, idx_val, false);
            mb.set_assertion(&key, Assertion::Unknown);
            continue;
        }
        match mb.get_local(&key) {
            None => {
                mb.set_with_flags(&key, idx_val, true);
                mb.set_assertion(&key, Assertion::Unknown);
            }
            Some([]) => {
                mb.set_with_flags(&key, idx_val, false);
                mb.set_assertion(&key, Assertion::NotExist);
            }
            Some(_) => {
                return Err(errors::new(format!(
                    "duplicate entry in mysql.{}.{}",
                    tbl.name.l, idx.name.l
                )));
            }
        }
    }
    Ok(())
}

/// `TableCommon.canSkip` / `CanSkip`: a NULL value of a column whose default is NULL is not
/// stored.
// glue: CanSkip for a nullable column without default
fn can_skip(c: &ColumnInfo, v: &Datum) -> bool {
    v.is_null() && c.default_value.is_nil() && c.origin_default_value.is_nil()
}

/// The REPLACE of one row (`ReplaceExec.replaceRow` without a handle column):
/// while the unique key finds a row, remove it unless it equals the new row (then nothing is
/// written); then add the row with a handle from `alloc_handle`. Returns the handle, `None`
/// when the row was unchanged.
// glue: REPLACE of one row of a system table (spec §6.2), following ReplaceExec.replaceRow / removeIndexRow and InsertValues.removeRow for an optimistic transaction
pub fn replace_row(
    tbl: &TableInfo,
    row: &[Option<&str>],
    snap: &dyn Snapshot,
    mb: &mut MemBuffer,
    alloc_handle: &mut dyn FnMut() -> Result<i64, Error>,
    cfg: &RowEncodingConfig,
) -> Result<Option<i64>, Error> {
    check_table_shape(tbl)?;
    if cfg.row_format_version != 1 && cfg.row_format_version != 2 {
        return Err(errors::new(format!(
            "unknown row format version {}",
            cfg.row_format_version
        )));
    }
    let new_row = row_datums(tbl, row)?;
    let loc = utc();
    let idxs = tbl.indices.as_deref().unwrap_or(&[]);
    loop {
        let mut found = None;
        // go-part: ReplaceExec.removeIndexRow (replace.go) + tables.FetchDuplicatedHandle
        for idx in idxs.iter().filter(|i| i.unique) {
            let mut vals = index_values(idx, &new_row);
            let (key, distinct) = tc::gen_index_key(&loc, tbl, idx, tbl.id, &mut vals, None, None)?;
            if !distinct {
                continue;
            }
            if let Some(v) = mb.get(snap, &key)? {
                found = tc::decode_handle_in_index_value(&v)?;
                if found.is_some() {
                    break;
                }
            }
        }
        let Some(h) = found else { break };
        // go-part: InsertValues.removeRow (insert_common.go)
        let old = old_row(tbl, mb, snap, &h)?;
        if equal_as_binary(&old, &new_row) {
            // Unchanged: an optimistic transaction locks nothing (addUnchangedKeysForLockByRow
            // returns early when the transaction is not pessimistic).
            return Ok(None);
        }
        remove_record(tbl, mb, &h, &old)?;
    }
    let h = Handle::Int(IntHandle(alloc_handle()?));
    add_record(tbl, mb, &new_row, &h, cfg)?;
    Ok(Some(h.int_value()))
}
