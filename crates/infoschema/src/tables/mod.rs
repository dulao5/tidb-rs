//! go-file: pkg/table/tables/tables.go
//!
//! Package `tables` (block D06.4 part 1): `TableCommon`, the table built from a
//! `model.TableInfo`, and the read-path helpers (raw row decoding, column defaults, handle
//! columns, the table scan of a coprocessor request).
//!
//! Not here: the record mutations and their helpers (`AddRecord`, `UpdateRecord`,
//! `RemoveRecord`, handle allocation, duplicate-key errors, sequences) — permanent exemption
//! "write path"; and part 2: partitioned / cached / temporary tables (`newPartitionedTable`,
//! `newCachedTable`, `TempTableFromMeta` are boundary stubs below), `IterRecords`,
//! `BuildPartitionTableScanFromInfos`. Check constraints (`table.LoadCheckConstraint`) are
//! enforced only by writes: `TableFromMeta` does not load them (D06.4 spec §4).

pub mod index;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tidb_base::Error;
use tidb_kv::key::{Handle, Key};
use tidb_model::{
    ColumnInfo, IndexColumn, IndexInfo, StateDeleteOnly, StateDeleteReorganization, StateNone,
    StatePublic, TableCacheStatusDisable, TableInfo,
};
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_sessionctx::exprctx::BuildContext;
use tidb_types::datum::{Datum, new_int_datum};
use tidb_types::{collate, etc, stringutil};

use crate::autoid::Allocators;
use crate::table::column::{self, ColumnRef, new_clonable_expr_node, to_column};
use crate::table::{
    self, CachedTable, ErrColumnStateCantNone, ErrIndexStateCantNone, ErrTableStateCantNone,
    IndexRef, NormalTable, PartitionedTable, PhysicalTable, Table, TableRef, Type,
};
use crate::util::generatedexpr;

/// TableCommon is shared by both Table and partition.
/// NOTE: when copying this struct, use Copy() to clear its columns cache.
#[derive(Clone)]
pub struct TableCommon {
    // TODO: Why do we need tableID, when it is already in meta.ID ?
    table_id: i64,
    /// physicalTableID is a unique int64 to identify a physical table.
    physical_table_id: i64,
    pub columns: Vec<ColumnRef>,

    // column caches
    public_columns: Vec<ColumnRef>,
    visible_columns: Vec<ColumnRef>,
    hidden_columns: Vec<ColumnRef>,
    writable_columns: Vec<ColumnRef>,
    full_hidden_cols_and_visible_columns: Vec<ColumnRef>,

    indices: Vec<IndexRef>,
    meta: Arc<TableInfo>,
    allocs: Allocators,

    /// recordPrefix and indexPrefix are generated using physicalTableID.
    record_prefix: Key,
    index_prefix: Key,

    /// The heap size the schema cache charges (exception A3).
    mem_size: u64,
}

impl TableCommon {
    /// ResetColumnsCache implements testingKnob interface.
    ///
    /// The writable constraints are not built (check constraints are the write path).
    // go: TableCommon.ResetColumnsCache L2 (no writable constraints: write path)
    pub fn reset_columns_cache(&mut self) {
        self.public_columns = self.get_cols(GetColsMode::Full);
        self.visible_columns = self.get_cols(GetColsMode::Visible);
        self.hidden_columns = self.get_cols(GetColsMode::Hidden);

        self.writable_columns = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            if col.state == StateDeleteOnly || col.state == StateDeleteReorganization {
                continue;
            }
            self.writable_columns.push(col.clone());
        }

        self.full_hidden_cols_and_visible_columns = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            if col.hidden || col.state == StatePublic {
                self.full_hidden_cols_and_visible_columns.push(col.clone());
            }
        }
    }

    /// Copy copies a TableCommon struct, and reset its column cache. This is not a deep copy.
    // go: TableCommon.Copy L1
    pub fn copy(&self) -> TableCommon {
        self.clone()
    }

    // go: TableCommon.getCols L1
    fn get_cols(&self, mode: GetColsMode) -> Vec<ColumnRef> {
        let mut columns = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            if col.state != StatePublic {
                continue;
            }
            if (mode == GetColsMode::Visible && col.hidden)
                || (mode == GetColsMode::Hidden && !col.hidden)
            {
                continue;
            }
            columns.push(col.clone());
        }
        columns
    }

    /// RecordKey implements table.Table interface.
    // go: TableCommon.RecordKey L1
    pub fn record_key(&self, h: &Handle) -> Key {
        tidb_tablecodec::tablecodec::encode_record_key(&self.record_prefix.0, h)
    }

    // go: TableCommon.canSkip L1
    pub fn can_skip_col(&self, col: &column::Column, value: &Datum) -> bool {
        can_skip(&self.meta, col, value)
    }
}

/// getColsMode is the mode of `getCols`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GetColsMode {
    Visible,
    Hidden,
    Full,
}

