//! go-file: pkg/expression/column.go
//!
//! `Column` and `CorrelatedColumn` (spec §3.1–3.2).
//!
//! - The cached `hashcode` is a `OnceLock` (N4 §3.5); `Clone` copies it.
//! - `CorrelatedColumn.Data *types.Datum` is a slot keyed by the outer column's `UniqueID`
//!   at creation: the value lives in the [`EvalState`]; clones and `RemapColumn` keep the
//!   slot, as Go keeps the pointer (spec §3.2).
//! - `VecEvalInt`/`VecEvalReal`/`VecEvalString` are ported for the cases that convert
//!   (hybrid columns, FLOAT columns); the RPN copies the other columns itself (D08.8 part 2).
//!   The other `VecEval*` methods only copy and are the RPN's column copy (M-EVAL). Not
//!   ported (spec §4): the cascades-only `Hash64`/`Equals` (EXC.B). `EqualByExprAndID` and
//!   `resolveIndicesByVirtualExpr` need `ScalarFunction.Equal` and come with part 2.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tidb_base::errors::RedactLogDisable;
use tidb_base::{Error, mysql};
use tidb_chunk::Row;
use tidb_model::ColumnInfo;
use tidb_parser::gocharset::CharsetASCII;
use tidb_parser::types::{ETJson, ETString};
use tidb_types::datum::{Datum, KindMysqlBit};
use tidb_types::etc::FieldType;
use tidb_types::json::BinaryJSON;
use tidb_types::mydecimal::MyDecimal;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::VectorFloat32;

use crate::collation::{
    ASCII, Coercibility, CollationInfo, Repertoire, UNICODE, derive_coercibility_for_column,
};
use crate::context::{EvalContext, ParamValues, type_ctx};
use crate::expression::{
    ConstLevel, ConstNone, EvalState, Expr, ExprRef, TraverseAction, columnFlag,
};
use crate::scalar_function::ast_func;
use crate::schema::Schema;
use crate::size;

/// The slot of a correlated column's value (Go's `Data *types.Datum`, spec §3.2): the
/// `UniqueID` of the outer column the correlated column was created for.
///
/// Go's `ExtractCorColumnsBySchema` points the `Data` of every correlated column of the
/// same outer column at one Datum, which Apply writes once per outer row; keying the
/// state by that `UniqueID` gives the same sharing without a pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CorrSlot(pub i64);

/// CorrelatedColumn stands for a column in a correlated sub query.
#[derive(Debug)]
pub struct CorrelatedColumn {
    /// The embedded `Column`.
    pub column: Column,
    /// `Data`: the slot of the value in the `EvalState`.
    pub slot: CorrSlot,
}

impl Clone for CorrelatedColumn {
    /// Clone implements Expression interface.
    // go: CorrelatedColumn.Clone L1
    fn clone(&self) -> Self {
        CorrelatedColumn {
            column: self.column.clone(),
            slot: self.slot,
        }
    }
}

impl CorrelatedColumn {
    /// `&CorrelatedColumn{Column: col, Data: new(types.Datum)}`.
    // glue: Go composite literal with a fresh Data pointer
    pub fn new(column: Column) -> CorrelatedColumn {
        CorrelatedColumn {
            slot: CorrSlot(column.unique_id),
            column,
        }
    }

    /// Traverse implements the TraverseDown interface.
    // go: CorrelatedColumn.Traverse L1
    pub fn traverse(&self, this: &ExprRef, action: &mut dyn TraverseAction) -> ExprRef {
        action.transform(this)
    }

    /// Eval implements Expression interface.
    // go: CorrelatedColumn.Eval L1
    pub fn eval(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Datum, Error> {
        Ok(st.correlated(self).clone())
    }

    /// EvalInt returns int representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalInt L1
    pub fn eval_int(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<i64>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        if self.column.get_type().hybrid() {
            let (res, r) = data.to_int64(&type_ctx(ctx));
            r?;
            return Ok(Some(res));
        }
        Ok(Some(data.get_int64()))
    }

    /// EvalReal returns real representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalReal L1
    pub fn eval_real(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<f64>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(data.get_float64()))
    }

