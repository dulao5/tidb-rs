//! go-file: pkg/planner/util/handle_cols.go
//!
//! Mapping:
//! - The `HandleCols` interface has two implementations and Go type-asserts to them
//!   (`GetCommonHandleDatum`), so it is an enum over the two structs (`M-IFACE`); the enum's
//!   methods only dispatch.
//! - `*expression.Column` is `Arc<Column>`; Go's `col.Clone()` shares the `Arc` (`M-EXPR`).
//!   `*model.TableInfo` / `*model.IndexInfo` are `Arc`s; `Clone` deep-copies them as Go does.
//! - `GetFieldsTypes` returns Go's `*FieldType` pointers into the columns; here copies
//!   (`M-FT`: callers only read them).
//! - `(T, error)` results whose `T` is a zero value on error are `Result<T, Error>`
//!   (`M-ERRVAL`).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::TypeLonglong;
use tidb_chunk::Row;
use tidb_codec::encode_key;
use tidb_expression::{Column, ParamValues, Schema};
use tidb_kv::Handle;
use tidb_kv::key::{IntHandle, PartitionHandle, new_common_handle, new_partition_handle};
use tidb_model::{IndexInfo, TableInfo};
use tidb_parser::types::FieldType;
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_tablecodec::truncate_index_values;
use tidb_types::collate::Collator;
use tidb_types::context::{Context as TypeContext, DefaultStmtNoWarningContext};
use tidb_types::datum::Datum;
use tidb_types::field_type::new_field_type;

/// `size.SizeOfPointer`.
const SIZE_OF_POINTER: i64 = 8;

/// HandleCols is the interface that holds handle columns.
#[derive(Clone, Debug)]
pub enum HandleCols {
    /// `*CommonHandleCols`.
    Common(CommonHandleCols),
    /// `*IntHandleCols`.
    Int(IntHandleCols),
}