impl Table for TableCommon {
    /// Cols implements table.Table Cols interface.
    // go: TableCommon.Cols L1
    fn cols(&self) -> &[ColumnRef] {
        &self.public_columns
    }

    /// VisibleCols implements table.Table VisibleCols interface.
    // go: TableCommon.VisibleCols L1
    fn visible_cols(&self) -> &[ColumnRef] {
        &self.visible_columns
    }

    /// HiddenCols implements table.Table HiddenCols interface.
    // go: TableCommon.HiddenCols L1
    fn hidden_cols(&self) -> &[ColumnRef] {
        &self.hidden_columns
    }

    /// WritableCols implements table WritableCols interface.
    // go: TableCommon.WritableCols L1
    fn writable_cols(&self) -> &[ColumnRef] {
        &self.writable_columns
    }

    /// DeletableCols implements table DeletableCols interface.
    // go: TableCommon.DeletableCols L1
    fn deletable_cols(&self) -> &[ColumnRef] {
        &self.columns
    }

    /// FullHiddenColsAndVisibleCols implements table FullHiddenColsAndVisibleCols interface.
    // go: TableCommon.FullHiddenColsAndVisibleCols L1
    fn full_hidden_cols_and_visible_cols(&self) -> &[ColumnRef] {
        &self.full_hidden_cols_and_visible_columns
    }

    /// Indices implements table.Table Indices interface.
    // go: TableCommon.Indices L1
    fn indices(&self) -> &[IndexRef] {
        &self.indices
    }

    /// DeletableIndices implements table.Table DeletableIndices interface.
    // go: TableCommon.DeletableIndices L1
    fn deletable_indices(&self) -> &[IndexRef] {
        // All indices are deletable because we don't need to check StateNone.
        &self.indices
    }

    /// RecordPrefix implements table.Table interface.
    // go: TableCommon.RecordPrefix L1
    fn record_prefix(&self) -> &Key {
        &self.record_prefix
    }

    /// IndexPrefix implements table.Table interface.
    // go: TableCommon.IndexPrefix L1
    fn index_prefix(&self) -> &Key {
        &self.index_prefix
    }

    /// Allocators implements table.Table Allocators interface. Without an
    /// `AllocatorContext` (the alternative allocators of temporary tables, part 2) it is
    /// `t.allocs`.
    // go: TableCommon.Allocators L2 (no AllocatorContext: temporary tables are part 2)
    fn allocators(&self) -> Allocators {
        self.allocs.clone()
    }

    /// Meta implements table.Table Meta interface.
    // go: TableCommon.Meta L1
    fn meta(&self) -> &Arc<TableInfo> {
        &self.meta
    }

    /// Type implements table.Table Type interface.
    // go: TableCommon.Type L1
    fn type_(&self) -> Type {
        NormalTable
    }

    /// GetPartitionedTable implements table.Table GetPhysicalID interface.
    // go: TableCommon.GetPartitionedTable L1
    fn get_partitioned_table(&self) -> Option<&dyn PartitionedTable> {
        None
    }

    // glue: type assertion t.(table.PhysicalTable) (M-IFACE)
    fn as_physical_table(&self) -> Option<&dyn PhysicalTable> {
        Some(self)
    }

    // glue: type assertion t.(table.CachedTable) (M-IFACE)
    fn as_cached_table(&self) -> Option<&dyn CachedTable> {
        None
    }

    // glue: A3
    fn mem_size(&self) -> u64 {
        self.mem_size
    }