    /// EvalString returns string representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalString L1
    pub fn eval_string(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<Vec<u8>>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        data.to_string().map(Some)
    }

    /// EvalDecimal returns decimal representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalDecimal L1
    pub fn eval_decimal(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<MyDecimal>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(*data.get_mysql_decimal()))
    }

    /// EvalTime returns DATE/DATETIME/TIMESTAMP representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalTime L1
    pub fn eval_time(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<Time>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(data.get_mysql_time()))
    }

    /// EvalDuration returns Duration representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalDuration L1
    pub fn eval_duration(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<Duration>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(data.get_mysql_duration()))
    }

    /// EvalJSON returns JSON representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalJSON L1
    pub fn eval_json(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<BinaryJSON<'static>>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(data.get_mysql_json().into_owned()))
    }

    /// EvalVectorFloat32 returns VectorFloat32 representation of CorrelatedColumn.
    // go: CorrelatedColumn.EvalVectorFloat32 L1
    pub fn eval_vector_float32(
        &self,
        _ctx: &dyn EvalContext,
        st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<VectorFloat32<'static>>, Error> {
        let data = st.correlated(self);
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(data.get_vector_float32().clone()))
    }

    /// Equal implements Expression interface.
    // go: CorrelatedColumn.Equal L1
    pub fn equal(&self, expr: &Expr) -> bool {
        self.equal_column(expr)
    }

    /// EqualColumn returns whether two column is equal
    // go: CorrelatedColumn.EqualColumn L1
    pub fn equal_column(&self, expr: &Expr) -> bool {
        if let Expr::CorrelatedColumn(cc) = expr {
            return self.column.unique_id == cc.column.unique_id;
        }
        false
    }

    /// IsCorrelated implements Expression interface.
    // go: CorrelatedColumn.IsCorrelated L1
    pub fn is_correlated(&self) -> bool {
        true
    }

    /// ConstLevel returns the const level for the expression
    // go: CorrelatedColumn.ConstLevel L1
    pub fn const_level(&self) -> ConstLevel {
        ConstNone
    }

    /// Decorrelate implements Expression interface.
    // go: CorrelatedColumn.Decorrelate L1
    pub fn decorrelate(&self, this: &ExprRef, schema: &Schema) -> ExprRef {
        if !schema.contains(&self.column) {
            return Arc::clone(this);
        }
        Expr::Column(self.column.clone()).into_ref()
    }

    /// ResolveIndices implements Expression interface.
    // go: CorrelatedColumn.ResolveIndices L1
    pub fn resolve_indices(&self, this: &ExprRef, _schema: &Schema) -> Result<ExprRef, Error> {
        Ok(Arc::clone(this))
    }

    // go: CorrelatedColumn.resolveIndices L1
    pub fn resolve_indices_in_place(&mut self, _schema: &Schema) -> Result<(), Error> {
        Ok(())
    }

    /// ResolveIndicesByVirtualExpr implements Expression interface.
    // go: CorrelatedColumn.ResolveIndicesByVirtualExpr L1
    pub fn resolve_indices_by_virtual_expr(
        &self,
        this: &ExprRef,
        _ctx: &dyn EvalContext,
        _schema: &Schema,
    ) -> (ExprRef, bool) {
        (Arc::clone(this), true)
    }

    // go: CorrelatedColumn.resolveIndicesByVirtualExpr L1
    pub fn resolve_indices_by_virtual_expr_in_place(
        &mut self,
        _ctx: &dyn EvalContext,
        _schema: &Schema,
    ) -> bool {
        true
    }

    /// MemoryUsage return the memory usage of CorrelatedColumn
    // go: CorrelatedColumn.MemoryUsage L2 (Data always points to a Datum in Go: Apply allocates it with the column; its size is counted as the zero Datum's)
    pub fn memory_usage(&self) -> i64 {
        self.column.memory_usage() + size::SizeOfPointer + Datum::default().mem_usage()
    }

    /// RemapColumn remaps columns with provided mapping and returns new expression
    // go: CorrelatedColumn.RemapColumn L1
    pub fn remap_column(&self, m: &HashMap<i64, Arc<Column>>) -> Result<ExprRef, Error> {
        let Some(mapped) = m.get(&self.column.unique_id) else {
            return Err(Error::plain(format!(
                "Can't remap column for {}",
                String::from_utf8_lossy(&self.column.to_string())
            )));
        };
        Ok(Expr::CorrelatedColumn(CorrelatedColumn {
            column: (**mapped).clone(),
            slot: self.slot,
        })
        .into_ref())
    }
}