impl HandleCols {
    /// `StringWithCtx` (expression.StringerWithCtx).
    // glue: M-IFACE (dispatch of HandleCols.StringWithCtx)
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        match self {
            HandleCols::Common(c) => c.string_with_ctx(ctx, redact),
            HandleCols::Int(c) => c.string_with_ctx(ctx, redact),
        }
    }

    /// BuildHandle builds a Handle from a row.
    // glue: M-IFACE (dispatch of HandleCols.BuildHandle)
    pub fn build_handle(&self, sc: &StatementContext, row: Row<'_>) -> Result<Handle, Error> {
        match self {
            HandleCols::Common(c) => c.build_handle(sc, row),
            HandleCols::Int(c) => c.build_handle(sc, row),
        }
    }

    /// BuildHandleByDatums builds a Handle from a datum slice.
    // glue: M-IFACE (dispatch of HandleCols.BuildHandleByDatums)
    pub fn build_handle_by_datums(
        &self,
        sc: &StatementContext,
        row: &[Datum],
    ) -> Result<Handle, Error> {
        match self {
            HandleCols::Common(c) => c.build_handle_by_datums(sc, row),
            HandleCols::Int(c) => c.build_handle_by_datums(sc, row),
        }
    }

    /// BuildHandleFromIndexRow builds a Handle from index row data.
    /// The last column(s) of `row` must be the handle column(s).
    // glue: M-IFACE (dispatch of HandleCols.BuildHandleFromIndexRow)
    pub fn build_handle_from_index_row(
        &self,
        sc: &StatementContext,
        row: Row<'_>,
    ) -> Result<Handle, Error> {
        match self {
            HandleCols::Common(c) => c.build_handle_from_index_row(sc, row),
            HandleCols::Int(c) => c.build_handle_from_index_row(sc, row),
        }
    }

    /// BuildHandleFromIndexRow builds a Handle from index row data.
    /// The last column of `row` must be the pids,
    /// and the second to last column(s) of `row` must be the handle column(s).
    // glue: M-IFACE (dispatch of HandleCols.BuildPartitionHandleFromIndexRow)
    pub fn build_partition_handle_from_index_row(
        &self,
        sc: &StatementContext,
        row: Row<'_>,
    ) -> Result<PartitionHandle, Error> {
        match self {
            HandleCols::Common(c) => c.build_partition_handle_from_index_row(sc, row),
            HandleCols::Int(c) => c.build_partition_handle_from_index_row(sc, row),
        }
    }

    /// ResolveIndices resolves handle column indices.
    // glue: M-IFACE (dispatch of HandleCols.ResolveIndices)
    pub fn resolve_indices(&self, schema: &Schema) -> Result<HandleCols, Error> {
        match self {
            HandleCols::Common(c) => c.resolve_indices(schema),
            HandleCols::Int(c) => c.resolve_indices(schema),
        }
    }

    /// IsInt returns if the HandleCols is a single int column.
    // glue: M-IFACE (dispatch of HandleCols.IsInt)
    pub fn is_int(&self) -> bool {
        match self {
            HandleCols::Common(c) => c.is_int(),
            HandleCols::Int(c) => c.is_int(),
        }
    }

    /// GetCol gets the column by idx.
    // glue: M-IFACE (dispatch of HandleCols.GetCol)
    pub fn get_col(&self, idx: isize) -> Option<&Arc<Column>> {
        match self {
            HandleCols::Common(c) => Some(c.get_col(idx)),
            HandleCols::Int(c) => c.get_col(idx),
        }
    }

    /// NumCols returns the number of columns.
    // glue: M-IFACE (dispatch of HandleCols.NumCols)
    pub fn num_cols(&self) -> isize {
        match self {
            HandleCols::Common(c) => c.num_cols(),
            HandleCols::Int(c) => c.num_cols(),
        }
    }

    /// Compare compares two datum rows by handle order.
    // glue: M-IFACE (dispatch of HandleCols.Compare)
    pub fn compare(
        &self,
        a: &[Datum],
        b: &[Datum],
        ctors: &[&'static dyn Collator],
        tp_ctx: &TypeContext,
    ) -> Result<isize, Error> {
        match self {
            HandleCols::Common(c) => c.compare(a, b, ctors, tp_ctx),
            HandleCols::Int(c) => c.compare(a, b, ctors, tp_ctx),
        }
    }

    /// GetFieldsTypes return field types of columns.
    // glue: M-IFACE (dispatch of HandleCols.GetFieldsTypes)
    pub fn get_fields_types(&self) -> Vec<FieldType> {
        match self {
            HandleCols::Common(c) => c.get_fields_types(),
            HandleCols::Int(c) => c.get_fields_types(),
        }
    }

    /// MemoryUsage return the memory usage
    // glue: M-IFACE (dispatch of HandleCols.MemoryUsage)
    pub fn memory_usage(&self) -> i64 {
        match self {
            HandleCols::Common(c) => c.memory_usage(),
            HandleCols::Int(c) => c.memory_usage(),
        }
    }

    /// Clone clones the HandleCols.
    // glue: M-IFACE (dispatch of HandleCols.Clone)
    pub fn clone_cols(&self) -> HandleCols {
        match self {
            HandleCols::Common(c) => c.clone_cols(),
            HandleCols::Int(c) => c.clone_cols(),
        }
    }
}

/// CommonHandleCols implements the kv.HandleCols interface.
/// Currently, HandleCols are data fields in some operators and executors, and will be used during execution.
/// So please avoid adding fields like sctx and stmtctx to avoid potential bugs when it's reused/shared by plan cache.
#[derive(Clone, Debug)]
pub struct CommonHandleCols {
    tbl_info: Arc<TableInfo>,
    idx_info: Arc<IndexInfo>,
    columns: Vec<Arc<Column>>,
}

