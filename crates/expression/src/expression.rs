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

    /// ResolveIndicesByVirtualExpr resolves indices by the given schema in terms of virtual
    /// expression.
    // glue: dispatch of Expression.ResolveIndicesByVirtualExpr
    pub fn resolve_indices_by_virtual_expr(
        self: &Arc<Self>,
        ctx: &dyn EvalContext,
        schema: &Schema,
    ) -> (ExprRef, bool) {
        match &**self {
            Expr::Column(c) => {
                let (c, ok) = c.resolve_indices_by_virtual_expr(ctx, schema);
                (Expr::Column(c).into_ref(), ok)
            }
            Expr::CorrelatedColumn(c) => c.resolve_indices_by_virtual_expr(self, ctx, schema),
            Expr::Constant(c) => c.resolve_indices_by_virtual_expr(self, ctx, schema),
            Expr::ScalarFunction(f) => f.resolve_indices_by_virtual_expr(self, ctx, schema),
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

    /// CharsetAndCollation gets charset and collation (of the embedded `collationInfo`).
    // glue: dispatch of CollationInfo.CharsetAndCollation
    pub fn charset_and_collation(&self) -> (&str, &str) {
        match self {
            Expr::Column(c) => c.collation_info.charset_and_collation(),
            Expr::CorrelatedColumn(c) => c.column.collation_info.charset_and_collation(),
            Expr::Constant(c) => c.collation_info.charset_and_collation(),
            Expr::ScalarFunction(f) => f.charset_and_collation(),
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

/// wrapWithIsTrue wraps `arg` with istrue function if the return type of expr is not
/// type int, otherwise, returns `arg` directly.
/// The `keepNull` controls what the istrue function will return when `arg` is null:
/// 1. keepNull is true and arg is null, the istrue function returns null.
/// 2. keepNull is false and arg is null, the istrue function returns 0.
///
/// The `wrapForInt` indicates whether we need to wrapIsTrue for non-logical Expression with int type.
// go: wrapWithIsTrue L1
pub fn wrap_with_is_true(
    ctx: &dyn crate::context::BuildContext,
    keep_null: bool,
    arg: ExprRef,
    wrap_for_int: bool,
) -> Result<ExprRef, Error> {
    if arg.get_type(&*ctx.get_eval_ctx()).eval_type() == tidb_parser::types::ETInt {
        if !wrap_for_int {
            return Ok(arg);
        }
        if let Some(child) = arg.as_scalar_function()
            && crate::util::is_logical_op(&child.func_name.l)
        {
            return Ok(arg);
        }
    }
    use crate::builtin_registry::FunctionClass as _;
    let fc = if keep_null {
        &crate::builtin_op::IS_TRUTH_WITH_NULL_CLASS
    } else {
        &crate::builtin_op::IS_TRUTH_WITHOUT_NULL_CLASS
    };
    let mut f = fc.get_function(ctx, vec![arg])?;
    let name = if keep_null {
        crate::ast_funcs::IsTruthWithNull
    } else {
        crate::ast_funcs::IsTruthWithoutNull
    };
    let ret_type = f.get_ret_tp().clone();
    let sf = Expr::ScalarFunction(ScalarFunction::new(name, ret_type, f)).into_ref();
    Ok(crate::constant_fold::fold_constant(ctx, sf))
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

// ------------------------------------------------------------------ vector evaluation (part 2b)

/// The error of an evaluation type no evaluation path handles.
// glue: Go errors.Errorf("unsupported type %s during evaluation", evalType)
pub(crate) fn unsupported_eval_type(et: tidb_parser::types::EvalType) -> Error {
    Error::plain(format!(
        "unsupported type {} during evaluation",
        et.to_string()
    ))
}

/// VecEvalBool does the same thing as EvalBool but it works in a vectorized manner.
///
/// Go reuses the caller's `selected`/`nulls` slices and a pooled `sel`/`isZero`; here the
/// vectors are taken and returned (mapping rule M-POOL). The input's `Sel()` is cleared for
/// the evaluation and restored before returning, as Go's `defer`.
// go: VecEvalBool L2 (selected/nulls by value; pools of sel/isZero/columns not ported, M-POOL)
#[allow(clippy::too_many_arguments)]
pub fn vec_eval_bool(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut crate::eval::RpnState,
    vec_enabled: bool,
    expr_list: &[ExprRef],
    input: &mut tidb_chunk::Chunk,
    selected: Vec<bool>,
    nulls: Vec<bool>,
) -> Result<(Vec<bool>, Vec<bool>), Error> {
    // If input.Sel() != nil, we will call input.SetSel(nil) to clear the sel slice in input chunk.
    // After the function finished, then we reset the input.Sel().
    // The caller will handle the input.Sel() and selected slices.
    let saved = input.sel().map(<[usize]>::to_vec);
    input.set_sel(None);
    let r = vec_eval_bool_inner(ctx, st, rpn, vec_enabled, expr_list, input, selected, nulls);
    input.set_sel(saved);
    r
}

// glue: body of VecEvalBool between Go's SetSel(nil) and the deferred SetSel
#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_range_loop)] // index loops kept in Go's shape
fn vec_eval_bool_inner(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut crate::eval::RpnState,
    vec_enabled: bool,
    expr_list: &[ExprRef],
    input: &mut tidb_chunk::Chunk,
    mut selected: Vec<bool>,
    mut nulls: Vec<bool>,
) -> Result<(Vec<bool>, Vec<bool>), Error> {
    use tidb_parser::types::{ETInt, ETReal};
    let n = input.num_rows();
    selected.clear();
    selected.resize(n, false);
    nulls.clear();
    nulls.resize(n, false);

    let mut sel: Vec<usize> = (0..n).collect();
    input.set_sel(Some(sel.clone()));

    // In isZero slice, -1 means Null, 0 means zero, 1 means not zero
    let mut is_zero = vec![0i8; n];
    let tc = type_ctx(ctx);
    for expr in expr_list {
        let tp = expr.get_type(ctx).into_owned();
        let mut e_type = tp.eval_type();
        if crate::builtin_cast::can_implicit_eval_real(expr) {
            e_type = ETReal;
        }
        let mut buf = crate::eval::rt::empty_column();

        // Take the implicit evalReal path if possible.
        if crate::builtin_cast::can_implicit_eval_real(expr) {
            implicit_eval_real(ctx, st, rpn, vec_enabled, expr, input, &mut buf)?;
        } else {
            eval_expr(ctx, st, rpn, vec_enabled, expr, e_type, input, &mut buf)?;
        }

        to_bool(&tc, &tp, e_type, &buf, &sel, &mut is_zero)?;

        let mut j = 0;
        let is_eq_cond_from_in = is_eq_cond_from_in(expr);
        for i in 0..sel.len() {
            if is_zero[i] == -1 {
                if e_type != ETInt && !is_eq_cond_from_in {
                    continue;
                }
                // In this case, we set this row to null and let it pass this filter.
                // The null flag may be set to false later by other expressions in some cases.
                nulls[sel[i]] = true;
                sel[j] = sel[i];
                j += 1;
                continue;
            }

            if is_zero[i] == 0 {
                continue;
            }
            sel[j] = sel[i]; // this row passes this filter
            j += 1;
        }
        sel.truncate(j);
        input.set_sel(Some(sel.clone()));
    }

    for &i in &sel {
        if !nulls[i] {
            selected[i] = true;
        }
    }

    Ok((selected, nulls))
}

/// toBool: `isZero[i]` of each of the first `len(sel)` rows of `buf` (-1 NULL, 0 zero, 1
/// not zero).
// go: toBool L1
#[allow(clippy::needless_range_loop)] // index loops kept in Go's shape
pub fn to_bool(
    tc: &tidb_types::context::Context,
    tp: &FieldType,
    e_type: tidb_parser::types::EvalType,
    buf: &tidb_chunk::Column,
    sel: &[usize],
    is_zero: &mut [i8],
) -> Result<(), Error> {
    use tidb_parser::types::{
        ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
        ETVectorFloat32,
    };
    let flag = |zero: bool| if zero { 0 } else { 1 };
    match e_type {
        ETInt => {
            let i64s = buf.int64s();
            for i in 0..sel.len() {
                is_zero[i] = if buf.is_null(i) {
                    -1
                } else {
                    flag(i64s[i].get() == 0)
                };
            }
        }
        ETReal => {
            let f64s = buf.float64s();
            for i in 0..sel.len() {
                is_zero[i] = if buf.is_null(i) {
                    -1
                } else {
                    flag(f64s[i].get() == 0.0)
                };
            }
        }
        ETDuration => {
            let d64s = buf.go_durations();
            for i in 0..sel.len() {
                is_zero[i] = if buf.is_null(i) {
                    -1
                } else {
                    flag(d64s[i].get() == 0)
                };
            }
        }
        ETDatetime | ETTimestamp => {
            for i in 0..sel.len() {
                is_zero[i] = if buf.is_null(i) {
                    -1
                } else {
                    flag(buf.get_time(i).is_zero())
                };
            }
        }
        ETString => {
            for i in 0..sel.len() {
                if buf.is_null(i) {
                    is_zero[i] = -1;
                    continue;
                }
                let mut f_val = 0f64;
                let s_val = buf.get_string(i);
                if tp.hybrid() {
                    match tp.get_type() {
                        mysql::TypeSet | mysql::TypeEnum => {
                            f_val = s_val.len() as f64;
                            if f_val == 0.0 {
                                // The elements listed in the column specification are assigned index numbers, beginning
                                // with 1. The index value of the empty string error value (distinguish from a "normal"
                                // empty string) is 0. Thus we need to check whether it's an empty string error value when
                                // `fVal==0`.
                                for (idx, elem) in
                                    tp.get_elems().unwrap_or_default().iter().enumerate()
                                {
                                    if elem.as_slice() == s_val {
                                        f_val = (idx + 1) as f64;
                                        break;
                                    }
                                }
                            }
                        }
                        mysql::TypeBit => {
                            let bl = tidb_types::binary_literal::BinaryLiteral(
                                buf.get_bytes(i).to_vec(),
                            );
                            let (i_val, r) = bl.to_int(tc);
                            r?;
                            f_val = i_val as f64;
                        }
                        _ => {}
                    }
                } else {
                    let (v, r) = tidb_types::convert::str_to_float(tc, s_val, false);
                    r?;
                    f_val = v;
                }
                is_zero[i] = flag(f_val == 0.0);
            }
        }
        ETDecimal => {
            for i in 0..sel.len() {
                is_zero[i] = if buf.is_null(i) {
                    -1
                } else {
                    flag(buf.get_decimal(i).is_zero())
                };
            }
        }
        ETJson => {
            for i in 0..sel.len() {
                is_zero[i] = if buf.is_null(i) {
                    -1
                } else {
                    flag(buf.get_json(i).is_zero())
                };
            }
        }
        ETVectorFloat32 => {
            for i in 0..sel.len() {
                is_zero[i] = if buf.is_null(i) {
                    -1
                } else {
                    flag(buf.get_vector_float32(i).is_zero_value())
                };
            }
        }
        _ => return Err(unsupported_eval_type(e_type)),
    }
    Ok(())
}

/// implicitEvalReal: `EvalExpr` as ETReal for an expression `CanImplicitEvalReal` accepts.
// go: implicitEvalReal L1
pub fn implicit_eval_real(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut crate::eval::RpnState,
    vec_enabled: bool,
    expr: &ExprRef,
    input: &tidb_chunk::Chunk,
    result: &mut tidb_chunk::Column,
) -> Result<(), Error> {
    use tidb_parser::types::ETReal;
    if expr.vectorized() && vec_enabled {
        crate::eval::RpnProgram::compile_as(expr, ETReal).eval(ctx, st, rpn, input, result)
    } else {
        let n = input.num_rows();
        let mut out = crate::eval::rt::RealOut::new(result, n, &[]);
        for ind in 0..n {
            match expr.eval_real(ctx, st, input.get_row(ind))? {
                None => out.set_null(ind),
                Some(v) => out.set(ind, v),
            }
        }
        Ok(())
    }
}

/// EvalExpr evaluates this expr according to its type.
/// And it selects the method for evaluating expression based on
/// the environment variables and whether the expression can be vectorized.
/// Note: the input argument `evalType` is needed because of that when `expr` is
/// of the hybrid type(ENUM/SET/BIT), we need the invoker decide the actual EvalType.
///
/// The vector path runs `expr` compiled for the RPN driver with the root read as `evalType`
/// (Go: `expr.VecEvalXxx`); it is compiled on each call (an executor that evaluates the same
/// expression over many chunks keeps an [`crate::eval::RpnProgram`] instead).
// go: EvalExpr L2 (vector path through RpnProgram::compile_as, N4 §3.4)
#[allow(clippy::too_many_arguments)]
pub fn eval_expr(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut crate::eval::RpnState,
    vec_enabled: bool,
    expr: &ExprRef,
    eval_type: tidb_parser::types::EvalType,
    input: &tidb_chunk::Chunk,
    result: &mut tidb_chunk::Column,
) -> Result<(), Error> {
    use crate::eval::rt::{DecimalOut, DurationOut, IntOut, RealOut, TimeOut};
    use tidb_parser::types::{
        ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
        ETVectorFloat32,
    };
    if expr.vectorized() && vec_enabled {
        return match eval_type {
            ETInt | ETReal | ETDuration | ETDatetime | ETTimestamp | ETString | ETJson
            | ETVectorFloat32 | ETDecimal => crate::eval::RpnProgram::compile_as(expr, eval_type)
                .eval(ctx, st, rpn, input, result),
            _ => Err(unsupported_eval_type(eval_type)),
        };
    }
    let n = input.num_rows();
    let row = |ind: usize| input.get_row(ind);
    match eval_type {
        ETInt => {
            let mut out = IntOut::new(result, n, &[]);
            for ind in 0..n {
                match expr.eval_int(ctx, st, row(ind))? {
                    None => out.set_null(ind),
                    Some(v) => out.set(ind, v),
                }
            }
        }
        ETReal => {
            let mut out = RealOut::new(result, n, &[]);
            for ind in 0..n {
                match expr.eval_real(ctx, st, row(ind))? {
                    None => out.set_null(ind),
                    Some(v) => out.set(ind, v),
                }
            }
        }
        ETDuration => {
            let mut out = DurationOut::new(result, n, &[]);
            for ind in 0..n {
                match expr.eval_duration(ctx, st, row(ind))? {
                    None => out.set_null(ind),
                    Some(v) => out.set(ind, v),
                }
            }
        }
        ETDatetime | ETTimestamp => {
            let mut out = TimeOut::new(result, n, &[]);
            for ind in 0..n {
                match expr.eval_time(ctx, st, row(ind))? {
                    None => out.set_null(ind),
                    Some(v) => out.set(ind, v),
                }
            }
        }
        ETString => {
            result.reserve_string(n);
            for ind in 0..n {
                match expr.eval_string(ctx, st, row(ind))? {
                    None => result.append_null(),
                    Some(v) => result.append_string(&v),
                }
            }
        }
        ETJson => {
            result.reserve_json(n);
            for ind in 0..n {
                match expr.eval_json(ctx, st, row(ind))? {
                    None => result.append_null(),
                    Some(v) => result.append_json(&v),
                }
            }
        }
        ETVectorFloat32 => {
            result.reserve_vector_float32(n);
            for ind in 0..n {
                match expr.eval_vector_float32(ctx, st, row(ind))? {
                    None => result.append_null(),
                    Some(v) => result.append_vector_float32(&v),
                }
            }
        }
        ETDecimal => {
            let mut out = DecimalOut::new(result, n, &[]);
            for ind in 0..n {
                match expr.eval_decimal(ctx, st, row(ind))? {
                    None => out.set_null(ind),
                    Some(v) => out.set(ind, v),
                }
            }
        }
        _ => return Err(unsupported_eval_type(expr.get_type(ctx).eval_type())),
    }
    Ok(())
}

/// composeConditionWithBinaryOp composes condition with binary operator into a balance deep
/// tree, which benefits a lot for pb decoder/encoder. `None` is Go's nil Expression (no
/// conditions, or `NewFunctionInternal` failed).
// go: composeConditionWithBinaryOp L1
fn compose_condition_with_binary_op(
    ctx: &dyn crate::context::BuildContext,
    conditions: &[ExprRef],
    func_name: &str,
) -> Option<ExprRef> {
    let length = conditions.len();
    if length == 0 {
        return None;
    }
    if length == 1 {
        return Some(Arc::clone(&conditions[0]));
    }
    let l = compose_condition_with_binary_op(ctx, &conditions[..length / 2], func_name);
    let r = compose_condition_with_binary_op(ctx, &conditions[length / 2..], func_name);
    // Go passes nil arguments on; a nil argument panics in newFunctionImpl. Both halves are
    // non-empty here, so they are nil only when a nested NewFunctionInternal failed.
    let (l, r) = (l?, r?);
    crate::scalar_function::new_function_internal(
        ctx,
        func_name,
        tidb_types::field_type::new_field_type(mysql::TypeTiny),
        vec![l, r],
    )
}

/// ComposeCNFCondition composes CNF items into a balance deep CNF tree, which benefits a
/// lot for pb decoder/encoder.
// go: ComposeCNFCondition L1
pub fn compose_cnf_condition(
    ctx: &dyn crate::context::BuildContext,
    conditions: &[ExprRef],
) -> Option<ExprRef> {
    compose_condition_with_binary_op(ctx, conditions, ast_func::LogicAnd)
}

/// ComposeDNFCondition composes DNF items into a balance deep DNF tree.
// go: ComposeDNFCondition L1
pub fn compose_dnf_condition(
    ctx: &dyn crate::context::BuildContext,
    conditions: &[ExprRef],
) -> Option<ExprRef> {
    compose_condition_with_binary_op(ctx, conditions, ast_func::LogicOr)
}

// go: extractBinaryOpItems L1
fn extract_binary_op_items(conditions: &ScalarFunction, func_name: &str) -> Vec<ExprRef> {
    let mut ret = Vec::new();
    for arg in conditions.get_args() {
        if let Expr::ScalarFunction(sf) = &**arg
            && sf.func_name.l == func_name
        {
            ret.extend(extract_binary_op_items(sf, func_name));
        } else {
            ret.push(Arc::clone(arg));
        }
    }
    ret
}

/// FlattenDNFConditions extracts DNF expression's leaf item.
/// e.g. or(or(a=1, a=2), or(a=3, a=4)), we'll get [a=1, a=2, a=3, a=4].
// go: FlattenDNFConditions L1
pub fn flatten_dnf_conditions(dnf_condition: &ScalarFunction) -> Vec<ExprRef> {
    extract_binary_op_items(dnf_condition, ast_func::LogicOr)
}

/// FlattenCNFConditions extracts CNF expression's leaf item.
/// e.g. and(and(a>1, a>2), and(a>3, a>4)), we'll get [a>1, a>2, a>3, a>4].
// go: FlattenCNFConditions L1
pub fn flatten_cnf_conditions(cnf_condition: &ScalarFunction) -> Vec<ExprRef> {
    extract_binary_op_items(cnf_condition, ast_func::LogicAnd)
}

/// splitNormalFormItems split CNF(conjunctive normal form) like "a and b and c", or
/// DNF(disjunctive normal form) like "a or b or c"
// go: splitNormalFormItems L1
fn split_normal_form_items(on_expr: &ExprRef, func_name: &str) -> Vec<ExprRef> {
    if let Expr::ScalarFunction(v) = &**on_expr
        && v.func_name.l == func_name
    {
        let mut ret = Vec::new();
        for arg in v.get_args() {
            ret.extend(split_normal_form_items(arg, func_name));
        }
        return ret;
    }
    vec![Arc::clone(on_expr)]
}

/// SplitCNFItems splits CNF items.
/// CNF means conjunctive normal form, e.g. "a and b and c".
// go: SplitCNFItems L1
pub fn split_cnf_items(on_expr: &ExprRef) -> Vec<ExprRef> {
    split_normal_form_items(on_expr, ast_func::LogicAnd)
}

/// SplitDNFItems splits DNF items.
/// DNF means disjunctive normal form, e.g. "a or b or c".
// go: SplitDNFItems L1
pub fn split_dnf_items(on_expr: &ExprRef) -> Vec<ExprRef> {
    split_normal_form_items(on_expr, ast_func::LogicOr)
}