/// Column represents a column.
#[derive(Debug)]
pub struct Column {
    /// RetType. Go's `*FieldType`, shared by the clones of the column (`Column.Clone` copies
    /// the pointer); changed only through [`Column::ret_type_mut`] (copy on write, N13 §8).
    pub ret_type: Arc<FieldType>,
    /// ID is used to specify whether this column is ExtraHandleColumn or to access histogram.
    /// We'll try to remove it in the future.
    pub id: i64,
    /// UniqueID is the unique id of this column.
    pub unique_id: i64,

    /// Index is used for execution, to tell the column's position in the given row.
    pub index: isize,

    /// The cached `HashCode` (Go's unexported `hashcode`); public so that struct-update
    /// syntax works outside the crate. Only `hash_code`/`clean_hash_code` touch it.
    pub hashcode: OnceLock<Vec<u8>>,

    /// VirtualExpr is used to save expression for virtual column
    pub virtual_expr: Option<ExprRef>,

    /// OrigName.
    pub orig_name: String,
    /// IsHidden.
    pub is_hidden: bool,

    /// IsPrefix indicates whether this column is a prefix column in index.
    pub is_prefix: bool,

    /// InOperand indicates whether this column is the inner operand of column equal condition
    /// converted from `[not] in (subq)`.
    pub in_operand: bool,

    /// The embedded `collationInfo`.
    pub collation_info: CollationInfo,

    /// CorrelatedColUniqueID.
    pub correlated_col_unique_id: i64,
}

impl Default for Column {
    // glue: Go zero value of Column (RetType nil is a default FieldType here, spec §4)
    fn default() -> Self {
        Column {
            ret_type: default_ret_type(),
            id: 0,
            unique_id: 0,
            index: 0,
            hashcode: OnceLock::new(),
            virtual_expr: None,
            orig_name: String::new(),
            is_hidden: false,
            is_prefix: false,
            in_operand: false,
            collation_info: CollationInfo::default(),
            correlated_col_unique_id: 0,
        }
    }
}

/// The `ret_type` of `Column::default()` (Go's nil `RetType` is a default `FieldType`, spec
/// §4): one shared value, so a default column allocates no type.
// glue: shared default FieldType of Column::default (N13 §8)
pub(crate) fn default_ret_type() -> Arc<FieldType> {
    static DEFAULT: std::sync::LazyLock<Arc<FieldType>> =
        std::sync::LazyLock::new(|| Arc::new(FieldType::default()));
    Arc::clone(&DEFAULT)
}

#[allow(non_upper_case_globals)]
const columnPrefix: &str = "Column#";

impl Clone for Column {
    /// Clone implements Expression interface.
    // go: Column.Clone L1
    fn clone(&self) -> Self {
        Column {
            ret_type: Arc::clone(&self.ret_type),
            id: self.id,
            unique_id: self.unique_id,
            index: self.index,
            hashcode: self.hashcode.clone(),
            virtual_expr: self.virtual_expr.clone(),
            orig_name: self.orig_name.clone(),
            is_hidden: self.is_hidden,
            is_prefix: self.is_prefix,
            in_operand: self.in_operand,
            collation_info: self.collation_info.clone(),
            correlated_col_unique_id: self.correlated_col_unique_id,
        }
    }
}