    // glue: type assertion (M-IFACE)
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl PhysicalTable for TableCommon {
    /// GetPhysicalID implements table.Table GetPhysicalID interface.
    // go: TableCommon.GetPhysicalID L1
    fn get_physical_id(&self) -> i64 {
        self.physical_table_id
    }
}

/// MockTableFromMeta only serves for test.
///
/// Check constraints are not loaded (module docs); cached and partitioned tables are the
/// part-2 stubs of [`table_from_meta`].
// go: MockTableFromMeta L2 (check constraints are not loaded: write path)
pub fn mock_table_from_meta(tbl_info: Arc<TableInfo>) -> Option<TableRef> {
    let columns: Vec<ColumnRef> = tbl_info
        .columns
        .iter()
        .flatten()
        .map(|col_info| Arc::new(to_column(Arc::new(col_info.clone()))))
        .collect();

    let mut t = init_table_common(&tbl_info, tbl_info.id, columns, Allocators::default());
    if tbl_info.table_cache_status_type != TableCacheStatusDisable {
        return new_cached_table(t).ok();
    }
    if tbl_info.get_partition_info().is_none() {
        if init_table_indices(&mut t).is_err() {
            return None;
        }
        return Some(Arc::new(t));
    }

    new_partitioned_table(t, &tbl_info).ok()
}

/// TableFromMeta creates a Table instance from model.TableInfo.
///
/// Check constraints are not loaded (module docs). Cached and partitioned tables are the
/// part-2 stubs [`new_cached_table`] / [`new_partitioned_table`].
// go: TableFromMeta L2 (check constraints are not loaded: write path)
pub fn table_from_meta(allocs: Allocators, tbl_info: Arc<TableInfo>) -> Result<TableRef, Error> {
    if tbl_info.state == StateNone {
        return Err(ErrTableStateCantNone
            .gen_with_stack_by_args(vec![crate::infoschema_v2::cistr_arg(&tbl_info.name)]));
    }

    let cols_len = tbl_info.columns.as_ref().map_or(0, Vec::len);
    let mut columns: Vec<ColumnRef> = Vec::with_capacity(cols_len);
    for (i, col_info) in tbl_info.columns.iter().flatten().enumerate() {
        if col_info.state == StateNone {
            return Err(ErrColumnStateCantNone
                .gen_with_stack_by_args(vec![crate::infoschema_v2::cistr_arg(&col_info.name)]));
        }

        // Print some information when the column's offset isn't equal to i.
        if col_info.offset != i as isize {
            log::error!(
                "wrong table schema, table: {}, column: {}, index: {i}, offset: {}, columnNumber: {cols_len}",
                tbl_info.name.o,
                col_info.name.o,
                col_info.offset
            );
        }

        let mut col = to_column(Arc::new(col_info.clone()));
        if col.is_generated() {
            let gen_str = col_info.generated_expr_string.clone();
            // Built once to report its error now, as Go does; the node is rebuilt on use.
            build_generated_expr(&tbl_info, &gen_str)?;
            let ti = tbl_info.clone();
            col.generated_expr = Some(new_clonable_expr_node(Arc::new(move || {
                build_generated_expr(&ti, &gen_str).inspect_err(|err1| {
                    log::warn!(
                        "unexpected parse generated string error, generatedStr: {gen_str}, error: {err1}"
                    );
                })
            })));
        }
        // default value is expr.
        if col.default_is_expr {
            let s = match &col_info.default_value {
                tidb_model::AnyValue::Value(tidb_base::gostd::json::Value::String(s)) => s.clone(),
                // Go: colInfo.DefaultValue.(string) panics for any other dynamic type.
                _ => panic!("interface conversion: interface {{}} is not string"),
            };
            generatedexpr::parse_expression(&s)?;
            col.default_expr = Some(new_clonable_expr_node(Arc::new(move || {
                generatedexpr::parse_expression(&s)
            })));
        }
        columns.push(Arc::new(col));
    }

    let mut t = init_table_common(&tbl_info, tbl_info.id, columns, allocs);
    if tbl_info.get_partition_info().is_none() {
        init_table_indices(&mut t)?;
        if tbl_info.table_cache_status_type != TableCacheStatusDisable {
            return new_cached_table(t);
        }
        return Ok(Arc::new(t));
    }
    new_partitioned_table(t, &tbl_info)
}

// go: buildGeneratedExpr L1
fn build_generated_expr(
    tbl_info: &TableInfo,
    gen_expr: &str,
) -> Result<tidb_parser::ast::Expr, Error> {
    let expr = generatedexpr::parse_expression(gen_expr)?;
    let expr = generatedexpr::simple_resolve_name(expr, tbl_info)?;
    Ok(expr)
}

/// initTableCommon initializes a TableCommon struct.
///
/// Sequences (`t.sequence`) are the write path (`nextval`), and check constraints are not
/// loaded.
// go: initTableCommon L2 (no sequence cache, no constraints: write path)
fn init_table_common(
    tbl_info: &Arc<TableInfo>,
    physical_table_id: i64,
    cols: Vec<ColumnRef>,
    allocs: Allocators,
) -> TableCommon {
    let mut t = TableCommon {
        table_id: tbl_info.id,
        physical_table_id,
        allocs,
        meta: tbl_info.clone(),
        columns: cols,
        public_columns: Vec::new(),
        visible_columns: Vec::new(),
        hidden_columns: Vec::new(),
        writable_columns: Vec::new(),
        full_hidden_cols_and_visible_columns: Vec::new(),
        indices: Vec::new(),
        record_prefix: tidb_tablecodec::tablecodec::gen_table_record_prefix(physical_table_id),
        index_prefix: tidb_tablecodec::tablecodec::gen_table_index_prefix(physical_table_id),
        mem_size: table::table_info_mem_size(tbl_info),
    };
    let _ = t.table_id;
    t.reset_columns_cache();
    t
}

/// initTableIndices initializes the indices of the TableCommon.
// go: initTableIndices L1
fn init_table_indices(t: &mut TableCommon) -> Result<(), Error> {
    let tbl_info = t.meta.clone();
    for idx_info in tbl_info.indices.iter().flatten() {
        if idx_info.state == StateNone {
            return Err(ErrIndexStateCantNone
                .gen_with_stack_by_args(vec![crate::infoschema_v2::cistr_arg(&idx_info.name)]));
        }

        // Use partition ID for index, because TableCommon may be table or partition.
        let idx = index::new_index(t.physical_table_id, &tbl_info, Arc::new(idx_info.clone()))?;
        t.indices.push(idx);
    }
    Ok(())
}

// go: initTableCommonWithIndices L1
pub fn init_table_common_with_indices(
    tbl_info: &Arc<TableInfo>,
    physical_table_id: i64,
    cols: Vec<ColumnRef>,
    allocs: Allocators,
) -> Result<TableCommon, Error> {
    let mut t = init_table_common(tbl_info, physical_table_id, cols, allocs);
    init_table_indices(&mut t)?;
    Ok(t)
}

/// The table of a partitioned table (`partition.go` `newPartitionedTable`, part 2): its
/// checks of the partition info and the indices as Go; the partition expressions and the
/// partitions are not built (so an expression error is not reported), and the table answers
/// `GetPartitionedTable` with a [`PartitionedTable`] marker that has no methods yet, so a
/// caller cannot read it as a plain table.
// glue: stub, blocked-by:D06.4 (part 2: newPartitionedTable, partition.go)
fn new_partitioned_table(tbl: TableCommon, tbl_info: &TableInfo) -> Result<TableRef, Error> {
    match tbl_info.get_partition_info() {
        Some(pi) if !pi.definitions.as_ref().is_none_or(Vec::is_empty) => {}
        _ => return Err(table::ErrUnknownPartition.clone()),
    }
    let mut ret = tbl.copy();
    init_table_indices(&mut ret)?;
    Ok(Arc::new(PartitionedTableStub(ret)))
}

/// The table of a cached table (`cache.go` `newCachedTable`, part 2): a [`CachedTable`]
/// marker without the cache (`Init`, `TryReadFromCache`, ... come with part 2).
// glue: stub, blocked-by:D06.4 (part 2: newCachedTable, cache.go)
fn new_cached_table(tbl: TableCommon) -> Result<TableRef, Error> {
    Ok(Arc::new(CachedTableStub(tbl)))
}

/// See [`new_partitioned_table`].
// glue: stub, blocked-by:D06.4 (part 2: partitionedTable)
pub struct PartitionedTableStub(TableCommon);

/// See [`new_cached_table`].
// glue: stub, blocked-by:D06.4 (part 2: cachedTable)
pub struct CachedTableStub(TableCommon);

/// The `Table` methods of the part-2 stubs: Go's `partitionedTable` and `cachedTable` embed
/// `TableCommon`.
macro_rules! embed_table_common {
    ($t:ty) => {
        impl Table for $t {
            // glue: embedded TableCommon.Cols
            fn cols(&self) -> &[ColumnRef] {
                self.0.cols()
            }
            // glue: embedded TableCommon.VisibleCols
            fn visible_cols(&self) -> &[ColumnRef] {
                self.0.visible_cols()
            }
            // glue: embedded TableCommon.HiddenCols
            fn hidden_cols(&self) -> &[ColumnRef] {
                self.0.hidden_cols()
            }
            // glue: embedded TableCommon.WritableCols
            fn writable_cols(&self) -> &[ColumnRef] {
                self.0.writable_cols()
            }
            // glue: embedded TableCommon.DeletableCols
            fn deletable_cols(&self) -> &[ColumnRef] {
                self.0.deletable_cols()
            }
            // glue: embedded TableCommon.FullHiddenColsAndVisibleCols
            fn full_hidden_cols_and_visible_cols(&self) -> &[ColumnRef] {
                self.0.full_hidden_cols_and_visible_cols()
            }
            // glue: embedded TableCommon.Indices
            fn indices(&self) -> &[IndexRef] {
                self.0.indices()
            }
            // glue: embedded TableCommon.DeletableIndices
            fn deletable_indices(&self) -> &[IndexRef] {
                self.0.deletable_indices()
            }
            // glue: embedded TableCommon.RecordPrefix
            fn record_prefix(&self) -> &Key {
                self.0.record_prefix()
            }
            // glue: embedded TableCommon.IndexPrefix
            fn index_prefix(&self) -> &Key {
                self.0.index_prefix()
            }
            // glue: embedded TableCommon.Allocators
            fn allocators(&self) -> Allocators {
                self.0.allocators()
            }
            // glue: embedded TableCommon.Meta
            fn meta(&self) -> &Arc<TableInfo> {
                self.0.meta()
            }
            // glue: embedded TableCommon.Type
            fn type_(&self) -> Type {
                self.0.type_()
            }
            // glue: stub, blocked-by:D06.4 (part 2: partitionedTable.GetPartitionedTable)
            fn get_partitioned_table(&self) -> Option<&dyn PartitionedTable> {
                self.partitioned()
            }
            // glue: type assertion t.(table.PhysicalTable) (M-IFACE)
            fn as_physical_table(&self) -> Option<&dyn PhysicalTable> {
                Some(self)
            }
            // glue: type assertion t.(table.CachedTable) (M-IFACE)
            fn as_cached_table(&self) -> Option<&dyn CachedTable> {
                self.cached()
            }
            // glue: A3
            fn mem_size(&self) -> u64 {
                self.0.mem_size()
            }
            // glue: type assertion (M-IFACE)
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        impl PhysicalTable for $t {
            // glue: embedded TableCommon.GetPhysicalID
            fn get_physical_id(&self) -> i64 {
                self.0.get_physical_id()
            }
        }
    };
}

embed_table_common!(PartitionedTableStub);
embed_table_common!(CachedTableStub);

impl PartitionedTableStub {
    // glue: stub, blocked-by:D06.4 (part 2: partitionedTable.GetPartitionedTable)
    fn partitioned(&self) -> Option<&dyn PartitionedTable> {
        Some(self)
    }
    // glue: a partitioned table is not a CachedTable
    fn cached(&self) -> Option<&dyn CachedTable> {
        None
    }
}

impl CachedTableStub {
    // glue: cachedTable embeds TableCommon: GetPartitionedTable is nil
    fn partitioned(&self) -> Option<&dyn PartitionedTable> {
        None
    }
    // glue: stub, blocked-by:D06.4 (part 2: cachedTable)
    fn cached(&self) -> Option<&dyn CachedTable> {
        Some(self)
    }
}

impl PartitionedTable for PartitionedTableStub {}
impl CachedTable for CachedTableStub {}

/// FindPrimaryIndex uses to find primary index in tableInfo.
// go: FindPrimaryIndex L1
pub fn find_primary_index(tbl_info: &TableInfo) -> Option<&IndexInfo> {
    tbl_info.indices.iter().flatten().find(|idx| idx.primary)
}

/// The primary index of a table that has one (Go dereferences the nil `pkIdx` otherwise).
// glue: nil pointer dereference of FindPrimaryIndex's result
fn primary_index(tbl_info: &TableInfo) -> &IndexInfo {
    find_primary_index(tbl_info).expect("invalid memory address or nil pointer dereference")
}

/// The columns of a table (`tbl.Columns`).
// glue: range over a possibly nil Go slice
fn tbl_columns(tbl: &TableInfo) -> &[ColumnInfo] {
    tbl.columns.as_deref().unwrap_or(&[])
}

/// The columns of an index (`idx.Columns`).
// glue: range over a possibly nil Go slice
fn idx_columns(idx: &IndexInfo) -> &[IndexColumn] {
    idx.columns.as_deref().unwrap_or(&[])
}

/// TryGetCommonPkColumnIds get the IDs of primary key column if the table has common handle.
// go: TryGetCommonPkColumnIds L1
pub fn try_get_common_pk_column_ids(tbl: &TableInfo) -> Option<Vec<i64>> {
    if !tbl.is_common_handle {
        return None;
    }
    let pk_idx = primary_index(tbl);
    let mut pk_col_ids = Vec::with_capacity(idx_columns(pk_idx).len());
    for idx_col in idx_columns(pk_idx) {
        pk_col_ids.push(tbl_columns(tbl)[idx_col.offset as usize].id);
    }
    Some(pk_col_ids)
}

/// PrimaryPrefixColumnIDs get prefix column ids in primary key.
// go: PrimaryPrefixColumnIDs L1
pub fn primary_prefix_column_ids(tbl: &TableInfo) -> Option<Vec<i64>> {
    let mut prefix_cols: Option<Vec<i64>> = None;
    for idx in tbl.indices.iter().flatten() {
        if !idx.primary {
            continue;
        }
        for col in idx_columns(idx) {
            let tc = &tbl_columns(tbl)[col.offset as usize];
            if col.length > 0 && tc.field_type.get_flen() > col.length {
                prefix_cols.get_or_insert_with(Vec::new).push(tc.id);
            }
        }
    }
    prefix_cols
}

/// TryGetCommonPkColumns get the primary key columns if the table has common handle.
// go: TryGetCommonPkColumns L1
pub fn try_get_common_pk_columns(tbl: &dyn Table) -> Option<Vec<ColumnRef>> {
    if !tbl.meta().is_common_handle {
        return None;
    }
    let pk_idx = primary_index(tbl.meta());
    let cols = tbl.cols();
    let mut pk_cols = Vec::with_capacity(idx_columns(pk_idx).len());
    for idx_col in idx_columns(pk_idx) {
        pk_cols.push(cols[idx_col.offset as usize].clone());
    }
    Some(pk_cols)
}

// go: containFullColInHandle L1 [M-INT]
fn contain_full_col_in_handle(meta: &TableInfo, col: &column::Column) -> (bool, isize) {
    let pk_idx = primary_index(meta);
    for (i, idx_col) in idx_columns(pk_idx).iter().enumerate() {
        if tbl_columns(meta)[idx_col.offset as usize].id == col.id {
            return (idx_col.length == UnspecifiedLength, i as isize);
        }
    }
    (false, 0)
}

/// DecodeRawRowData decodes raw row data into a datum slice and a (columnID:columnValue) map.
///
/// `cols` may hold nil columns (`None`), as in Go. The row map is returned with the datums;
/// on error Go returns it with the error, which no caller reads.
// go: DecodeRawRowData L1 [M-ERRVAL]
pub fn decode_raw_row_data(
    ctx: &dyn BuildContext,
    meta: &TableInfo,
    h: &Handle,
    cols: &[Option<ColumnRef>],
    value: &[u8],
) -> Result<(Vec<Datum>, HashMap<i64, Datum>), Error> {
    let mut v: Vec<Datum> = vec![Datum::default(); cols.len()];
    let mut col_tps: HashMap<i64, Arc<FieldType>> = HashMap::with_capacity(cols.len());
    let mut prefix_cols: HashSet<i64> = HashSet::new();
    let loc = ctx.get_eval_ctx().location();
    for (i, col) in cols.iter().enumerate() {
        let Some(col) = col else { continue };
        if col.is_pk_handle_column(meta) {
            if tidb_base::mysql::has_unsigned_flag(col.field_type.get_flag()) {
                v[i].set_uint64(h.int_value() as u64);
            } else {
                v[i].set_int64(h.int_value());
            }
            continue;
        }
        if col.is_common_handle_column(meta) && !etc::need_restored_data(&col.field_type) {
            let (contain_full_col, idx_in_handle) = contain_full_col_in_handle(meta, col);
            if contain_full_col {
                let dt_bytes = h.encoded_col(idx_in_handle);
                let (_, dt, err) = tidb_codec::codec::decode_one(&dt_bytes);
                err?;
                let (dt, err) =
                    tidb_tablecodec::tablecodec::unflatten(dt, &col.field_type, Some(&loc));
                err?;
                v[i] = dt;
                continue;
            }
            prefix_cols.insert(col.id);
        }
        col_tps.insert(col.id, Arc::new(col.field_type.clone()));
    }
    let row_map =
        tidb_tablecodec::tablecodec::decode_row_to_datum_map(value, &col_tps, Some(&loc))?;
    let mut default_vals: Vec<Datum> = vec![Datum::default(); cols.len()];
    for (i, col) in cols.iter().enumerate() {
        let Some(col) = col else { continue };
        if (col.is_pk_handle_column(meta)
            || (col.is_common_handle_column(meta) && !etc::need_restored_data(&col.field_type)))
            && !prefix_cols.contains(&col.id)
        {
            continue;
        }
        if let Some(ri) = row_map.get(&col.id) {
            v[i] = ri.clone();
            continue;
        }
        if col.is_virtual_generated() {
            continue;
        }
        v[i] = if col.change_state_info.is_some() {
            get_changing_col_val(ctx, cols, col, &row_map, &mut default_vals)?.0
        } else {
            get_col_default_value(ctx, col, &mut default_vals)?
        };
    }
    Ok((v, row_map))
}

/// GetChangingColVal gets the changing column value when executing "modify/change column"
/// statement. For statement like update-where, it will fetch the old row out and insert it
/// into kv again. Since update statement can see the writable columns, it is responsible for
/// the casting relative column / get the fault value here.
/// old row : a-b-[nil]
/// new row : a-b-[a'/default]
/// Thus the writable new row is corresponding to Write-Only constraints.
///
/// Returns the value and whether it is the default value; Go returns the value with the
/// error too, which no caller reads.
// go: GetChangingColVal L1 [M-ERRVAL]
pub fn get_changing_col_val(
    ctx: &dyn BuildContext,
    cols: &[Option<ColumnRef>],
    col: &column::Column,
    row_map: &HashMap<i64, Datum>,
    default_vals: &mut [Datum],
) -> Result<(Datum, bool), Error> {
    let dep = col
        .change_state_info
        .as_ref()
        .expect("checked by the caller")
        .dependency_column_offset as usize;
    let relative_col = cols[dep]
        .as_ref()
        .expect("invalid memory address or nil pointer dereference");
    if let Some(idx_column_val) = row_map.get(&relative_col.id) {
        let (idx_column_val, err) =
            column::cast_column_value(ctx, idx_column_val.clone(), col.to_info(), false, false);
        // TODO: Consider sql_mode and the error msg(encounter this error check whether to rollback).
        err?;
        return Ok((idx_column_val, false));
    }

    let idx_column_val = get_col_default_value(ctx, col, default_vals)?;
    Ok((idx_column_val, true))
}

// go: tryDecodeColumnFromCommonHandle L1
pub fn try_decode_column_from_common_handle(
    col: &column::Column,
    handle: &Handle,
    pk_ids: &[i64],
    decode_loc: Option<&Arc<tidb_base::gostd::time::Location>>,
) -> Result<Datum, Error> {
    for (i, &hid) in pk_ids.iter().enumerate() {
        if hid != col.id {
            continue;
        }
        let (_, d, err) = tidb_codec::codec::decode_one(&handle.encoded_col(i as isize));
        err?;
        let (d, err) = tidb_tablecodec::tablecodec::unflatten(d, &col.field_type, decode_loc);
        err?;
        return Ok(d);
    }
    Ok(Datum::default())
}

/// GetColDefaultValue gets a column default value. The defaultVals is used to avoid
/// calculating the default value multiple times.
// go: GetColDefaultValue L1
pub fn get_col_default_value(
    ctx: &dyn BuildContext,
    col: &column::Column,
    default_vals: &mut [Datum],
) -> Result<Datum, Error> {
    let origin_is_nil = matches!(
        col.get_origin_default_value(),
        tidb_model::DefaultValueRef::Value(a) if a.is_nil()
    );
    if origin_is_nil && tidb_base::mysql::has_not_null_flag(col.field_type.get_flag()) {
        return Err(tidb_base::errors::new("Miss column"));
    }
    let off = col.offset as usize;
    if default_vals[off].is_null() {
        let col_val = column::get_col_origin_default_value(ctx, col.to_info())?;
        default_vals[off] = col_val.clone();
        Ok(col_val)
    } else {
        Ok(default_vals[off].clone())
    }
}

/// CanSkip is for these cases, we can skip the columns in encoded row:
/// 1. the column is included in primary key;
/// 2. the column's default value is null, and the value equals to that but has no origin default;
/// 3. the column is virtual generated.
// go: CanSkip L1
pub fn can_skip(info: &TableInfo, col: &column::Column, value: &Datum) -> bool {
    if col.is_pk_handle_column(info) {
        return true;
    }
    if col.is_common_handle_column(info) {
        let pk_idx = primary_index(info);
        for idx_col in idx_columns(pk_idx) {
            if tbl_columns(info)[idx_col.offset as usize].id != col.id {
                continue;
            }
            let mut can_skip = idx_col.length == UnspecifiedLength;
            can_skip = can_skip && !etc::need_restored_data(&col.field_type);
            return can_skip;
        }
    }
    let is_nil = |v: tidb_model::DefaultValueRef<'_>| matches!(v, tidb_model::DefaultValueRef::Value(a) if a.is_nil());
    if is_nil(col.get_default_value()) && value.is_null() && is_nil(col.get_origin_default_value())
    {
        return true;
    }
    if col.is_virtual_generated() {
        return true;
    }
    false
}

/// FindIndexByColName returns a public table index containing only one column named `name`.
// go: FindIndexByColName L1
pub fn find_index_by_col_name<'a>(t: &'a dyn Table, name: &str) -> Option<&'a IndexRef> {
    for idx in t.indices() {
        // only public index can be read.
        if idx.meta().state != StatePublic {
            continue;
        }

        let cols = idx_columns(idx.meta());
        if cols.len() == 1 && tidb_base::gostd::equal_fold(&cols[0].name.l, name) {
            return Some(idx);
        }
    }
    None
}

