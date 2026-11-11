//! go-file: pkg/expression/expression.go
//!
//! The `Expression` interface as an enum (N4 §3.1, mapping rules M-EXPR and M-IFACE): Go's
//! four node types are the four variants, the interface methods are methods of [`Expr`]
//! that dispatch on the variant, and `switch e.(type)` is a `match`. Nodes are shared as
//! [`ExprRef`] (`Arc<Expr>`): Go shares `Expression` values between plans and clones before
//! modifying; Rust shares immutable nodes and copies on write.
//!
//! Row evaluation (`Eval*`) takes an [`EvalState`] besides the context (N4 §3.4: the row
//! driver carries the per-execution state too); in part 1 it only holds the values of
//! correlated columns (spec §3.2). `Ok(None)` is Go's `isNull == true`.
//!
//! Part 1 ports the node types and the functions of this file that do not build scalar
//! functions; `ComposeCNFCondition`, `EvaluateExprWithNull`, `wrapWithIsTrue`, the
//! `VecEvalBool` family and the other builders come with part 2 (spec §1).

#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tidb_base::Error;
use tidb_base::mysql;
use tidb_chunk::Row;
use tidb_model::{ColumnInfo, StatePublic, TableInfo};
use tidb_parser::model::CIStr;
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::{BuildContextRef, ctx_with_handle_truncate_err_level};
use tidb_types::datum::{Datum, KindBinaryLiteral};
use tidb_types::etc::FieldType;
use tidb_types::field_name::FieldName;
use tidb_types::json::BinaryJSON;
use tidb_types::mydecimal::MyDecimal;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::VectorFloat32;

use crate::collation::{Coercibility, Repertoire};
use crate::column::{Column, CorrelatedColumn};
use crate::constant::Constant;
use crate::context::{EvalContext, ParamValues, type_ctx};
use crate::scalar_function::{ScalarFunction, ast_func};
use crate::schema::{KeyInfo, Schema, new_schema};

/// Hash code flag of a constant (`constantFlag`).
pub(crate) const constantFlag: u8 = 0;
/// Hash code flag of a column (`columnFlag`).
pub(crate) const columnFlag: u8 = 1;
/// Hash code flag of a scalar function (`scalarFunctionFlag`).
#[allow(dead_code)] // used by ScalarFunction.HashCode (part 2)
pub(crate) const scalarFunctionFlag: u8 = 3;
/// Hash code flag of a parameter marker (`parameterFlag`).
pub(crate) const parameterFlag: u8 = 4;
/// ScalarSubQFlag indicates the hashcode of a scalar sub-query.
pub const ScalarSubQFlag: u8 = 5;

/// ConstLevel indicates the const level for an expression
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConstLevel(pub u32);

/// ConstNone indicates the expression is not a constant expression.
pub const ConstNone: ConstLevel = ConstLevel(0);
/// ConstOnlyInContext indicates the expression is only a constant for a same context.
pub const ConstOnlyInContext: ConstLevel = ConstLevel(1);
/// ConstStrict indicates the expression is a constant expression.
pub const ConstStrict: ConstLevel = ConstLevel(2);

/// A shared expression node (Go's `Expression` interface value).
pub type ExprRef = Arc<Expr>;

/// Expression represents all scalar expression in SQL.
#[derive(Clone, Debug)]
pub enum Expr {
    /// `*Column`.
    Column(Column),
    /// `*CorrelatedColumn`.
    CorrelatedColumn(CorrelatedColumn),
    /// `*Constant`.
    Constant(Constant),
    /// `*ScalarFunction`.
    ScalarFunction(ScalarFunction),
}

/// Per-execution state of row evaluation (N4 §3.4). Part 1: the values of correlated
/// columns, by slot (spec §3.2); Go keeps them behind `CorrelatedColumn.Data`.
#[derive(Debug, Default)]
pub struct EvalState {
    corr: HashMap<i64, Datum>,
}

/// The zero Datum (`new(types.Datum)`) a correlated column reads before Apply writes it.
static NullDatum: OnceLock<Datum> = OnceLock::new();

impl EvalState {
    /// A state without correlated values.
    // glue: per-execution state of N4 §3.4
    pub fn new() -> EvalState {
        EvalState::default()
    }

    /// `*col.Data = d`: Apply writes the outer row's value of a correlated column.
    // glue: write through CorrelatedColumn.Data (spec §3.2)
    pub fn set_correlated(&mut self, col: &CorrelatedColumn, d: Datum) {
        self.corr.insert(col.slot.0, d);
    }