impl CommonHandleCols {
    /// Clone implements the kv.HandleCols interface.
    // go: CommonHandleCols.Clone L1 [M-EXPR]
    pub fn clone_cols(&self) -> HandleCols {
        let mut new_cols = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            new_cols.push(Arc::clone(col));
        }
        HandleCols::Common(CommonHandleCols {
            tbl_info: Arc::new((*self.tbl_info).clone()),
            idx_info: Arc::new((*self.idx_info).clone()),
            columns: new_cols,
        })
    }

    /// GetColumns returns all the internal columns out.
    // go: CommonHandleCols.GetColumns L1
    pub fn get_columns(&self) -> &[Arc<Column>] {
        &self.columns
    }

    // go: CommonHandleCols.buildHandleByDatumsBuffer L1 [M-ERRVAL]
    fn build_handle_by_datums_buffer(
        &self,
        sc: &StatementContext,
        mut datum_buf: Vec<Datum>,
    ) -> Result<Handle, Error> {
        truncate_index_values(&self.tbl_info, &self.idx_info, &mut datum_buf);
        let mut handle_bytes = Vec::new();
        if let Err(err) = encode_key(&sc.time_zone(), &mut handle_bytes, &datum_buf) {
            sc.handle_error(err)?;
        }
        Ok(Handle::Common(Arc::new(new_common_handle(handle_bytes)?)))
    }

    /// BuildHandle implements the kv.HandleCols interface.
    // go: CommonHandleCols.BuildHandle L1
    pub fn build_handle(&self, sc: &StatementContext, row: Row<'_>) -> Result<Handle, Error> {
        let mut datum_buf = Vec::with_capacity(4);
        for col in &self.columns {
            datum_buf.push(row.get_datum(col.index as usize, &col.ret_type));
        }
        self.build_handle_by_datums_buffer(sc, datum_buf)
    }

    /// BuildHandleFromIndexRow implements the kv.HandleCols interface.
    // go: CommonHandleCols.BuildHandleFromIndexRow L1
    pub fn build_handle_from_index_row(
        &self,
        sc: &StatementContext,
        row: Row<'_>,
    ) -> Result<Handle, Error> {
        let mut datum_buf = Vec::with_capacity(4);
        let n = self.num_cols() as usize;
        for i in 0..n {
            datum_buf.push(row.get_datum(row.len() - n + i, &self.columns[i].ret_type));
        }
        self.build_handle_by_datums_buffer(sc, datum_buf)
    }

    /// BuildPartitionHandleFromIndexRow implements the kv.HandleCols interface.
    // go: CommonHandleCols.BuildPartitionHandleFromIndexRow L1
    pub fn build_partition_handle_from_index_row(
        &self,
        sc: &StatementContext,
        row: Row<'_>,
    ) -> Result<PartitionHandle, Error> {
        let mut datum_buf = Vec::with_capacity(4);
        let n = self.num_cols() as usize;
        for i in 0..n {
            datum_buf.push(row.get_datum(row.len() - 1 - n + i, &self.columns[i].ret_type));
        }
        let handle = self.build_handle_by_datums_buffer(sc, datum_buf)?;
        Ok(new_partition_handle(row.get_int64(row.len() - 1), handle))
    }

    /// BuildHandleByDatums implements the kv.HandleCols interface.
    // go: CommonHandleCols.BuildHandleByDatums L1
    pub fn build_handle_by_datums(
        &self,
        sc: &StatementContext,
        row: &[Datum],
    ) -> Result<Handle, Error> {
        let mut datum_buf = Vec::with_capacity(4);
        for col in &self.columns {
            datum_buf.push(row[col.index as usize].clone());
        }
        self.build_handle_by_datums_buffer(sc, datum_buf)
    }

    /// ResolveIndices implements the kv.HandleCols interface.
    // go: CommonHandleCols.ResolveIndices L1
    pub fn resolve_indices(&self, schema: &Schema) -> Result<HandleCols, Error> {
        let mut ncb = CommonHandleCols {
            tbl_info: Arc::clone(&self.tbl_info),
            idx_info: Arc::clone(&self.idx_info),
            columns: Vec::with_capacity(self.columns.len()),
        };
        for col in &self.columns {
            let new_col = col.resolve_indices(schema)?;
            ncb.columns.push(Arc::new(new_col));
        }
        Ok(HandleCols::Common(ncb))
    }

    /// IsInt implements the kv.HandleCols interface.
    // go: CommonHandleCols.IsInt L1
    pub fn is_int(&self) -> bool {
        false
    }

    /// GetCol implements the kv.HandleCols interface.
    // go: CommonHandleCols.GetCol L1
    pub fn get_col(&self, idx: isize) -> &Arc<Column> {
        &self.columns[idx as usize]
    }

    /// NumCols implements the kv.HandleCols interface.
    // go: CommonHandleCols.NumCols L1
    pub fn num_cols(&self) -> isize {
        self.columns.len() as isize
    }

    /// StringWithCtx implements the kv.HandleCols interface.
    // go: CommonHandleCols.StringWithCtx L1
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, _redact: &str) -> Vec<u8> {
        let mut b = vec![b'['];
        for (i, col) in self.columns.iter().enumerate() {
            if i != 0 {
                b.push(b',');
            }
            b.extend(col.column_explain_info(ctx, false));
        }
        b.push(b']');
        b
    }

    /// Compare implements the kv.HandleCols interface.
    // go: CommonHandleCols.Compare L1 [M-ERRVAL]
    pub fn compare(
        &self,
        a: &[Datum],
        b: &[Datum],
        ctors: &[&'static dyn Collator],
        tp_ctx: &TypeContext,
    ) -> Result<isize, Error> {
        for (i, col) in self.columns.iter().enumerate() {
            let a_datum = &a[col.index as usize];
            let b_datum = &b[col.index as usize];
            let (cmp, err) = a_datum.compare(tp_ctx, b_datum, ctors[i]);
            err?;
            if cmp != 0 {
                return Ok(cmp);
            }
        }
        Ok(0)
    }

    /// GetFieldsTypes implements the kv.HandleCols interface.
    // go: CommonHandleCols.GetFieldsTypes L1 [M-FT]
    pub fn get_fields_types(&self) -> Vec<FieldType> {
        let mut field_tps = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            field_tps.push(col.ret_type.clone());
        }
        field_tps
    }

    /// MemoryUsage return the memory usage of CommonHandleCols
    // go: CommonHandleCols.MemoryUsage L2 (unsafe.Sizeof(CommonHandleCols{}) is Go's size on 64-bit targets)
    pub fn memory_usage(&self) -> i64 {
        let mut sum = emptyCommonHandleColsSize + self.columns.capacity() as i64 * SIZE_OF_POINTER;
        for col in &self.columns {
            sum += col.memory_usage();
        }
        sum
    }
}