/// The package's init: Go assigns `table.TableFromMeta`, `table.MockTableFromMeta` and
/// `tableutil.TempTableFromMeta`; here [`table::table_from_meta`] and
/// [`table::mock_table_from_meta`] call this package directly, and temporary tables are part 2.
// go: init L2 (function variables become direct calls)
pub fn init() {}

/// TryGetHandleRestoredDataWrapper tries to get the restored data for handle if needed. The
/// argument can be a slice or a map.
// go: TryGetHandleRestoredDataWrapper L1
pub fn try_get_handle_restored_data_wrapper(
    tbl_info: &TableInfo,
    row: &[Datum],
    row_map: &HashMap<i64, Datum>,
    idx: &IndexInfo,
) -> Option<Vec<Datum>> {
    if !collate::new_collation_enabled()
        || !tbl_info.is_common_handle
        || tbl_info.common_handle_version == 0
    {
        return None;
    }
    let mut rs_data = Vec::with_capacity(4);
    let pk_idx = primary_index(tbl_info);
    for pk_idx_col in idx_columns(pk_idx) {
        let pk_col = &tbl_columns(tbl_info)[pk_idx_col.offset as usize];
        if !etc::need_restored_data(&pk_col.field_type) {
            continue;
        }
        let mut datum = if !row_map.is_empty() {
            row_map.get(&pk_col.id).cloned().unwrap_or_default()
        } else {
            row[pk_col.offset as usize].clone()
        };
        try_truncate_restored_data(&mut datum, pk_col, pk_idx_col, idx);
        convert_datum_to_tail_space_count(&mut datum, pk_col);
        rs_data.push(datum);
    }
    Some(rs_data)
}