impl Column {
    /// Equal implements Expression interface.
    // go: Column.Equal L1
    pub fn equal(&self, expr: &Expr) -> bool {
        self.equal_column(expr)
    }

    /// EqualColumn returns whether two column is equal
    // go: Column.EqualColumn L1
    pub fn equal_column(&self, expr: &Expr) -> bool {
        if let Expr::Column(new_col) = expr {
            return new_col.unique_id == self.unique_id;
        }
        false
    }

    /// `EqualColumn` with a `*Column` argument.
    // glue: EqualColumn called with a *Column (no interface boxing needed)
    pub fn equal_col(&self, other: &Column) -> bool {
        other.unique_id == self.unique_id
    }

    /// StringWithCtx implements Expression interface.
    // go: Column.StringWithCtx L1
    pub fn string_with_ctx(&self, _ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        self.string(redact)
    }

    /// String implements Stringer interface.
    // go: Column.String L1 [M-NAME]
    pub fn to_string(&self) -> Vec<u8> {
        self.string(RedactLogDisable)
    }

    // go: Column.string L1
    fn string(&self, redact: &str) -> Vec<u8> {
        if let (true, Some(ve)) = (self.is_hidden, &self.virtual_expr) {
            // A hidden column without virtual expression indicates it's a stored type.
            // a virtual column should be able to be stringified without context.
            return ve.string_with_ctx(&**tidb_sessionctx::exprctx::EmptyParamValues, redact);
        }
        if !self.orig_name.is_empty() {
            return self.orig_name.clone().into_bytes();
        }
        format!("{}{}", columnPrefix, self.unique_id).into_bytes()
    }

    /// GetType implements Expression interface.
    // go: Column.GetType L1
    #[inline]
    pub fn get_type(&self) -> &FieldType {
        self.get_static_type()
    }

    /// GetStaticType returns the type without considering the context.
    // go: Column.GetStaticType L1
    #[inline]
    pub fn get_static_type(&self) -> &FieldType {
        &self.ret_type
    }

    /// `RetType` for an in-place change (Go's `col.RetType.SetXxx(..)` / `col.RetType = ..`
    /// through the shared pointer): copy on write (`Arc::make_mut`), so the clones that share
    /// the type keep the old one (`M-FT`: Rust changes only this column's type; N13 §8).
    // glue: M-FT / M-SHARE copy on write of the shared *FieldType
    #[inline]
    pub fn ret_type_mut(&mut self) -> &mut FieldType {
        Arc::make_mut(&mut self.ret_type)
    }

    /// Traverse implements the TraverseDown interface.
    // go: Column.Traverse L1
    pub fn traverse(&self, this: &ExprRef, action: &mut dyn TraverseAction) -> ExprRef {
        action.transform(this)
    }

    /// Eval implements Expression interface.
    // go: Column.Eval L1
    pub fn eval(&self, _ctx: &dyn EvalContext, row: Row<'_>) -> Result<Datum, Error> {
        Ok(row.get_datum(self.index as usize, &self.ret_type))
    }

    /// VecEvalInt of a hybrid (ENUM/SET/BIT) column: each row through `EvalInt`. The RPN calls
    /// it only for hybrid columns; Go's other branch is a plain copy, which the RPN does.
    // go: Column.VecEvalInt L1
    pub fn vec_eval_int(
        &self,
        ctx: &dyn EvalContext,
        input: &tidb_chunk::Chunk,
        result: &mut tidb_chunk::Column,
    ) -> Result<(), Error> {
        debug_assert!(self.ret_type.hybrid());
        result.resize_int64(0, false);
        for idx in chunk_rows(input) {
            match self.eval_int(ctx, input.get_row(idx))? {
                None => result.append_null(),
                Some(v) => result.append_int64(v),
            }
        }
        Ok(())
    }