    /// `*col.Data`.
    // glue: read through CorrelatedColumn.Data (spec §3.2)
    pub fn correlated(&self, col: &CorrelatedColumn) -> &Datum {
        self.corr
            .get(&col.slot.0)
            .unwrap_or_else(|| NullDatum.get_or_init(Datum::default))
    }
}

/// `TraverseAction` defines the interface for action when traversing down an expression.
pub trait TraverseAction {
    /// Transform.
    // glue: interface method of TraverseAction
    fn transform(&mut self, e: &ExprRef) -> ExprRef;
}

impl Expr {
    /// Wraps a node into a shared reference.
    // glue: Go takes the address of a node to get an Expression
    pub fn into_ref(self) -> ExprRef {
        Arc::new(self)
    }

    /// The column, if this is a `*Column`.
    // glue: type assertion expr.(*Column) (M-IFACE)
    pub fn as_column(&self) -> Option<&Column> {
        match self {
            Expr::Column(c) => Some(c),
            _ => None,
        }
    }

    /// The constant, if this is a `*Constant`.
    // glue: type assertion expr.(*Constant) (M-IFACE)
    pub fn as_constant(&self) -> Option<&Constant> {
        match self {
            Expr::Constant(c) => Some(c),
            _ => None,
        }
    }

    /// The scalar function, if this is a `*ScalarFunction`.
    // glue: type assertion expr.(*ScalarFunction) (M-IFACE)
    pub fn as_scalar_function(&self) -> Option<&ScalarFunction> {
        match self {
            Expr::ScalarFunction(f) => Some(f),
            _ => None,
        }
    }

    /// The correlated column, if this is a `*CorrelatedColumn`.
    // glue: type assertion expr.(*CorrelatedColumn) (M-IFACE)
    pub fn as_correlated_column(&self) -> Option<&CorrelatedColumn> {
        match self {
            Expr::CorrelatedColumn(c) => Some(c),
            _ => None,
        }
    }

    /// Traverse lets the action transform this node (`Traverse`).
    // glue: dispatch of Expression.Traverse
    pub fn traverse(self: &Arc<Self>, action: &mut dyn TraverseAction) -> ExprRef {
        match &**self {
            Expr::Column(c) => c.traverse(self, action),
            Expr::CorrelatedColumn(c) => c.traverse(self, action),
            Expr::Constant(c) => c.traverse(self, action),
            Expr::ScalarFunction(f) => f.traverse(self, action),
        }
    }