/// TryTruncateRestoredData tries to truncate index values. Says that primary key(a (8)),
/// For index t(a), don't truncate the value. For index t(a(9)), truncate to a(9). For index
/// t(a(7)), truncate to a(8).
// go: TryTruncateRestoredData L1
pub fn try_truncate_restored_data(
    datum: &mut Datum,
    pk_col: &ColumnInfo,
    pk_idx_col: &IndexColumn,
    idx: &IndexInfo,
) {
    let mut truncate_target_col = pk_idx_col;
    for idx_col in idx_columns(idx) {
        if idx_col.offset == pk_idx_col.offset {
            truncate_target_col = max_index_len(pk_idx_col, idx_col);
            break;
        }
    }
    tidb_tablecodec::tablecodec::truncate_index_value(datum, truncate_target_col, pk_col);
}

/// ConvertDatumToTailSpaceCount converts a string datum to an int datum that represents the
/// tail space count.
// go: ConvertDatumToTailSpaceCount L1
pub fn convert_datum_to_tail_space_count(datum: &mut Datum, col: &ColumnInfo) {
    if collate::is_bin_collation(col.field_type.get_collate()) {
        *datum = new_int_datum(stringutil::get_tail_space_count(datum.get_string()));
    }
}

// go: maxIndexLen L1
fn max_index_len<'a>(idx_a: &'a IndexColumn, idx_b: &'a IndexColumn) -> &'a IndexColumn {
    if idx_a.length == UnspecifiedLength {
        return idx_a;
    }
    if idx_b.length == UnspecifiedLength {
        return idx_b;
    }
    if idx_a.length > idx_b.length {
        return idx_a;
    }
    idx_b
}