    /// VecEvalReal of a FLOAT column: float32 widened to float64. The RPN calls it only for
    /// FLOAT columns; Go's other branch is a plain copy, which the RPN does.
    // go: Column.VecEvalReal L1
    pub fn vec_eval_real(
        &self,
        _ctx: &dyn EvalContext,
        input: &tidb_chunk::Chunk,
        result: &mut tidb_chunk::Column,
    ) -> Result<(), Error> {
        debug_assert_eq!(self.get_type().get_type(), mysql::TypeFloat);
        let n = input.num_rows();
        let src = input.column(self.index as usize);
        result.resize_float64(n, false);
        if let Some(sel) = input.sel() {
            for (i, &j) in sel.iter().enumerate() {
                if src.is_null(j) {
                    result.set_null(i, true);
                } else {
                    result.float64s_mut()[i].set(f64::from(src.get_float32(j)));
                }
            }
            return Ok(());
        }
        result.merge_nulls(&[src]);
        for i in 0..n {
            if result.is_null(i) {
                continue;
            }
            result.float64s_mut()[i].set(f64::from(src.get_float32(i)));
        }
        Ok(())
    }

    /// VecEvalString of a hybrid (ENUM/SET/BIT) column: each row through `EvalString`. The
    /// RPN calls it only for hybrid columns; Go's other branch is a plain copy.
    // go: Column.VecEvalString L1
    pub fn vec_eval_string(
        &self,
        ctx: &dyn EvalContext,
        input: &tidb_chunk::Chunk,
        result: &mut tidb_chunk::Column,
    ) -> Result<(), Error> {
        debug_assert!(self.ret_type.hybrid());
        result.reserve_string(input.num_rows());
        for idx in chunk_rows(input) {
            match self.eval_string(ctx, input.get_row(idx))? {
                None => result.append_null(),
                Some(v) => result.append_bytes(&v),
            }
        }
        Ok(())
    }

    /// EvalInt returns int representation of Column.
    // go: Column.EvalInt L1
    pub fn eval_int(&self, ctx: &dyn EvalContext, row: Row<'_>) -> Result<Option<i64>, Error> {
        if self.get_type().hybrid() {
            let val = row.get_datum(self.index as usize, &self.ret_type);
            if val.is_null() {
                return Ok(None);
            }
            if val.kind() == KindMysqlBit {
                let (v, r) = val.get_binary_literal().to_int(&type_ctx(ctx));
                r?;
                return Ok(Some(v as i64));
            }
            let (res, r) = val.to_int64(&type_ctx(ctx));
            r?;
            return Ok(Some(res));
        }
        if row.is_null(self.index as usize) {
            return Ok(None);
        }
        Ok(Some(row.get_int64(self.index as usize)))
    }

    /// EvalReal returns real representation of Column.
    // go: Column.EvalReal L1
    pub fn eval_real(&self, _ctx: &dyn EvalContext, row: Row<'_>) -> Result<Option<f64>, Error> {
        if row.is_null(self.index as usize) {
            return Ok(None);
        }
        if self.get_type().get_type() == mysql::TypeFloat {
            return Ok(Some(f64::from(row.get_float32(self.index as usize))));
        }
        Ok(Some(row.get_float64(self.index as usize)))
    }

    /// EvalString returns string representation of Column.
    // go: Column.EvalString L1
    pub fn eval_string(
        &self,
        _ctx: &dyn EvalContext,
        row: Row<'_>,
    ) -> Result<Option<Vec<u8>>, Error> {
        if row.is_null(self.index as usize) {
            return Ok(None);
        }

        // Specially handle the ENUM/SET/BIT input value.
        if self.get_type().hybrid() {
            let val = row.get_datum(self.index as usize, &self.ret_type);
            return val.to_string().map(Some);
        }

        Ok(Some(row.get_string(self.index as usize).to_vec()))
    }