/// `unsafe.Sizeof(CommonHandleCols{})` of Go v8.5.8 on amd64 / arm64.
#[allow(non_upper_case_globals)]
const emptyCommonHandleColsSize: i64 = 40;

/// NewCommonHandleCols creates a new CommonHandleCols.
// go: NewCommonHandleCols L1
pub fn new_common_handle_cols(
    tbl_info: Arc<TableInfo>,
    idx_info: Arc<IndexInfo>,
    table_columns: &[Arc<Column>],
) -> CommonHandleCols {
    let idx_cols = idx_info.columns.as_deref().unwrap_or(&[]);
    let mut columns = Vec::with_capacity(idx_cols.len());
    for idx_col in idx_cols {
        columns.push(Arc::clone(&table_columns[idx_col.offset as usize]));
    }
    CommonHandleCols {
        tbl_info,
        idx_info,
        columns,
    }
}

/// NewCommonHandlesColsWithoutColsAlign creates a new CommonHandleCols without internal col align.
// go: NewCommonHandlesColsWithoutColsAlign L1
pub fn new_common_handles_cols_without_cols_align(
    tbl_info: Arc<TableInfo>,
    idx_info: Arc<IndexInfo>,
    cols: Vec<Arc<Column>>,
) -> CommonHandleCols {
    CommonHandleCols {
        tbl_info,
        idx_info,
        columns: cols,
    }
}

/// IntHandleCols implements the kv.HandleCols interface.
#[derive(Clone, Debug)]
pub struct IntHandleCols {
    col: Arc<Column>,
}

impl IntHandleCols {
    /// Clone implements the kv.HandleCols interface.
    // go: IntHandleCols.Clone L1 [M-EXPR]
    pub fn clone_cols(&self) -> HandleCols {
        HandleCols::Int(IntHandleCols {
            col: Arc::clone(&self.col),
        })
    }

    /// BuildHandle implements the kv.HandleCols interface.
    // go: IntHandleCols.BuildHandle L1
    pub fn build_handle(&self, _sc: &StatementContext, row: Row<'_>) -> Result<Handle, Error> {
        Ok(Handle::Int(IntHandle(
            row.get_int64(self.col.index as usize),
        )))
    }

    /// BuildHandleFromIndexRow implements the kv.HandleCols interface.
    // go: IntHandleCols.BuildHandleFromIndexRow L1
    pub fn build_handle_from_index_row(
        &self,
        _sc: &StatementContext,
        row: Row<'_>,
    ) -> Result<Handle, Error> {
        Ok(Handle::Int(IntHandle(row.get_int64(row.len() - 1))))
    }