/// BuildTableScanFromInfos build tipb.TableScan with *model.TableInfo and *model.ColumnInfo.
// go: BuildTableScanFromInfos L1
pub fn build_table_scan_from_infos(
    table_info: &TableInfo,
    column_infos: &[&ColumnInfo],
    is_tiflash_store: bool,
) -> tipb::TableScan {
    let pk_col_ids = try_get_common_pk_column_ids(table_info);
    // The gogoproto non-nullable fields Go always marshals (`desc`, `next_read_engine`,
    // `max_wait_time_ms`), as `expr_to_pb.rs` does for `tipb.Expr`.
    let mut ts_exec = tipb::TableScan::default();
    ts_exec.set_desc(false);
    ts_exec.set_next_read_engine(tipb::EngineType::Local);
    ts_exec.set_max_wait_time_ms(0);
    ts_exec.set_table_id(table_info.id);
    ts_exec.set_columns(
        crate::util::misc::columns_to_proto(
            column_infos,
            table_info.pk_is_handle,
            false,
            is_tiflash_store,
        )
        .into(),
    );
    ts_exec.set_primary_column_ids(pk_col_ids.unwrap_or_default());
    if table_info.is_common_handle {
        ts_exec.set_primary_prefix_column_ids(
            primary_prefix_column_ids(table_info).unwrap_or_default(),
        );
    }
    ts_exec
}