    /// EvalDecimal returns decimal representation of Column.
    // go: Column.EvalDecimal L1
    pub fn eval_decimal(
        &self,
        _ctx: &dyn EvalContext,
        row: Row<'_>,
    ) -> Result<Option<MyDecimal>, Error> {
        if row.is_null(self.index as usize) {
            return Ok(None);
        }
        Ok(Some(row.get_my_decimal(self.index as usize)))
    }

    /// EvalTime returns DATE/DATETIME/TIMESTAMP representation of Column.
    // go: Column.EvalTime L1
    pub fn eval_time(&self, _ctx: &dyn EvalContext, row: Row<'_>) -> Result<Option<Time>, Error> {
        if row.is_null(self.index as usize) {
            return Ok(None);
        }
        Ok(Some(row.get_time(self.index as usize)))
    }

    /// EvalDuration returns Duration representation of Column.
    // go: Column.EvalDuration L1
    pub fn eval_duration(
        &self,
        _ctx: &dyn EvalContext,
        row: Row<'_>,
    ) -> Result<Option<Duration>, Error> {
        if row.is_null(self.index as usize) {
            return Ok(None);
        }
        let duration = row.get_duration(self.index as usize, self.ret_type.get_decimal());
        Ok(Some(duration))
    }

    /// EvalJSON returns JSON representation of Column.
    // go: Column.EvalJSON L1
    pub fn eval_json(
        &self,
        _ctx: &dyn EvalContext,
        row: Row<'_>,
    ) -> Result<Option<BinaryJSON<'static>>, Error> {
        if row.is_null(self.index as usize) {
            return Ok(None);
        }
        Ok(Some(row.get_json(self.index as usize).into_owned()))
    }

    /// EvalVectorFloat32 returns VectorFloat32 representation of Column. The value borrows
    /// from the row, as Go's (`row.GetVectorFloat32` is a zero-copy view).
    // go: Column.EvalVectorFloat32 L1
    pub fn eval_vector_float32<'r>(
        &self,
        _ctx: &dyn EvalContext,
        row: Row<'r>,
    ) -> Result<Option<VectorFloat32<'r>>, Error> {
        if row.is_null(self.index as usize) {
            return Ok(None);
        }
        Ok(Some(row.get_vector_float32(self.index as usize)))
    }

    /// IsCorrelated implements Expression interface.
    // go: Column.IsCorrelated L1
    pub fn is_correlated(&self) -> bool {
        false
    }

    /// ConstLevel returns the const level for the expression
    // go: Column.ConstLevel L1
    pub fn const_level(&self) -> ConstLevel {
        ConstNone
    }

    /// Decorrelate implements Expression interface.
    // go: Column.Decorrelate L1
    pub fn decorrelate(&self, this: &ExprRef, _schema: &Schema) -> ExprRef {
        Arc::clone(this)
    }

    /// HashCode implements Expression interface.
    // go: Column.HashCode L1
    pub fn hash_code(&self) -> &[u8] {
        self.hashcode.get_or_init(|| {
            let mut h = Vec::with_capacity(9);
            h.push(columnFlag);
            tidb_codec::encode_int(&mut h, self.unique_id);
            h
        })
    }

    /// CanonicalHashCode implements Expression interface.
    // go: Column.CanonicalHashCode L1
    pub fn canonical_hash_code(&self) -> &[u8] {
        self.hash_code()
    }

    /// CleanHashCode will clean the hashcode you may be cached before. It's used especially in
    /// schema-cloned & reallocated-uniqueID's cases.
    // go: Column.CleanHashCode L1
    pub fn clean_hash_code(&mut self) {
        self.hashcode = OnceLock::new();
    }

    /// ResolveIndices implements Expression interface.
    // go: Column.ResolveIndices L1
    pub fn resolve_indices(&self, schema: &Schema) -> Result<Column, Error> {
        let mut new_col = self.clone();
        new_col.resolve_indices_in_place(schema)?;
        Ok(new_col)
    }

    // go: Column.resolveIndices L1
    pub(crate) fn resolve_indices_in_place(&mut self, schema: &Schema) -> Result<(), Error> {
        self.index = schema.column_index(self);
        if self.index == -1 {
            return Err(Error::plain(format!(
                "Can't find column {} in schema {}",
                String::from_utf8_lossy(&self.to_string()),
                String::from_utf8_lossy(&schema.to_string())
            )));
        }
        Ok(())
    }

    /// ResolveIndicesByVirtualExpr implements Expression interface.
    // go: Column.ResolveIndicesByVirtualExpr L1
    pub fn resolve_indices_by_virtual_expr(
        &self,
        ctx: &dyn EvalContext,
        schema: &Schema,
    ) -> (Column, bool) {
        let mut new_col = self.clone();
        let is_ok = new_col.resolve_indices_by_virtual_expr_in_place(ctx, schema);
        (new_col, is_ok)
    }

    // go: Column.resolveIndicesByVirtualExpr L1
    pub(crate) fn resolve_indices_by_virtual_expr_in_place(
        &mut self,
        ctx: &dyn EvalContext,
        schema: &Schema,
    ) -> bool {
        let mut fallback_idx: isize = -1;
        for (i, c) in schema.columns.iter().enumerate() {
            if c.unique_id == self.unique_id {
                self.index = i as isize;
                return true;
            }
            // Different expression indexes may create hidden generated columns with the same
            // virtual expression. Prefer the exact column ID when it exists; only fall back
            // to expression equality when the selected schema has no exact column match.
            if fallback_idx == -1 && c.equal_by_expr_and_id(ctx, self) {
                fallback_idx = i as isize;
            }
        }
        if fallback_idx != -1 {
            self.index = fallback_idx;
            return true;
        }
        false
    }

    /// EqualByExprAndID extends Equal by comparing virtual expression.
    // go: Column.EqualByExprAndID L1
    pub fn equal_by_expr_and_id(&self, ctx: &dyn EvalContext, new_col: &Column) -> bool {
        let is_vir_expr_matched = match (&self.virtual_expr, &new_col.virtual_expr) {
            (Some(e), other) if matches!(&**e, Expr::ScalarFunction(_)) => {
                let Expr::ScalarFunction(expr) = &**e else {
                    unreachable!()
                };
                // expr.Equal(ctx, nil) is false for a nil VirtualExpr.
                other.as_ref().is_some_and(|o| expr.equal(ctx, o))
                    && self.ret_type.equal(&new_col.ret_type)
            }
            _ => false,
        };
        new_col.unique_id == self.unique_id || is_vir_expr_matched
    }

    /// RemapColumn remaps columns with provided mapping and returns new expression
    // go: Column.RemapColumn L1
    pub fn remap_column(&self, m: &HashMap<i64, Arc<Column>>) -> Result<ExprRef, Error> {
        let Some(mapped) = m.get(&self.unique_id) else {
            return Err(Error::plain(format!(
                "Can't remap column for {}",
                String::from_utf8_lossy(&self.to_string())
            )));
        };
        Ok(Expr::Column((**mapped).clone()).into_ref())
    }

    /// Vectorized returns if this expression supports vectorized evaluation.
    // go: Column.Vectorized L1
    pub fn vectorized(&self) -> bool {
        true
    }

    /// ToInfo converts the expression.Column to model.ColumnInfo for casting values,
    /// beware it doesn't fill all the fields of the model.ColumnInfo.
    // go: Column.ToInfo L1
    pub fn to_info(&self) -> ColumnInfo {
        ColumnInfo {
            id: self.id,
            field_type: FieldType::clone(&self.ret_type),
            ..ColumnInfo::default()
        }
    }

    /// EvalVirtualColumn evals the virtual column
    // go: Column.EvalVirtualColumn L1
    pub fn eval_virtual_column(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Datum, Error> {
        self.virtual_expr
            .as_ref()
            .expect("Go dereferences a nil VirtualExpr")
            .eval(ctx, st, row)
    }

    /// Coercibility returns the coercibility value which is used to check collations.
    // go: Column.Coercibility L1
    pub fn coercibility(&self) -> Coercibility {
        if !self.collation_info.has_coercibility() {
            self.collation_info
                .set_coercibility(derive_coercibility_for_column(self));
        }
        self.collation_info.coercibility()
    }

    /// Repertoire returns the repertoire value which is used to check collations.
    // go: Column.Repertoire L1
    pub fn repertoire(&self) -> Repertoire {
        if self.collation_info.repertoire.0 != 0 {
            return self.collation_info.repertoire;
        }
        let et = self.ret_type.eval_type();
        if et == ETJson {
            UNICODE
        } else if et == ETString {
            if self.ret_type.get_charset() == CharsetASCII {
                return ASCII;
            }
            UNICODE
        } else {
            ASCII
        }
    }

    /// InColumnArray check whether the col is in the cols array
    // go: Column.InColumnArray L1
    pub fn in_column_array(&self, cols: &[Arc<Column>]) -> bool {
        cols.iter().any(|c| self.equal_col(c))
    }

    /// MemoryUsage return the memory usage of Column
    // go: Column.MemoryUsage L2 (unsafe.Sizeof(Column{}) is Go's size on 64-bit targets, spec §4)
    pub fn memory_usage(&self) -> i64 {
        let hashcode_cap = self.hashcode.get().map_or(0, |h| h.capacity().max(9)) as i64;
        let mut sum = emptyColumnSize
            + hashcode_cap
            + self.orig_name.len() as i64
            + self.collation_info.strings_len();

        sum += self.ret_type.memory_usage();
        if let Some(ve) = &self.virtual_expr {
            sum += ve.memory_usage();
        }
        sum
    }
}