    /// Eval evaluates an expression through a row.
    // glue: dispatch of Expression.Eval
    pub fn eval(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Datum, Error> {
        match self {
            Expr::Column(c) => c.eval(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval(ctx, st, row),
            Expr::Constant(c) => c.eval(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval(ctx, st, row),
        }
    }

    /// EvalInt returns the int64 representation of expression.
    // glue: dispatch of Expression.EvalInt
    pub fn eval_int(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<i64>, Error> {
        match self {
            Expr::Column(c) => c.eval_int(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_int(ctx, st, row),
            Expr::Constant(c) => c.eval_int(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_int(ctx, st, row),
        }
    }

    /// EvalReal returns the float64 representation of expression.
    // glue: dispatch of Expression.EvalReal
    pub fn eval_real(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<f64>, Error> {
        match self {
            Expr::Column(c) => c.eval_real(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_real(ctx, st, row),
            Expr::Constant(c) => c.eval_real(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_real(ctx, st, row),
        }
    }

    /// EvalString returns the string representation of expression.
    // glue: dispatch of Expression.EvalString
    pub fn eval_string(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Vec<u8>>, Error> {
        match self {
            Expr::Column(c) => c.eval_string(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_string(ctx, st, row),
            Expr::Constant(c) => c.eval_string(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_string(ctx, st, row),
        }
    }

    /// EvalDecimal returns the decimal representation of expression.
    // glue: dispatch of Expression.EvalDecimal
    pub fn eval_decimal(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<MyDecimal>, Error> {
        match self {
            Expr::Column(c) => c.eval_decimal(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_decimal(ctx, st, row),
            Expr::Constant(c) => c.eval_decimal(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_decimal(ctx, st, row),
        }
    }

    /// EvalTime returns the DATE/DATETIME/TIMESTAMP representation of expression.
    // glue: dispatch of Expression.EvalTime
    pub fn eval_time(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Time>, Error> {
        match self {
            Expr::Column(c) => c.eval_time(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_time(ctx, st, row),
            Expr::Constant(c) => c.eval_time(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_time(ctx, st, row),
        }
    }

    /// EvalDuration returns the duration representation of expression.
    // glue: dispatch of Expression.EvalDuration
    pub fn eval_duration(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Duration>, Error> {
        match self {
            Expr::Column(c) => c.eval_duration(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_duration(ctx, st, row),
            Expr::Constant(c) => c.eval_duration(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_duration(ctx, st, row),
        }
    }

    /// EvalJSON returns the JSON representation of expression.
    // glue: dispatch of Expression.EvalJSON
    pub fn eval_json(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<BinaryJSON<'static>>, Error> {
        match self {
            Expr::Column(c) => c.eval_json(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_json(ctx, st, row),
            Expr::Constant(c) => c.eval_json(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_json(ctx, st, row),
        }
    }

    /// EvalVectorFloat32 returns the VectorFloat32 representation of expression.
    // glue: dispatch of Expression.EvalVectorFloat32
    pub fn eval_vector_float32(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<VectorFloat32<'static>>, Error> {
        match self {
            Expr::Column(c) => c.eval_vector_float32(ctx, row),
            Expr::CorrelatedColumn(c) => c.eval_vector_float32(ctx, st, row),
            Expr::Constant(c) => c.eval_vector_float32(ctx, st, row),
            Expr::ScalarFunction(f) => f.eval_vector_float32(ctx, st, row),
        }
    }

    /// GetType gets the type that the expression returns.
    // glue: dispatch of Expression.GetType
    #[inline]
    pub fn get_type(&self, ctx: &dyn EvalContext) -> Cow<'_, FieldType> {
        match self {
            Expr::Column(c) => Cow::Borrowed(c.get_type()),
            Expr::CorrelatedColumn(c) => Cow::Borrowed(c.column.get_type()),
            Expr::Constant(c) => c
                .get_type(ctx)
                .expect("nil *FieldType: the parameter cannot be read"),
            Expr::ScalarFunction(f) => Cow::Borrowed(f.get_type()),
        }
    }

    /// GetStaticType returns the type without considering the context.
    // glue: dispatch of Expression.GetStaticType
    #[inline]
    pub fn get_static_type(&self) -> &FieldType {
        match self {
            Expr::Column(c) => c.get_static_type(),
            Expr::CorrelatedColumn(c) => c.column.get_static_type(),
            Expr::Constant(c) => &c.ret_type,
            Expr::ScalarFunction(f) => f.get_type(),
        }
    }

    /// StringWithCtx returns the string representation of the expression with context.
    // glue: dispatch of Expression.StringWithCtx
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        match self {
            Expr::Column(c) => c.string_with_ctx(ctx, redact),
            Expr::CorrelatedColumn(c) => c.column.string_with_ctx(ctx, redact),
            Expr::Constant(c) => c.string_with_ctx(ctx, redact),
            Expr::ScalarFunction(f) => f.string_with_ctx(ctx, redact),
        }
    }

    /// Equal checks whether two expressions are equal.
    // glue: dispatch of Expression.Equal
    pub fn equal(&self, ctx: &dyn EvalContext, e: &Expr) -> bool {
        match self {
            Expr::Column(c) => c.equal(e),
            Expr::CorrelatedColumn(c) => c.equal(e),
            Expr::Constant(c) => c.equal(ctx, e),
            Expr::ScalarFunction(f) => f.equal(ctx, e),
        }
    }

    /// IsCorrelated checks if this expression has correlated key.
    // glue: dispatch of Expression.IsCorrelated
    pub fn is_correlated(&self) -> bool {
        match self {
            Expr::Column(c) => c.is_correlated(),
            Expr::CorrelatedColumn(c) => c.is_correlated(),
            Expr::Constant(c) => c.is_correlated(),
            Expr::ScalarFunction(f) => f.is_correlated(),
        }
    }

    /// ConstLevel returns the const level of the expression.
    // glue: dispatch of Expression.ConstLevel
    pub fn const_level(&self) -> ConstLevel {
        match self {
            Expr::Column(c) => c.const_level(),
            Expr::CorrelatedColumn(c) => c.const_level(),
            Expr::Constant(c) => c.const_level(),
            Expr::ScalarFunction(f) => f.const_level(),
        }
    }

    /// Decorrelate try to decorrelate the expression by schema.
    // glue: dispatch of Expression.Decorrelate
    pub fn decorrelate(self: &Arc<Self>, schema: &Schema) -> ExprRef {
        match &**self {
            Expr::Column(c) => c.decorrelate(self, schema),
            Expr::Constant(c) => c.decorrelate(self, schema),
            Expr::CorrelatedColumn(c) => c.decorrelate(self, schema),
            Expr::ScalarFunction(f) => f.decorrelate(self, schema),
        }
    }

    /// ResolveIndices resolves indices by the given schema.
    // glue: dispatch of Expression.ResolveIndices
    pub fn resolve_indices(self: &Arc<Self>, schema: &Schema) -> Result<ExprRef, Error> {
        match &**self {
            Expr::Column(c) => c
                .resolve_indices(schema)
                .map(|c| Expr::Column(c).into_ref()),
            Expr::CorrelatedColumn(c) => c.resolve_indices(self, schema),
            Expr::Constant(c) => c.resolve_indices(self, schema),
            Expr::ScalarFunction(f) => f.resolve_indices(self, schema),
        }
    }

    /// RemapColumn remaps columns with provided mapping and returns new expression.
    // glue: dispatch of Expression.RemapColumn
    pub fn remap_column(self: &Arc<Self>, m: &HashMap<i64, Arc<Column>>) -> Result<ExprRef, Error> {
        match &**self {
            Expr::Column(c) => c.remap_column(m),
            Expr::CorrelatedColumn(c) => c.remap_column(m),
            Expr::Constant(c) => c.remap_column(self, m),
            Expr::ScalarFunction(f) => f.remap_column(self, m),
        }
    }

    /// HashCode creates the hashcode for expression which can be used to identify itself
    /// from other expression.
    // glue: dispatch of Expression.HashCode
    pub fn hash_code(&self) -> &[u8] {
        match self {
            Expr::Column(c) => c.hash_code(),
            Expr::CorrelatedColumn(c) => c.column.hash_code(),
            Expr::Constant(c) => c.hash_code(),
            Expr::ScalarFunction(f) => f.hash_code(),
        }
    }

    /// CanonicalHashCode is the same as HashCode except it sorts the arguments of
    /// commutative functions.
    // glue: dispatch of Expression.CanonicalHashCode
    pub fn canonical_hash_code(&self) -> &[u8] {
        match self {
            Expr::Column(c) => c.canonical_hash_code(),
            Expr::CorrelatedColumn(c) => c.column.canonical_hash_code(),
            Expr::Constant(c) => c.canonical_hash_code(),
            Expr::ScalarFunction(f) => f.canonical_hash_code(),
        }
    }

    /// Vectorized returns if this expression supports vectorized evaluation.
    // glue: dispatch of Expression.Vectorized
    pub fn vectorized(&self) -> bool {
        match self {
            Expr::Column(c) => c.vectorized(),
            Expr::CorrelatedColumn(c) => c.column.vectorized(),
            Expr::Constant(c) => c.vectorized(),
            Expr::ScalarFunction(f) => f.vectorized(),
        }
    }

    /// Coercibility returns the coercibility value which is used to check collations.
    // glue: dispatch of CollationInfo.Coercibility
    pub fn coercibility(&self) -> Coercibility {
        match self {
            Expr::Column(c) => c.coercibility(),
            Expr::CorrelatedColumn(c) => c.column.coercibility(),
            Expr::Constant(c) => c.coercibility(),
            Expr::ScalarFunction(f) => f.coercibility(),
        }
    }

    /// Repertoire returns the repertoire value which is used to check collations.
    // glue: dispatch of CollationInfo.Repertoire
    pub fn repertoire(&self) -> Repertoire {
        match self {
            Expr::Column(c) => c.repertoire(),
            Expr::CorrelatedColumn(c) => c.column.repertoire(),
            Expr::Constant(c) => c.collation_info.repertoire(),
            Expr::ScalarFunction(f) => f.repertoire(),
        }
    }

    /// MemoryUsage returns the memory usage of the expression.
    // glue: dispatch of Expression.MemoryUsage
    pub fn memory_usage(&self) -> i64 {
        match self {
            Expr::Column(c) => c.memory_usage(),
            Expr::CorrelatedColumn(c) => c.memory_usage(),
            Expr::Constant(c) => c.memory_usage(),
            Expr::ScalarFunction(f) => f.memory_usage(),
        }
    }
}

/// CNFExprs stands for a CNF expression.
pub type CNFExprs = Vec<ExprRef>;

/// CNFExprs.Clone clones itself.
// go: CNFExprs.Clone L2 (nodes are immutable and shared: cloning the list is enough, M-EXPR)
pub fn cnf_exprs_clone(e: &CNFExprs) -> CNFExprs {
    e.clone()
}

/// CNFExprs.Shallow makes a shallow copy of itself.
// go: CNFExprs.Shallow L1
pub fn cnf_exprs_shallow(e: &CNFExprs) -> CNFExprs {
    e.clone()
}

/// IsEQCondFromIn checks if an expression is equal condition converted from `[not] in (subq)`.
// go: IsEQCondFromIn L1
pub fn is_eq_cond_from_in(expr: &Expr) -> bool {
    let Some(sf) = expr.as_scalar_function() else {
        return false;
    };
    if sf.func_name.l != ast_func::EQ {
        return false;
    }
    let mut cols = Vec::new();
    for arg in sf.get_args() {
        extract_columns_into(arg, &mut cols);
    }
    cols.iter().any(|c| c.in_operand)
}

/// The `ExtractColumns` walk used by `IsEQCondFromIn` (`util.go`, block D08.12).
// go-part: IsEQCondFromIn
fn extract_columns_into<'a>(e: &'a Expr, out: &mut Vec<&'a Column>) {
    match e {
        Expr::Column(c) => out.push(c),
        Expr::ScalarFunction(f) => {
            for a in f.get_args() {
                extract_columns_into(a, out);
            }
        }
        _ => {}
    }
}

/// EvalBool evaluates expression list to a boolean value. The first returned value
/// indicates bool result of the expression list, the second returned value indicates
/// whether the result of the expression list is null, it can only be true when the
/// first returned values is false.
// go: EvalBool L1
pub fn eval_bool(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr_list: &[ExprRef],
    row: Row<'_>,
) -> Result<(bool, bool), Error> {
    let mut has_null = false;
    let tc = type_ctx(ctx);
    for expr in expr_list {
        let data = expr.eval(ctx, st, row)?;
        if data.is_null() {
            // For queries like `select a in (select a from s where t.b = s.b) from t`,
            // if result of `t.a = s.a` is null, we cannot return immediately until
            // we have checked if `t.b = s.b` is null or false, because it means
            // subquery is empty, and we should return false as the result of the whole
            // exprList in that case, instead of null.
            if !is_eq_cond_from_in(expr) {
                return Ok((false, false));
            }
            has_null = true;
            continue;
        }

        let (i, r) = data.to_bool(&tc);
        r?;
        if i == 0 {
            return Ok((false, false));
        }
    }
    if has_null {
        return Ok((false, true));
    }
    Ok((true, false))
}

/// IsBinaryLiteral checks whether an expression is a binary literal
// go: IsBinaryLiteral L1
pub fn is_binary_literal(expr: &Expr) -> bool {
    matches!(expr.as_constant(), Some(con) if con.value.kind() == KindBinaryLiteral)
}

/// ExprNotNull checks if an expression is possible to be null.
// go: ExprNotNull L1
pub fn expr_not_null(ctx: &dyn EvalContext, expr: &Expr) -> bool {
    if let Some(c) = expr.as_constant() {
        return !c.value.is_null();
    }
    // For ScalarFunction, the result would not be correct until we support maintaining
    // NotNull flag for it.
    mysql::has_not_null_flag(expr.get_type(ctx).get_flag())
}

/// `BuildSimpleExpr` builds a simple expression from an ast node. Go declares it as a
/// package-level function variable that the planner sets at init; here it is a callback
/// registered once (spec §2). The ast node is passed as the generated column's expression
/// string, which is what `ColumnInfos2ColumnsAndNames` parses.
pub type BuildSimpleExprFn = dyn Fn(
        &BuildContextRef,
        &ColumnInfo,
        &TableInfo,
        &Schema,
        &[FieldName],
    ) -> Result<Option<ExprRef>, Error>
    + Send
    + Sync;

static BUILD_SIMPLE_EXPR: OnceLock<Box<BuildSimpleExprFn>> = OnceLock::new();

/// Registers `BuildSimpleExpr` (the planner's `init`).
// glue: assignment to the package-level func variable BuildSimpleExpr
pub fn register_build_simple_expr(f: Box<BuildSimpleExprFn>) {
    let _ = BUILD_SIMPLE_EXPR.set(f);
}

/// ColumnInfos2ColumnsAndNames converts the ColumnInfo to the *Column and NameSlice.
// go: ColumnInfos2ColumnsAndNames L2 (the virtual expression is built by the registered BuildSimpleExpr from the column info, see BuildSimpleExprFn)
pub fn column_infos_to_columns_and_names(
    ctx: &BuildContextRef,
    db_name: &CIStr,
    tbl_name: &CIStr,
    col_infos: &[&ColumnInfo],
    tbl_info: &TableInfo,
) -> Result<(Vec<Arc<Column>>, Vec<FieldName>), Error> {
    let mut columns: Vec<Column> = Vec::with_capacity(col_infos.len());
    let mut names: Vec<FieldName> = Vec::with_capacity(col_infos.len());
    for col in col_infos {
        names.push(FieldName {
            orig_tbl_name: tbl_name.clone(),
            orig_col_name: col.name.clone(),
            db_name: db_name.clone(),
            tbl_name: tbl_name.clone(),
            col_name: col.name.clone(),
            ..FieldName::default()
        });
        let new_col = Column {
            ret_type: col.field_type.clone(),
            id: col.id,
            unique_id: ctx.alloc_plan_column_id(),
            index: col.offset,
            orig_name: names.last().expect("just pushed").string(),
            is_hidden: col.hidden,
            ..Column::default()
        };
        columns.push(new_col);
    }
    // Resolve virtual generated column.
    let mock_schema = new_schema(columns.iter().cloned().map(Arc::new).collect());

    let mut ctx = Arc::clone(ctx);
    let mut truncate_ignored = false;
    for (i, col) in col_infos.iter().enumerate() {
        if col.is_virtual_generated() {
            if !truncate_ignored {
                // Ignore redundant warning here.
                ctx = ctx_with_handle_truncate_err_level(ctx, errctx::LevelIgnore);
                truncate_ignored = true;
            }
            let build = BUILD_SIMPLE_EXPR
                .get()
                .expect("expression.BuildSimpleExpr is not registered (nil func in Go)");
            let e = build(&ctx, col, tbl_info, &mock_schema, &names)?;
            if let Some(e) = e {
                columns[i].virtual_expr = Some(e);
            }
            let ve = columns[i]
                .virtual_expr
                .clone()
                .expect("Go dereferences a nil VirtualExpr");
            columns[i].virtual_expr = Some(ve.resolve_indices(&mock_schema)?);
        }
    }
    Ok((columns.into_iter().map(Arc::new).collect(), names))
}

/// TableInfo2SchemaAndNames converts the TableInfo to the schema and name slice.
// go: TableInfo2SchemaAndNames L1
pub fn table_info_to_schema_and_names(
    ctx: &BuildContextRef,
    db_name: &CIStr,
    tbl: &TableInfo,
) -> Result<(Schema, Vec<FieldName>), Error> {
    let tbl_cols: Vec<&ColumnInfo> = tbl
        .cols()
        .into_iter()
        .map(|c| c.expect("Go dereferences a nil *ColumnInfo"))
        .collect();
    let (cols, names) = column_infos_to_columns_and_names(ctx, db_name, &tbl.name, &tbl_cols, tbl)?;
    let indices: &[_] = tbl.indices.as_deref().unwrap_or(&[]);
    let tbl_columns: &[ColumnInfo] = tbl.columns.as_deref().unwrap_or(&[]);
    let mut keys: Vec<KeyInfo> = Vec::with_capacity(indices.len() + 1);
    for idx in indices {
        if !idx.unique || idx.state != StatePublic {
            continue;
        }
        let mut ok = true;
        let idx_columns: &[_] = idx.columns.as_deref().unwrap_or(&[]);
        let mut new_key: KeyInfo = Vec::with_capacity(idx_columns.len());
        for idx_col in idx_columns {
            let mut find = false;
            for (i, col) in tbl_columns.iter().enumerate() {
                if idx_col.name.l == col.name.l {
                    if !mysql::has_not_null_flag(col.get_flag()) {
                        break;
                    }
                    new_key.push(Arc::clone(&cols[i]));
                    find = true;
                    break;
                }
            }
            if !find {
                ok = false;
                break;
            }
        }
        if ok {
            keys.push(new_key);
        }
    }
    if tbl.pk_is_handle {
        for (i, col) in tbl_columns.iter().enumerate() {
            if mysql::has_pri_key_flag(col.get_flag()) {
                keys.push(vec![Arc::clone(&cols[i])]);
                break;
            }
        }
    }
    let mut schema = new_schema(cols);
    schema.set_keys(keys);
    Ok((schema, names))
}