/// SetPBColumnsDefaultValue sets the default values of tipb.ColumnInfo.
// go: SetPBColumnsDefaultValue L1
pub fn set_pb_columns_default_value(
    ctx: &dyn BuildContext,
    pb_columns: &mut [tipb::ColumnInfo],
    columns: &[&ColumnInfo],
) -> Result<(), Error> {
    for (i, c) in columns.iter().enumerate() {
        // For virtual columns, we set their default values to NULL so that TiKV will return
        // NULL properly, They real values will be computed later.
        if c.is_generated() && !c.generated_stored {
            pb_columns[i].set_default_val(vec![tidb_codec::codec::NilFlag]);
        }
        if matches!(
            c.get_origin_default_value(),
            tidb_model::DefaultValueRef::Value(a) if a.is_nil()
        ) {
            continue;
        }

        let eval_ctx = ctx.get_eval_ctx();
        let d = column::get_col_origin_default_value_without_strict_sql_mode(ctx, c)?;

        let (val, err) = tidb_tablecodec::tablecodec::encode_value_datum(
            Some(&eval_ctx.location()),
            Vec::new(),
            &d,
        );
        pb_columns[i].set_default_val(val);
        let ec = eval_ctx.err_ctx();
        if let Err(err) = err {
            ec.handle_error(err)?;
        }
    }
    Ok(())
}

/// TempTableFromMeta builds a TempTable from model.TableInfo.
// glue: stub, blocked-by:D06.4 (part 2: TempTableFromMeta, temporary tables)
pub fn temp_table_from_meta(_tbl_info: &TableInfo) -> Result<(), Error> {
    Err(crate::blocked("D06.4", "temporary tables"))
}