/// `unsafe.Sizeof(Column{})` of Go v8.5.8 on amd64/arm64.
#[allow(non_upper_case_globals)]
pub(crate) const emptyColumnSize: i64 = 168;

/// Column2Exprs will transfer column slice to expression slice.
// go: Column2Exprs L1
pub fn column2_exprs(cols: &[Arc<Column>]) -> Vec<ExprRef> {
    cols.iter()
        .map(|c| Expr::Column((**c).clone()).into_ref())
        .collect()
}

/// ColInfo2Col finds the corresponding column of the ColumnInfo in a column slice.
// go: ColInfo2Col L1
pub fn col_info_to_col<'a>(cols: &'a [Arc<Column>], col: &ColumnInfo) -> Option<&'a Arc<Column>> {
    cols.iter().find(|c| c.id == col.id)
}

/// SortColumns sort columns based on UniqueID.
// go: SortColumns L1
pub fn sort_columns(cols: &[Arc<Column>]) -> Vec<Arc<Column>> {
    let mut sorted = cols.to_vec();
    sorted.sort_by_key(|c| c.unique_id);
    sorted
}

/// GcColumnExprIsTidbShard check whether the expression is tidb_shard()
// go: GcColumnExprIsTidbShard L1
pub fn gc_column_expr_is_tidb_shard(virtual_expr: Option<&Expr>) -> bool {
    let Some(virtual_expr) = virtual_expr else {
        return false;
    };
    let Some(f) = virtual_expr.as_scalar_function() else {
        return false;
    };
    if f.func_name.l != ast_func::TiDBShard {
        return false;
    }
    true
}

/// The logical row indexes of `input` in order (Go `NewIterator4Chunk`): `GetRow` maps them
/// through the selection vector (mapping them here as well read the wrong rows, or past the
/// end, when the chunk had a `Sel()`; found by the D08.1 2b govec records).
// glue: Go chunk.Iterator4Chunk over the selected rows
fn chunk_rows(input: &tidb_chunk::Chunk) -> std::ops::Range<usize> {
    0..input.num_rows()
}