    /// BuildPartitionHandleFromIndexRow implements the kv.HandleCols interface.
    // go: IntHandleCols.BuildPartitionHandleFromIndexRow L1
    pub fn build_partition_handle_from_index_row(
        &self,
        _sc: &StatementContext,
        row: Row<'_>,
    ) -> Result<PartitionHandle, Error> {
        Ok(new_partition_handle(
            row.get_int64(row.len() - 1),
            Handle::Int(IntHandle(row.get_int64(row.len() - 2))),
        ))
    }

    /// BuildHandleByDatums implements the kv.HandleCols interface.
    // go: IntHandleCols.BuildHandleByDatums L1
    pub fn build_handle_by_datums(
        &self,
        _sc: &StatementContext,
        row: &[Datum],
    ) -> Result<Handle, Error> {
        Ok(Handle::Int(IntHandle(
            row[self.col.index as usize].get_int64(),
        )))
    }

    /// ResolveIndices implements the kv.HandleCols interface.
    // go: IntHandleCols.ResolveIndices L1
    pub fn resolve_indices(&self, schema: &Schema) -> Result<HandleCols, Error> {
        let new_col = self.col.resolve_indices(schema)?;
        Ok(HandleCols::Int(IntHandleCols {
            col: Arc::new(new_col),
        }))
    }

    /// IsInt implements the kv.HandleCols interface.
    // go: IntHandleCols.IsInt L1
    pub fn is_int(&self) -> bool {
        true
    }

    /// StringWithCtx implements the kv.HandleCols interface.
    // go: IntHandleCols.StringWithCtx L1
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, _redact: &str) -> Vec<u8> {
        self.col.column_explain_info(ctx, false)
    }

    /// GetCol implements the kv.HandleCols interface.
    // go: IntHandleCols.GetCol L1
    pub fn get_col(&self, idx: isize) -> Option<&Arc<Column>> {
        if idx != 0 {
            return None;
        }
        Some(&self.col)
    }

    /// NumCols implements the kv.HandleCols interface.
    // go: IntHandleCols.NumCols L1
    pub fn num_cols(&self) -> isize {
        1
    }

    /// Compare implements the kv.HandleCols interface.
    ///
    /// Note: Go indexes the collators with the column's index in the row
    /// (`ctors[ib.col.Index]`), not 0; ported as is.
    // go: IntHandleCols.Compare L1 [M-ERRVAL]
    pub fn compare(
        &self,
        a: &[Datum],
        b: &[Datum],
        ctors: &[&'static dyn Collator],
        _tp_ctx: &TypeContext,
    ) -> Result<isize, Error> {
        let a_val = &a[self.col.index as usize];
        let b_val = &b[self.col.index as usize];
        let (cmp, err) = a_val.compare(
            &DefaultStmtNoWarningContext,
            b_val,
            ctors[self.col.index as usize],
        );
        err?;
        Ok(cmp)
    }

    /// GetFieldsTypes implements the kv.HandleCols interface.
    // go: IntHandleCols.GetFieldsTypes L1
    pub fn get_fields_types(&self) -> Vec<FieldType> {
        vec![new_field_type(TypeLonglong)]
    }

    /// MemoryUsage return the memory usage of IntHandleCols
    // go: IntHandleCols.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        self.col.memory_usage()
    }
}

/// NewIntHandleCols creates a new IntHandleCols.
// go: NewIntHandleCols L1
pub fn new_int_handle_cols(col: Arc<Column>) -> HandleCols {
    HandleCols::Int(IntHandleCols { col })
}

/// GetCommonHandleDatum gets the original data for the common handle.
///
/// Go's nil result for an int handle is `None`.
// go: GetCommonHandleDatum L1
pub fn get_common_handle_datum(cols: &HandleCols, row: Row<'_>) -> Option<Vec<Datum>> {
    if cols.is_int() {
        return None;
    }
    let HandleCols::Common(cb) = cols else {
        unreachable!("interface conversion: HandleCols is not *CommonHandleCols");
    };

    let mut datum_buf = Vec::with_capacity(4);
    for col in &cb.columns {
        datum_buf.push(row.get_datum(col.index as usize, &col.ret_type));
    }

    Some(datum_buf)
}
