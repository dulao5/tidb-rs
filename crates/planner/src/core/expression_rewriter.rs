//! go-file: pkg/planner/core/expression_rewriter.go
//!
//! The expression rewriter (D09.2 part 1, VS1-22): an AST expression (a WHERE condition, a
//! select field, an ORDER BY item) becomes a planner expression. Design: D09.2 spec §3.
//!
//! - Go walks the AST with `Accept` (`Enter`, the children, `Leave`); the Rust AST has no
//!   visitor, so [`ExpressionRewriter::visit`] recurses over the children in the order of
//!   Go's `Accept` and runs the `Enter` / `Leave` code around them. Go's `er.err` stops the
//!   walk at the next `Leave`; here the first error is returned with `?` (spec §3 item 1).
//! - `rewrite` takes the schema and output names of the plan instead of the plan (the Rust
//!   logical plans have no output names yet, spec §3 item 2).
//! - The AST-node-keyed maps (`aggMapper`, `windowMapper`, `colMapper`) key by
//!   [`ByAddr`]: the node's identity, borrowed so that a key cannot outlive its node
//!   (spec §3 item 4).
//! - Subqueries are boundary stubs `blocked-by:D09.6` (spec §4).

#![allow(non_upper_case_globals)]

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::plannererrors::{
    ErrAmbiguous, ErrColumnaccessDenied, ErrInvalidGroupFuncUse, ErrTableaccessDenied,
    ErrTooBigPrecision, ErrUnknownColumn, ErrViewInvalid, ErrWindowInvalidWindowFuncUse,
};
use tidb_base::errors::{RedactLogDisable, errorf};
use tidb_base::go_args;
use tidb_base::mysql::{
    self, NotNullFlag, SelectPriv, TypeDatetime, TypeDuration, TypeEnum, TypeJSON, TypeLongBlob,
    TypeNull, TypeSet, TypeString, TypeTiny, TypeUnspecified, TypeVarString, TypeVarchar,
    UnderScoreCharsetFlag, has_not_null_flag,
};
use tidb_chunk::Row;
use tidb_expression::builtin_cast::{
    build_cast_collation_function, build_cast_function, build_cast_function_with_check,
    wrap_with_cast_as_decimal, wrap_with_cast_as_int, wrap_with_cast_as_real,
    wrap_with_cast_as_string, wrap_with_cast_as_time,
};
use tidb_expression::builtin_compare::{
    get_accurate_cmp_type, refine_compared_constant, remove_mutable_const, resolve_type4_between,
};
use tidb_expression::builtin_control::infer_type4_control_funcs;
use tidb_expression::collation::{
    ASCII, Coercibility, CoercibilityExplicit, CoercibilityImplicit, CoercibilityNumeric,
    ExprCollation, Repertoire, UNICODE, check_and_derive_collation_from_exprs,
};
use tidb_expression::constant::{Constant, new_null};
use tidb_expression::constant_fold::maybe_over_optimized4_plan_cache;
use tidb_expression::context::BuildContext;
use tidb_expression::errors::{ErrIncorrectParameterCount, ErrNotSupportedYet, ErrOperandColumns};
use tidb_expression::expression::{
    EvalState, Expr, ExprRef, compose_cnf_condition, compose_dnf_condition,
};
use tidb_expression::function_traits::{
    DisableFoldFunctions, TryFoldFunctions, is_deferred_functions,
};
use tidb_expression::scalar_function::{
    ScalarFunctionCallBack, new_function, new_function_base, new_function_try_fold,
    new_function_with_init,
};
use tidb_expression::schema::Schema;
use tidb_expression::simple_rewriter::{
    column_name_orig_col_name, column_name_string, find_field_name,
};
use tidb_expression::util::{
    check_args_not_multi_column_row, get_func_arg, get_int_from_constant, get_row_len,
    param_marker_expression,
};
use tidb_model::TableInfo;
use tidb_parser::ast::funcs as fnames;
use tidb_parser::ast::{self, Expr as AstExpr, Op};
use tidb_parser::gocharset::{CharsetBin, CharsetUTF8MB4, ErrCollationCharsetMismatch};
use tidb_parser::types::{ETDatetime, ETDecimal, ETDuration, ETInt, ETReal, ETString};
use tidb_types::collate::{get_collation_by_name, new_collation_enabled};
use tidb_types::datum::{GoValue, new_datum, new_int_datum, new_string_datum};
use tidb_types::etc::{FieldType, op_name};
use tidb_types::field_name::{EmptyName, FieldName};
use tidb_types::field_type::{default_type_for_value, new_field_type, set_bin_chs_cln_flag};
use tidb_types::fsp::MaxFsp;
use tidb_types::stringutil::{compile_pattern, is_exact_match};

use crate::arena::{PlanArena, PlanId};
use crate::base::LogicalKind;
use crate::core::logical_plan_builder::append_visit_info;
use crate::core::planbuilder::{PlanBuilder, VisitInfo};
use crate::core::point_get_plan::value_expr_datum;

/// An AST node as the key of Go's pointer-keyed maps (spec §3 item 4): equal and hashed by
/// the node's address, and holding a borrow of the node, so that the borrow checker keeps
/// every keyed node alive as long as the map (Go's map holds the pointer, which keeps the
/// node alive). A key therefore never meets a later node allocated at a freed node's
/// address. Lookups pass the address of a live node ([`node_addr`]).
pub struct ByAddr<'a, T> {
    addr: usize,
    _node: PhantomData<&'a T>,
}

impl<'a, T> ByAddr<'a, T> {
    /// The key of `node`.
    // glue: Go map keyed by an AST node pointer
    pub fn new(node: &'a T) -> ByAddr<'a, T> {
        ByAddr {
            addr: node_addr(node),
            _node: PhantomData,
        }
    }
}

impl<T> Clone for ByAddr<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ByAddr<'_, T> {}

impl<T> PartialEq for ByAddr<'_, T> {
    // glue: pointer equality of Go map keys
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}

impl<T> Eq for ByAddr<'_, T> {}

impl<T> Hash for ByAddr<'_, T> {
    // glue: pointer hash of Go map keys (the same as the address's, see `Borrow`)
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
    }
}

impl<T> Borrow<usize> for ByAddr<'_, T> {
    // glue: lookup of a Go map key by the address of a live node
    fn borrow(&self) -> &usize {
        &self.addr
    }
}

impl<T> std::fmt::Debug for ByAddr<'_, T> {
    // glue: Debug of a map key
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ByAddr({:#x})", self.addr)
    }
}

/// The address of a live AST node: looks a [`ByAddr`] key up.
// glue: Go map keyed by an AST node pointer
pub fn node_addr<T>(n: &T) -> usize {
    n as *const T as usize
}

/// `map[*ast.AggregateFuncExpr]int`: the offset of each aggregate in the plan's schema.
pub type AggMapper<'a> = HashMap<ByAddr<'a, ast::AggregateFuncExpr>, isize>;
/// `map[*ast.WindowFuncExpr]int`.
pub type WindowMapper<'a> = HashMap<ByAddr<'a, ast::WindowFuncExpr>, isize>;

/// clauseCode (planbuilder.go): which part of the query is being processed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ClauseCode {
    #[default]
    UnknowClause,
    FieldList,
    HavingClause,
    OnClause,
    OrderByClause,
    WhereClause,
    GroupByClause,
    ShowStatement,
    GlobalOrderByClause,
    ExpressionClause,
    WindowOrderByClause,
    PartitionByClause,
}

/// clauseMsg (planbuilder.go).
// glue: the clauseMsg map
pub fn clause_msg(c: ClauseCode) -> &'static str {
    match c {
        ClauseCode::UnknowClause => "",
        ClauseCode::FieldList => "field list",
        ClauseCode::HavingClause => "having clause",
        ClauseCode::OnClause => "on clause",
        ClauseCode::OrderByClause => "order clause",
        ClauseCode::WhereClause => "where clause",
        ClauseCode::GroupByClause => "group statement",
        ClauseCode::ShowStatement => "show statement",
        ClauseCode::GlobalOrderByClause => "global ORDER clause",
        ClauseCode::ExpressionClause => "expression",
        ClauseCode::WindowOrderByClause => "window order by",
        ClauseCode::PartitionByClause => "window partition by",
    }
}

/// checkSelectColumnPrivOption (planbuilder.go).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CheckSelectColumnPrivOption {
    #[default]
    NoCheckOption,
    ReportColumnErrOption,
    ReportTableErrOption,
}

/// A visited column for the lazy privilege check (Go builds an `*ast.ColumnName`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisitedColumn {
    /// Schema.L.
    pub schema: String,
    /// Table.L.
    pub table: String,
    /// Name.L.
    pub name: String,
}

/// The plan a rewrite resolves names against: Go's `p` (its `Schema()` and `OutputNames()`,
/// and the plan itself for `findFieldNameFromNaturalUsingJoin`). spec §3 item 2.
#[derive(Clone, Copy)]
pub struct RewritePlan<'a> {
    /// The plan, when the caller has one in the arena (Go's fake TableDual has none).
    pub id: Option<PlanId>,
    /// The arena of `id`.
    pub arena: Option<&'a PlanArena>,
    /// `p.Schema()`.
    pub schema: &'a Schema,
    /// `p.OutputNames()`.
    pub names: &'a [Arc<FieldName>],
}

/// exprRewriterPlanCtx.
///
/// `insertPlan` (INSERT ... ON DUPLICATE KEY UPDATE) and `rollExpand` (ROLLUP) are not
/// here: their branches are stubs (spec §4).
pub struct ExprRewriterPlanCtx<'b, 'ast> {
    /// plan (see [`RewritePlan`]).
    pub plan: Option<PlanId>,
    /// builder.
    pub builder: &'b mut PlanBuilder<'ast>,
    /// curClause tracks which part of the query is being processed
    pub cur_clause: ClauseCode,
    /// aggrMap.
    pub aggr_map: Option<&'b AggMapper<'b>>,
    /// windowMap.
    pub window_map: Option<&'b WindowMapper<'b>>,
    /// colNamesForLazilyPrivilegeCheck pre-collects the visited columns during rewriting, These columns
    /// will be added to PlanBuilder.visitInfo, and be later checked in CheckPrivilege after building plan.
    pub col_names_for_lazily_privilege_check: Vec<VisitedColumn>,
}

/// `map[*ast.ColumnNameExpr]ast.ExprNode`: local AST column-alias replacements for an
/// immutable AST (`havingWindowAndOrderbyExprResolver.Leave` inside aggregates, and
/// `gbyResolver` for `GROUP BY` aliases/positions, D09.4 spec §4.3).
pub type ColReplacements<'a> = HashMap<ByAddr<'a, ast::ColumnName>, &'a AstExpr>;

/// expressionRewriter.
///
/// `ctx` (`context.Context`) is not needed outside subqueries; `preprocess` is the hook of
/// `rewriteWithPreprocess` (a closure over the AST, D09.4's resolver passes it).
pub struct ExpressionRewriter<'a, 'b, 'ast> {
    ctx_stack: Vec<ExprRef>,
    ctx_name_stk: Vec<Arc<FieldName>>,
    schema: &'a Schema,
    names: &'a [Arc<FieldName>],
    arena: Option<&'a PlanArena>,

    sctx: &'a dyn BuildContext,

    /// asScalar indicates the return value must be a scalar value.
    /// NOTE: This value can be changed during expression rewritten.
    as_scalar: bool,
    /// allowBuildCastArray indicates whether allow cast(... as ... array).
    allow_build_cast_array: bool,
    /// sourceTable is only used to build simple expression without all columns from a single table
    source_table: Option<&'a TableInfo>,

    /// disableFoldCounter controls fold-disabled scope. If > 0, rewriter will NOT do constant folding.
    /// Typically, during visiting AST, while entering the scope(disable), the counter will +1; while
    /// leaving the scope(enable again), the counter will -1.
    /// NOTE: This value can be changed during expression rewritten.
    disable_fold_counter: i32,
    try_fold_counter: i32,

    /// Optional local column-alias replacement table for immutable ASTs (D09.4 spec §4.3).
    col_replacements: Option<&'a ColReplacements<'a>>,

    plan_ctx: Option<ExprRewriterPlanCtx<'b, 'ast>>,
}

/// The plan builder's state the rewriter reads (Go reads `b.*` through `planCtx.builder`).
/// `outerSchemas` / `outerNames` exist only inside a subquery (D09.6) and are not here.
impl<'ast> PlanBuilder<'ast> {
    /// rewrite function rewrites ast expr to expression.Expression.
    /// aggMapper maps ast.AggregateFuncExpr to the columns offset in p's output schema.
    /// asScalar means whether this expression must be treated as a scalar expression.
    /// And this function returns a result expression, a new plan that may have apply or semi-join.
    ///
    /// Outside subqueries the plan never changes (spec §3 item 2), so only the expression is
    /// returned (`None` is Go's nil expression).
    // go: PlanBuilder.rewrite L2 (schema and names instead of the plan, spec §3 item 2)
    pub fn rewrite(
        &mut self,
        sctx: &dyn BuildContext,
        expr_node: &AstExpr,
        p: RewritePlan<'_>,
        agg_mapper: Option<&AggMapper<'_>>,
        as_scalar: bool,
    ) -> Result<Option<ExprRef>, Error> {
        self.rewrite_with_preprocess(sctx, expr_node, p, agg_mapper, None, as_scalar)
    }

    /// rewriteWithPreprocess is for handling the situation that we need to adjust the input ast tree
    /// before really using its node in `expressionRewriter.Leave`. In that case, we first call
    /// er.preprocess(expr), which returns a new expr. Then we use the new expr in `Leave`.
    ///
    /// The `preprocess` hook is not here: its only caller (`resolveHavingAndOrderBy`'s
    /// window-function path, D09.4) rewrites the AST before the call instead.
    // go: PlanBuilder.rewriteWithPreprocess L2 (no preprocess hook; no rewriter pool, M-POOL)
    pub fn rewrite_with_preprocess(
        &mut self,
        sctx: &dyn BuildContext,
        expr_node: &AstExpr,
        p: RewritePlan<'_>,
        agg_mapper: Option<&AggMapper<'_>>,
        window_mapper: Option<&WindowMapper<'_>>,
        as_scalar: bool,
    ) -> Result<Option<ExprRef>, Error> {
        // b.rewriterCounter++ / the pool: M-POOL (spec §3 item 3).
        let allow_build_cast_array = self.allow_build_cast_array;
        let mut rewriter = self.get_expression_rewriter(sctx, p);
        if let Some(pc) = rewriter.plan_ctx.as_mut() {
            pc.aggr_map = agg_mapper;
            pc.window_map = window_mapper;
        }
        rewriter.as_scalar = as_scalar;
        rewriter.allow_build_cast_array = allow_build_cast_array;
        rewrite_expr_node(&mut rewriter, expr_node, as_scalar)
    }

    /// Variant of [`PlanBuilder::rewrite`] that applies local AST column-alias replacements
    /// without mutating the AST (`agg_col_replacements` and `gby_col_replacements`, D09.4 §4.3).
    // glue: immutable-AST helper for PlanBuilder.rewrite with local column replacements
    pub fn rewrite_with_col_replacements<'a>(
        &mut self,
        sctx: &'a dyn BuildContext,
        expr_node: &'a AstExpr,
        p: RewritePlan<'a>,
        agg_mapper: Option<&AggMapper<'_>>,
        col_replacements: Option<&'a ColReplacements<'a>>,
        as_scalar: bool,
    ) -> Result<Option<ExprRef>, Error> {
        let allow_build_cast_array = self.allow_build_cast_array;
        let mut rewriter = self.get_expression_rewriter(sctx, p);
        if let Some(pc) = rewriter.plan_ctx.as_mut() {
            pc.aggr_map = agg_mapper;
        }
        rewriter.as_scalar = as_scalar;
        rewriter.allow_build_cast_array = allow_build_cast_array;
        rewriter.col_replacements = col_replacements;
        rewrite_expr_node(&mut rewriter, expr_node, as_scalar)
    }

    /// getExpressionRewriter: a fresh rewriter (Go reuses pooled ones and resets every
    /// field, which is the same thing, M-POOL).
    // go: PlanBuilder.getExpressionRewriter L2 (no pool, M-POOL)
    pub fn get_expression_rewriter<'a, 'b>(
        &'b mut self,
        sctx: &'a dyn BuildContext,
        p: RewritePlan<'a>,
    ) -> ExpressionRewriter<'a, 'b, 'ast>
    where
        'a: 'b,
    {
        let cur_clause = self.cur_clause;
        ExpressionRewriter {
            ctx_stack: Vec::new(),
            ctx_name_stk: Vec::new(),
            schema: p.schema,
            names: p.names,
            arena: p.arena,
            sctx,
            as_scalar: false,
            allow_build_cast_array: false,
            source_table: None,
            disable_fold_counter: 0,
            try_fold_counter: 0,
            col_replacements: None,
            plan_ctx: Some(ExprRewriterPlanCtx {
                plan: p.id,
                builder: self,
                cur_clause,
                aggr_map: None,
                window_map: None,
                col_names_for_lazily_privilege_check: Vec::with_capacity(8),
            }),
        }
    }

    /// appendColNamesToVisitInfo.
    ///
    /// `b.SavedViews` (views, D09.1 `BuildDataSourceFromView`) is always empty here.
    // go: PlanBuilder.appendColNamesToVisitInfo L2 (no saved views: views are not built yet)
    pub fn append_col_names_to_visit_info(&mut self, column_visited: &[VisitedColumn]) {
        let user = self.user_name.as_str();
        let host = self.host_name.as_str();
        let gs = |s: &str| tidb_base::gofmt::Arg::from(s.to_owned());
        match self.check_col_priv {
            CheckSelectColumnPrivOption::ReportTableErrOption => {
                for col_name in column_visited {
                    // R32 (D5 option A): hidden `_tidb_rowid` requires full-table SELECT (`column = ""`).
                    let check_col = if col_name.name == tidb_model::ExtraHandleName.l {
                        ""
                    } else {
                        &col_name.name
                    };
                    let err = ErrTableaccessDenied.fast_gen_by_args(vec![
                        gs("SELECT"),
                        gs(user),
                        gs(host),
                        gs(&col_name.table),
                    ]);
                    append_visit_info(
                        &mut self.visit_info,
                        SelectPriv.0,
                        &col_name.schema,
                        &col_name.table,
                        check_col,
                        Some(err),
                    );
                }
            }
            _ => {
                for col_name in column_visited {
                    // R32 (D5 option A): hidden `_tidb_rowid` requires full-table SELECT (`column = ""`)
                    // while reporting `1143 for column '_tidb_rowid' in table '...'` for partial-column users.
                    let check_col = if col_name.name == tidb_model::ExtraHandleName.l {
                        ""
                    } else {
                        &col_name.name
                    };
                    let err = ErrColumnaccessDenied.fast_gen_by_args(vec![
                        gs("SELECT"),
                        gs(user),
                        gs(host),
                        gs(&col_name.name),
                        gs(&col_name.table),
                    ]);
                    append_visit_info(
                        &mut self.visit_info,
                        SelectPriv.0,
                        &col_name.schema,
                        &col_name.table,
                        check_col,
                        Some(err),
                    );
                }
            }
        }
        let _ = &ErrViewInvalid; // the saved-view branch (see the doc)
    }
}

/// evalAstExprWithPlanCtx evaluates ast expression with plan context.
/// Different with expression.EvalSimpleAst, it uses planner context and is more powerful to build some special expressions
/// like subquery, window function, etc.
// go: evalAstExprWithPlanCtx L1
pub fn eval_ast_expr_with_plan_ctx(
    sctx: &dyn BuildContext,
    expr: &AstExpr,
) -> Result<tidb_types::datum::Datum, Error> {
    if let AstExpr::Value(val) = expr {
        return Ok(value_expr_datum(val));
    }
    let (new_expr, _) = rewrite_ast_expr_with_plan_ctx(sctx, expr, None, &[], false)?;
    new_expr.eval(&*sctx.get_eval_ctx(), &EvalState::new(), Row::default())
}

/// evalAstExpr evaluates ast expression directly (through `buildSimpleExpr`).
// glue: stub of evalAstExpr, blocked-by:D09.2 (part 2: the plan-less rewriter buildSimpleExpr)
pub fn eval_ast_expr() -> Result<(), Error> {
    Err(blocked(
        "D09.2",
        "evalAstExpr (the plan-less rewriter buildSimpleExpr)",
    ))
}

/// buildSimpleExpr: the rewriter without a plan (`ParseSimpleExpr`, the `BuildOption`s;
/// handed over from P1). [`ExpressionRewriter::new_simple`] is its rewriter.
// glue: stub of buildSimpleExpr, blocked-by:D09.2 (part 2: the build options and the source-table columns)
pub fn build_simple_expr() -> Result<(), Error> {
    Err(blocked("D09.2", "buildSimpleExpr (the plan-less rewriter)"))
}

/// The DEFAULT(col) helpers of the rewriter read column default values; DEFAULT() is a
/// boundary stub (spec §4).
// glue: stub of expressionRewriter.evalDefaultExprForTable, blocked-by:D09.15 (column default values)
pub fn eval_default_expr_for_table() -> Result<(), Error> {
    Err(blocked("D09.15", "DEFAULT(col) (column default values)"))
}

// glue: stub of expressionRewriter.evalFieldDefaultValue, blocked-by:D09.15 (column default values)
pub fn eval_field_default_value() -> Result<(), Error> {
    Err(blocked("D09.15", "DEFAULT(col) (column default values)"))
}

// glue: stub of hasCurrentDatetimeDefault, blocked-by:D09.15 (column default values)
pub fn has_current_datetime_default() -> Result<(), Error> {
    Err(blocked("D09.15", "DEFAULT(col) (column default values)"))
}

/// rewriteAstExprWithPlanCtx rewrites ast expression directly.
/// Different with expression.BuildSimpleExpr, it uses planner context and is more powerful to build some special expressions
/// like subquery, window function, etc.
///
/// The fake plan is a TableDual with `schema` / `names` (spec §3 item 2); the builder is
/// fresh (`NewPlanBuilder().Init`), its clause is `expressionClause` and column privileges
/// are reported per column. Returns the expression and the builder's visit infos.
// go: rewriteAstExprWithPlanCtx L2 (the hint handler and saved block names are D13.3)
pub fn rewrite_ast_expr_with_plan_ctx(
    sctx: &dyn BuildContext,
    expr: &AstExpr,
    schema: Option<&Schema>,
    names: &[Arc<FieldName>],
    allow_cast_array: bool,
) -> Result<(ExprRef, Vec<VisitInfo>), Error> {
    let mut b = PlanBuilder {
        allow_build_cast_array: allow_cast_array,
        cur_clause: ClauseCode::ExpressionClause,
        check_col_priv: CheckSelectColumnPrivOption::ReportColumnErrOption,
        ..PlanBuilder::default()
    };
    let empty = Schema::default();
    let (schema, names) = match schema {
        Some(s) => (s, names),
        None => (&empty, &[][..]),
    };
    let p = RewritePlan {
        id: None,
        arena: None,
        schema,
        names,
    };
    let new_expr = b.rewrite(sctx, expr, p, None, true)?;
    let new_expr = new_expr.expect("asScalar rewrite returns an expression");
    Ok((new_expr, b.visit_info))
}

/// rewriteExprNode.
///
/// The deferred reset of the plan's output names only matters after a subquery added
/// columns to the plan (spec §3 item 2): not ported.
// go: rewriteExprNode L2 (no output-name reset: only subqueries add columns, spec §3)
pub fn rewrite_expr_node(
    rewriter: &mut ExpressionRewriter<'_, '_, '_>,
    expr_node: &AstExpr,
    as_scalar: bool,
) -> Result<Option<ExprRef>, Error> {
    let res = rewriter.visit(expr_node);
    if let Some(pc) = rewriter.plan_ctx.as_mut()
        && !pc.col_names_for_lazily_privilege_check.is_empty()
        && pc.builder.check_col_priv != CheckSelectColumnPrivOption::NoCheckOption
    {
        let cols = std::mem::take(&mut pc.col_names_for_lazily_privilege_check);
        pc.builder.append_col_names_to_visit_info(&cols);
    }
    res?;

    if !as_scalar && rewriter.ctx_stack.is_empty() {
        return Ok(None);
    }
    if rewriter.ctx_stack.len() != 1 {
        return Err(go_errorf(
            "context len %v is invalid",
            go_args![rewriter.ctx_stack.len() as i64],
        ));
    }
    check_args_not_multi_column_row(&rewriter.ctx_stack[..1])?;
    Ok(Some(Arc::clone(&rewriter.ctx_stack[0])))
}

/// Go's `%T` of the AST node for the error texts.
// glue: fmt %T of an ast node
fn node_type_name(e: &AstExpr) -> &'static str {
    match e {
        AstExpr::Column(_) => "*ast.ColumnNameExpr",
        AstExpr::Value(_) => "*driver.ValueExpr",
        AstExpr::ParamMarker(_) => "*driver.ParamMarkerExpr",
        AstExpr::Binary(_) => "*ast.BinaryOperationExpr",
        AstExpr::Unary(_) => "*ast.UnaryOperationExpr",
        AstExpr::FuncCall(_) => "*ast.FuncCallExpr",
        AstExpr::FuncCast(_) => "*ast.FuncCastExpr",
        AstExpr::JSONSumCrc32(_) => "*ast.JSONSumCrc32Expr",
        AstExpr::Aggregate(_) => "*ast.AggregateFuncExpr",
        AstExpr::Window(_) => "*ast.WindowFuncExpr",
        AstExpr::Paren(_) => "*ast.ParenthesesExpr",
        AstExpr::PatternIn(_) => "*ast.PatternInExpr",
        AstExpr::PatternLike(_) => "*ast.PatternLikeOrIlikeExpr",
        AstExpr::PatternRegexp(_) => "*ast.PatternRegexpExpr",
        AstExpr::Between(_) => "*ast.BetweenExpr",
        AstExpr::IsNull(_) => "*ast.IsNullExpr",
        AstExpr::IsTruth(_) => "*ast.IsTruthExpr",
        AstExpr::Case(_) => "*ast.CaseExpr",
        AstExpr::Subquery(_) => "*ast.SubqueryExpr",
        AstExpr::Exists(_) => "*ast.ExistsSubqueryExpr",
        AstExpr::CompareSubquery(_) => "*ast.CompareSubqueryExpr",
        AstExpr::Variable(_) => "*ast.VariableExpr",
        AstExpr::Row(_) => "*ast.RowExpr",
        AstExpr::SetCollation(_) => "*ast.SetCollationExpr",
        AstExpr::Default(_) => "*ast.DefaultExpr",
        AstExpr::Values(_) => "*ast.ValuesExpr",
        AstExpr::MatchAgainst(_) => "*ast.MatchAgainst",
        AstExpr::TableName(_) => "*ast.TableName",
        AstExpr::TimeUnit(_) => "*ast.TimeUnitExpr",
        AstExpr::GetFormatSelector(_) => "*ast.GetFormatSelectorExpr",
        AstExpr::TrimDirection(_) => "*ast.TrimDirectionExpr",
        AstExpr::Position(_) => "*ast.PositionExpr",
        AstExpr::Unported(_) => "ast.Node",
    }
}

/// `errors.Errorf(format, args...)`.
// glue: errors.Errorf
fn go_errorf(format: &str, args: Vec<tidb_base::gofmt::Arg>) -> Error {
    errorf(format, &args)
}

// glue: Go's map membership test on DisableFoldFunctions / TryFoldFunctions
fn in_set(set: &[&str], name: &str) -> bool {
    set.contains(&name)
}

/// `expr.SetCoercibility(c)` on any expression (`Coercibility` is atomic, as Go's write
/// through the shared pointer).
// glue: Expression.SetCoercibility
fn set_coercibility(e: &Expr, c: Coercibility) {
    match e {
        Expr::Column(v) => v.collation_info.set_coercibility(c),
        Expr::CorrelatedColumn(v) => v.column.collation_info.set_coercibility(c),
        Expr::Constant(v) => v.collation_info.set_coercibility(c),
        Expr::ScalarFunction(v) => v.set_coercibility(c),
    }
}

/// `expr.SetRepertoire(r)`: the node the rewriter just built (only the stack holds it, so
/// `make_mut` does not copy, spec §3 item 9).
// glue: Expression.SetRepertoire
fn set_repertoire(e: &mut ExprRef, r: Repertoire) {
    match Arc::make_mut(e) {
        Expr::Column(v) => v.collation_info.set_repertoire(r),
        Expr::CorrelatedColumn(v) => v.column.collation_info.set_repertoire(r),
        Expr::Constant(v) => v.collation_info.set_repertoire(r),
        Expr::ScalarFunction(v) => v.set_repertoire(r),
    }
}

/// `expr.SetCharsetAndCollation(chs, coll)` (see [`set_repertoire`]).
// glue: Expression.SetCharsetAndCollation
fn set_charset_and_collation(e: &mut ExprRef, chs: &str, coll: &str) {
    match Arc::make_mut(e) {
        Expr::Column(v) => v.collation_info.set_charset_and_collation(chs, coll),
        Expr::CorrelatedColumn(v) => v.column.collation_info.set_charset_and_collation(chs, coll),
        Expr::Constant(v) => v.collation_info.set_charset_and_collation(chs, coll),
        Expr::ScalarFunction(v) => v.set_charset_and_collation(chs, coll),
    }
}

/// initConstantRepertoire.
// go: initConstantRepertoire L1
fn init_constant_repertoire(ctx: &dyn tidb_expression::context::EvalContext, c: &mut Constant) {
    c.collation_info.set_repertoire(ASCII);
    if c.ret_type.eval_type() == ETString {
        let _ = ctx;
        for b in c.value.get_bytes() {
            // if any character in constant is not ascii, set the repertoire to UNICODE.
            if *b >= 0x80 {
                c.collation_info.set_repertoire(UNICODE);
                break;
            }
        }
    }
}

/// The literal's Go value (`driver.ValueExpr` holds it as `Datum`, the parser built it from
/// this value), for `DefaultTypeForValue` (spec §3 item 7).
// glue: the value the parser passes to ast.NewValueExpr
pub(crate) fn value_expr_go_value(v: &ast::ValueExpr) -> GoValue {
    use tidb_types::binary_literal::{BitLiteral, HexLiteral};
    match &v.datum {
        ast::Datum::Null => GoValue::Nil,
        ast::Datum::Bool(b) => GoValue::Bool(*b),
        ast::Datum::Int(i) => GoValue::Int64(*i),
        ast::Datum::Uint(u) => GoValue::Uint64(*u),
        ast::Datum::Float(f) => GoValue::Float64(*f),
        ast::Datum::Decimal(s) => {
            let mut dec = tidb_types::mydecimal::MyDecimal::default();
            let _ = dec.from_string(s.as_bytes());
            GoValue::Decimal(Box::new(dec))
        }
        ast::Datum::Str(s) => GoValue::String(s.as_bytes().to_vec()),
        ast::Datum::Hex(b) => GoValue::HexLiteral(HexLiteral(b.clone())),
        ast::Datum::Bit(b) => GoValue::BitLiteral(BitLiteral(b.clone())),
    }
}

/// The `types.FieldType` of a CAST target (the Rust AST keeps its own copy of the fields the
/// parser sets).
// glue: ast.FuncCastExpr.Tp (the Rust AST's FieldType stand-in, D02.2)
fn cast_target_type(tp: &ast::FieldType) -> Result<FieldType, Error> {
    if let Some(kind) = tp.unported {
        return Err(blocked("D02.2", &format!("the CAST target type {kind}")));
    }
    let mut ft = FieldType::default();
    ft.set_type(tp.tp);
    ft.set_flag(tp.flag as usize);
    ft.set_flen(tp.flen as isize);
    ft.set_decimal(tp.decimal as isize);
    ft.set_charset(&tp.charset);
    ft.set_collate(&tp.collate);
    ft.set_array(tp.array);
    Ok(ft)
}

impl<'a, 'b, 'ast> ExpressionRewriter<'a, 'b, 'ast> {
    /// A rewriter without a plan context (`buildSimpleExpr`): only the schema and names.
    // glue: the expressionRewriter literal of buildSimpleExpr
    pub fn new_simple(
        sctx: &'a dyn BuildContext,
        schema: &'a Schema,
        names: &'a [Arc<FieldName>],
        source_table: Option<&'a TableInfo>,
        allow_cast_array: bool,
    ) -> ExpressionRewriter<'a, 'b, 'ast> {
        ExpressionRewriter {
            ctx_stack: Vec::new(),
            ctx_name_stk: Vec::new(),
            schema,
            names,
            arena: None,
            sctx,
            as_scalar: true,
            allow_build_cast_array: allow_cast_array,
            source_table,
            disable_fold_counter: 0,
            try_fold_counter: 0,
            col_replacements: None,
            plan_ctx: None,
        }
    }

    // go: expressionRewriter.ctxStackLen L1
    fn ctx_stack_len(&self) -> usize {
        self.ctx_stack.len()
    }

    // go: expressionRewriter.ctxStackPop L1
    fn ctx_stack_pop(&mut self, num: usize) {
        let l = self.ctx_stack_len();
        self.ctx_stack.truncate(l - num);
        self.ctx_name_stk.truncate(l - num);
    }

    // go: expressionRewriter.ctxStackAppend L1
    fn ctx_stack_append(&mut self, col: ExprRef, name: Arc<FieldName>) {
        self.ctx_stack.push(col);
        self.ctx_name_stk.push(name);
    }

    // glue: er.sctx.GetEvalCtx()
    fn eval_ctx(&self) -> tidb_sessionctx::exprctx::EvalContextRef {
        self.sctx.get_eval_ctx()
    }

    /// constructBinaryOpFunction converts binary operator functions
    ///
    /// ```text
    /// The algorithm is as follows:
    /// 1. If the length of the two sides of the expression is 1, return l op r directly.
    /// 2. If the length of the two sides of the expression is not equal, return an error.
    /// 3. If the operator is EQ, NE, or NullEQ, converts (a0,a1,a2) op (b0,b1,b2) to (a0 op b0) and (a1 op b1) and (a2 op b2)
    /// 4. If the operator is not EQ, NE, or NullEQ,
    ///    converts (a0,a1,a2) op (b0,b1,b2) to (a0 > b0) or (a0 = b0 and a1 > b1) or (a0 = b0 and a1 = b1 and a2 op b2)
    ///    Especially, op is GE or LE, the prefix element will be converted to > or <.
    ///    converts (a0,a1,a2) >= (b0,b1,b2) to (a0 > b0) or (a0 = b0 and a1 > b1) or (a0 = b0 and a1 = b1 and a2 >= b2)
    ///    The only different between >= and > is that >= additional include the (x,y,z) = (a,b,c).
    /// ```
    // go: expressionRewriter.constructBinaryOpFunction L1
    fn construct_binary_op_function(
        &mut self,
        l: &ExprRef,
        r: &ExprRef,
        op: &str,
    ) -> Result<ExprRef, Error> {
        let (l_len, r_len) = (get_row_len(l), get_row_len(r));
        if l_len == 1 && r_len == 1 {
            return self.new_function(
                op,
                new_field_type(TypeTiny),
                vec![Arc::clone(l), Arc::clone(r)],
            );
        } else if r_len != l_len {
            return Err(ErrOperandColumns.gen_with_stack_by_args(go_args![l_len as i64]));
        }
        let arg = |e: &ExprRef, i: usize| get_func_arg(e, i).expect("row function");
        match op {
            fnames::EQ | fnames::NE | fnames::NullEQ => {
                let mut funcs = Vec::with_capacity(l_len);
                for i in 0..l_len {
                    funcs.push(self.construct_binary_op_function(&arg(l, i), &arg(r, i), op)?);
                }
                if op == fnames::NE {
                    return Ok(compose_dnf_condition(self.sctx, &funcs).expect("non-empty"));
                }
                Ok(compose_cnf_condition(self.sctx, &funcs).expect("non-empty"))
            }
            _ => {
                // The algorithm is as follows:
                // 1. Iterate over i left columns and construct his own CNF for each left column.
                //    1.1 Iterate over j (every i-1 columns) to l[j]=r[j]
                //    1.2 Build current i column with op to l[i] op r[i]
                //    1.3 Combine 1.1 and 1.2 predicates with AND operator
                // 2. Combine every i CNF with OR operator.
                let mut result_dnf_list = Vec::with_capacity(l_len);
                // Step 1
                for i in 0..l_len {
                    let mut expr_list = Vec::with_capacity(i + 1);
                    // Step 1.1 build prefix equal conditions
                    // (l[0], ... , l[i-1], ...) op (r[0], ... , r[i-1], ...) should be convert to
                    // l[0] = r[0] and l[1] = r[1] and ... and l[i-1] = r[i-1]
                    for j in 0..i {
                        let j_expr =
                            self.construct_binary_op_function(&arg(l, j), &arg(r, j), fnames::EQ)?;
                        expr_list.push(j_expr);
                    }

                    // Especially, op is GE or LE, the prefix element will be converted to > or <.
                    let mut degenerated_op = op;
                    if i < l_len - 1 {
                        match op {
                            fnames::GE => degenerated_op = fnames::GT,
                            fnames::LE => degenerated_op = fnames::LT,
                            _ => {}
                        }
                    }
                    // Step 1.2
                    let current_index_expr =
                        self.construct_binary_op_function(&arg(l, i), &arg(r, i), degenerated_op)?;
                    expr_list.push(current_index_expr);
                    // Step 1.3
                    let current_expr =
                        compose_cnf_condition(self.sctx, &expr_list).expect("non-empty");
                    result_dnf_list.push(current_expr);
                }
                // Step 2
                Ok(compose_dnf_condition(self.sctx, &result_dnf_list).expect("non-empty"))
            }
        }
    }

    /// requirePlanCtx.
    // go: expressionRewriter.requirePlanCtx L1
    fn require_plan_ctx(&self, in_node: &AstExpr, detail: &str) -> Result<(), Error> {
        if self.plan_ctx.is_none() {
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(", {detail}")
            };
            return Err(go_errorf(
                "planCtx is required when rewriting node: '%s'%s",
                go_args![node_type_name(in_node), detail],
            ));
        }
        Ok(())
    }

    /// One AST node: Go's `Accept` with this rewriter as the visitor (`Enter`, the children
    /// in `Accept`'s order unless `Enter` skips them, `Leave`).
    // glue: ast.Node.Accept (the Rust AST has no visitor, spec §3 item 1)
    pub fn visit(&mut self, node: &AstExpr) -> Result<(), Error> {
        if let AstExpr::Column(v) = node
            && let Some(repl) = self
                .col_replacements
                .and_then(|m| m.get(&node_addr(&**v)).copied())
        {
            let saved = self.col_replacements.take();
            let res = self.visit(repl);
            self.col_replacements = saved;
            return res;
        }
        let skip = self.enter(node)?;
        if !skip {
            self.visit_children(node)?;
        }
        self.leave(node)
    }

    /// The children of `node` in the order of its Go `Accept`.
    // glue: the child order of each ast node's Accept
    fn visit_children(&mut self, node: &AstExpr) -> Result<(), Error> {
        match node {
            AstExpr::Column(c) => self.visit_column_name(c),
            AstExpr::Binary(b) => {
                self.visit(&b.l)?;
                self.visit(&b.r)
            }
            AstExpr::Unary(u) => self.visit(&u.v),
            AstExpr::FuncCall(f) => f.args.iter().try_for_each(|a| self.visit(a)),
            AstExpr::FuncCast(c) => self.visit(&c.expr),
            AstExpr::JSONSumCrc32(c) => self.visit(&c.expr),
            AstExpr::Aggregate(a) => a.args.iter().try_for_each(|x| self.visit(x)),
            AstExpr::Window(w) => w.args.iter().try_for_each(|x| self.visit(x)),
            AstExpr::Paren(p) => self.visit(p),
            AstExpr::PatternIn(p) => {
                self.visit(&p.expr)?;
                p.list.iter().try_for_each(|x| self.visit(x))
            }
            AstExpr::PatternLike(p) => {
                self.visit(&p.expr)?;
                self.visit(&p.pattern)
            }
            AstExpr::PatternRegexp(p) => {
                self.visit(&p.expr)?;
                self.visit(&p.pattern)
            }
            AstExpr::Between(b) => {
                self.visit(&b.expr)?;
                self.visit(&b.left)?;
                self.visit(&b.right)
            }
            AstExpr::IsNull(x) => self.visit(&x.expr),
            AstExpr::IsTruth(x) => self.visit(&x.expr),
            AstExpr::Case(c) => {
                if let Some(v) = &c.value {
                    self.visit(v)?;
                }
                for w in &c.when_clauses {
                    // WhenClause: Enter's default branch, then Expr and Result; Leave does nothing.
                    self.as_scalar = true;
                    self.visit(&w.expr)?;
                    self.visit(&w.result)?;
                }
                if let Some(e) = &c.else_clause {
                    self.visit(e)?;
                }
                Ok(())
            }
            AstExpr::Variable(v) => match &v.value {
                Some(x) => self.visit(x),
                None => Ok(()),
            },
            AstExpr::Row(r) => r.values.iter().try_for_each(|x| self.visit(x)),
            AstExpr::SetCollation(s) => self.visit(&s.expr),
            // DefaultExpr.Accept does not visit its name.
            AstExpr::Default(_) => Ok(()),
            AstExpr::MatchAgainst(m) => {
                for c in &m.column_names {
                    self.visit_column_name(c)?;
                }
                self.visit(&m.against)
            }
            // Leaves; the subquery nodes are stubs in `enter`.
            AstExpr::Value(_)
            | AstExpr::ParamMarker(_)
            | AstExpr::Subquery(_)
            | AstExpr::Exists(_)
            | AstExpr::CompareSubquery(_)
            | AstExpr::Values(_)
            | AstExpr::TableName(_)
            | AstExpr::TimeUnit(_)
            | AstExpr::GetFormatSelector(_)
            | AstExpr::TrimDirection(_)
            | AstExpr::Position(_)
            | AstExpr::Unported(_) => Ok(()),
        }
    }

    /// `*ast.ColumnName` as a node: `Enter`'s default branch, no children, `Leave` →
    /// `toColumn`.
    // glue: ast.ColumnName.Accept
    fn visit_column_name(&mut self, c: &ast::ColumnName) -> Result<(), Error> {
        self.as_scalar = true;
        self.to_column(c)
    }

    /// Enter implements Visitor interface. Returns Go's `skipChildren`.
    // go: expressionRewriter.Enter L2 (subqueries are blocked-by:D09.6; INSERT VALUES() is blocked-by:D09.15)
    fn enter(&mut self, in_node: &AstExpr) -> Result<bool, Error> {
        match in_node {
            AstExpr::Aggregate(v) => {
                self.require_plan_ctx(in_node, "")?;
                let pc = self.plan_ctx.as_ref().expect("plan ctx");
                let (mut index, mut ok) = (-1isize, false);
                if let Some(m) = pc.aggr_map
                    && let Some(i) = m.get(&node_addr(&**v))
                {
                    (index, ok) = (*i, true);
                }
                if ok {
                    // index < 0 indicates this is a correlated aggregate belonging to outer query,
                    // for which a correlated column will be created later, so we append a null constant
                    // as a temporary result expression.
                    if index < 0 {
                        self.ctx_stack_append(
                            Expr::Constant(new_null()).into_ref(),
                            EmptyName.clone(),
                        );
                    } else {
                        // index >= 0 indicates this is a regular aggregate column
                        let i = index as usize;
                        let col = Expr::Column((*self.schema.columns[i]).clone()).into_ref();
                        self.ctx_stack_append(col, Arc::clone(&self.names[i]));
                    }
                    return Ok(true);
                }
                // replace correlated aggregate in sub-query with its corresponding correlated column:
                // b.correlatedAggMapper is only filled inside a subquery (D09.6), empty here.
                Err(ErrInvalidGroupFuncUse.clone())
            }
            AstExpr::Column(v) => {
                if let Some(pc) = self.plan_ctx.as_ref()
                    && let Some(index) = pc.builder.col_mapper.get(&node_addr(&**v))
                {
                    let i = *index;
                    let col = Expr::Column((*self.schema.columns[i]).clone()).into_ref();
                    self.ctx_stack_append(col, Arc::clone(&self.names[i]));
                    return Ok(true);
                }
                Ok(false)
            }
            AstExpr::CompareSubquery(_) | AstExpr::Exists(_) | AstExpr::Subquery(_) => {
                self.require_plan_ctx(in_node, "")?;
                // glue: blocked-by:D09.6 (subqueries build a sub-plan joined to the outer plan)
                Err(blocked(
                    "D09.6",
                    "a subquery in an expression (it builds a sub-plan joined to the outer plan)",
                ))
            }
            AstExpr::PatternIn(v) => {
                if v.sel.is_some() {
                    self.require_plan_ctx(in_node, "")?;
                    // glue: blocked-by:D09.6 (IN subquery)
                    return Err(blocked(
                        "D09.6",
                        "an IN subquery (it builds a sub-plan joined to the outer plan)",
                    ));
                }
                if v.list.len() != 1 {
                    // IN-list operands are scalar expression children. A nested IN-subquery on the left
                    // must therefore append its boolean result for this parent IN expression.
                    self.as_scalar = true;
                    return Ok(false);
                }
                // For 10 in ((select * from t)), the parser won't set v.Sel.
                // So we must process this case here.
                let mut x = &v.list[0];
                loop {
                    match x {
                        AstExpr::Subquery(_) => {
                            self.require_plan_ctx(in_node, "")?;
                            // glue: blocked-by:D09.6 (IN subquery)
                            return Err(blocked(
                                "D09.6",
                                "an IN subquery (it builds a sub-plan joined to the outer plan)",
                            ));
                        }
                        AstExpr::Paren(y) => x = y,
                        _ => return Ok(false),
                    }
                }
            }
            AstExpr::Paren(_) => Ok(false),
            AstExpr::Values(_) => {
                self.require_plan_ctx(in_node, "")?;
                // glue: blocked-by:D09.15 (VALUES(col) is INSERT ... ON DUPLICATE KEY UPDATE's)
                Err(blocked(
                    "D09.15",
                    "VALUES(col) (NewValuesFunc, used by INSERT ... ON DUPLICATE KEY UPDATE)",
                ))
            }
            AstExpr::Window(v) => {
                self.require_plan_ctx(in_node, "")?;
                let pc = self.plan_ctx.as_ref().expect("plan ctx");
                let index = pc.window_map.and_then(|m| m.get(&node_addr(&**v)).copied());
                let Some(index) = index else {
                    return Err(ErrWindowInvalidWindowFuncUse
                        .gen_with_stack_by_args(go_args![v.name.to_lowercase()]));
                };
                let i = index as usize;
                let col = Expr::Column((*self.schema.columns[i]).clone()).into_ref();
                self.ctx_stack_append(col, Arc::clone(&self.names[i]));
                Ok(true)
            }
            AstExpr::FuncCall(v) => {
                self.as_scalar = true;
                let l = v.fn_name.l();
                if in_set(DisableFoldFunctions, &l) {
                    self.disable_fold_counter += 1;
                }
                if in_set(TryFoldFunctions, &l) {
                    self.try_fold_counter += 1;
                }
                Ok(false)
            }
            AstExpr::Case(_) => {
                self.as_scalar = true;
                if in_set(DisableFoldFunctions, "case") {
                    self.disable_fold_counter += 1;
                }
                if in_set(TryFoldFunctions, "case") {
                    self.try_fold_counter += 1;
                }
                Ok(false)
            }
            AstExpr::Binary(v) => {
                self.as_scalar = true;
                if v.op == Op::LogicAnd || v.op == Op::LogicOr {
                    self.try_fold_counter += 1;
                }
                Ok(false)
            }
            AstExpr::SetCollation(_) => {
                // Do nothing
                Ok(false)
            }
            _ => {
                self.as_scalar = true;
                Ok(false)
            }
        }
    }

    /// adjustUTF8MB4Collation.
    // go: expressionRewriter.adjustUTF8MB4Collation L1
    fn adjust_utf8mb4_collation(&self, tp: &mut FieldType) {
        if tp.get_flag() & UnderScoreCharsetFlag > 0 && CharsetUTF8MB4 == tp.get_charset() {
            tp.set_collate(&self.sctx.get_default_collation_for_utf8mb4());
        }
    }

    /// Leave implements Visitor interface.
    // go: expressionRewriter.Leave L2 (variables, DEFAULT(), GROUPING() and ORDER BY ? are stubs, spec §4)
    fn leave(&mut self, in_node: &AstExpr) -> Result<(), Error> {
        match in_node {
            AstExpr::Aggregate(_) => {
                // Table-read SELECT privilege is registered unconditionally at DataSource and
                // PointGet boundaries (specs/D04.1-privilege/spec.md §5.2 item 1, §5.3 item 2,
                // ruling R29), superseding Go's case-sensitive `collectPrivsForCount` patch.
            }
            AstExpr::Column(_)
            | AstExpr::Paren(_)
            | AstExpr::Subquery(_)
            | AstExpr::Exists(_)
            | AstExpr::CompareSubquery(_)
            | AstExpr::Values(_)
            | AstExpr::Window(_) => {}
            AstExpr::Value(v) => {
                // set right not null flag for constant value
                let go_val = value_expr_go_value(v);
                let mut ret_type = FieldType::default();
                default_type_for_value(&go_val, &mut ret_type, v.charset, v.collate);
                if v.underscore_charset {
                    // The parser's `UNDERSCORE_CHARSET` productions (`tp.SetCharset` /
                    // `SetCollate` / `AddFlag` after `NewValueExpr`).
                    ret_type.set_charset(v.charset);
                    ret_type.set_collate(v.collate);
                    ret_type.add_flag(UnderScoreCharsetFlag);
                    if ret_type.get_collate() == tidb_parser::gocharset::CollationBin {
                        ret_type.add_flag(mysql::BinaryFlag);
                    }
                }
                let mut datum = value_expr_datum(v);
                if datum.is_null() {
                    ret_type.del_flag(NotNullFlag);
                } else {
                    ret_type.add_flag(NotNullFlag);
                }
                let val = datum.get_value();
                datum.set_value(val, &ret_type);
                let mut value = Constant::new(datum, ret_type);
                init_constant_repertoire(&*self.eval_ctx(), &mut value);
                // Go adjusts `retType` after the constant took it by pointer: the constant's
                // type changes too.
                let mut tp = std::mem::take(&mut value.ret_type);
                self.adjust_utf8mb4_collation(&mut tp);
                value.ret_type = tp;
                self.ctx_stack_append(Expr::Constant(value).into_ref(), EmptyName.clone());
            }
            AstExpr::ParamMarker(v) => self.to_param_marker(v)?,
            AstExpr::Variable(v) => {
                if v.is_system {
                    self.require_plan_ctx(
                        in_node,
                        "accessing system variable requires plan context",
                    )?;
                    // glue: stub of expressionRewriter.rewriteSystemVariable, blocked-by:D09.2 (part 2: the rewriter does not get the session variables yet)
                    return Err(blocked(
                        "D09.2",
                        "a system variable in an expression (the rewriter does not get the session variables yet)",
                    ));
                }
                // glue: stub of expressionRewriter.rewriteUserVariable, blocked-by:D08.9 (getvar / setvar functions)
                return Err(blocked(
                    "D08.9",
                    "a user variable in an expression (the getvar / setvar functions)",
                ));
            }
            AstExpr::FuncCall(v) => {
                let l = v.fn_name.l();
                if l == fnames::Grouping {
                    self.require_plan_ctx(in_node, "grouping function requires plan context")?;
                    // glue: stub of expressionRewriter.funcCallToExpressionWithPlanCtx, blocked-by:D09.4 (ROLLUP's Expand)
                    return Err(blocked(
                        "D09.4",
                        "GROUPING() (it needs the Expand operator of ROLLUP)",
                    ));
                }
                if in_set(TryFoldFunctions, &l) {
                    self.try_fold_counter -= 1;
                }
                self.func_call_to_expression(v)?;
                if in_set(DisableFoldFunctions, &l) {
                    self.disable_fold_counter -= 1;
                }
            }
            AstExpr::TableName(v) => self.to_table(v),
            AstExpr::Unary(v) => self.unary_op_to_expression(v)?,
            AstExpr::Binary(v) => {
                if v.op == Op::LogicAnd || v.op == Op::LogicOr {
                    self.try_fold_counter -= 1;
                }
                self.binary_op_to_expression(v)?;
            }
            AstExpr::Between(v) => self.between_to_expression(v)?,
            AstExpr::Case(v) => {
                if in_set(TryFoldFunctions, "case") {
                    self.try_fold_counter -= 1;
                }
                self.case_to_expression(v)?;
                if in_set(DisableFoldFunctions, "case") {
                    self.disable_fold_counter -= 1;
                }
            }
            AstExpr::FuncCast(v) => {
                let tp = cast_target_type(&v.tp)?;
                if tp.is_array() && !self.allow_build_cast_array {
                    return Err(ErrNotSupportedYet.gen_with_stack_by_args(go_args![
                        "Use of CAST( .. AS .. ARRAY) outside of functional index in CREATE(non-SELECT)/ALTER TABLE or in general expressions"
                    ]));
                }
                let arg = Arc::clone(self.ctx_stack.last().expect("cast argument"));
                check_args_not_multi_column_row(std::slice::from_ref(&arg))?;

                // check the decimal precision of "CAST(AS TIME)".
                self.check_time_precision(&tp)?;

                let tp_eval_type = tp.eval_type();
                let tp_charset = tp.get_charset().to_owned();
                let mut cast_function =
                    build_cast_function_with_check(self.sctx, arg, tp, false, v.explicit_char_set)?;
                if tp_eval_type == ETString {
                    set_coercibility(&cast_function, CoercibilityImplicit);
                    if tp_charset == tidb_parser::gocharset::CharsetASCII {
                        set_repertoire(&mut cast_function, ASCII);
                    } else {
                        set_repertoire(&mut cast_function, UNICODE);
                    }
                } else {
                    set_coercibility(&cast_function, CoercibilityNumeric);
                    set_repertoire(&mut cast_function, ASCII);
                }

                let n = self.ctx_stack.len();
                self.ctx_stack[n - 1] = cast_function;
                self.ctx_name_stk[n - 1] = EmptyName.clone();
            }
            AstExpr::PatternLike(v) => self.pattern_like_or_ilike_to_expression(v)?,
            AstExpr::PatternRegexp(v) => self.regexp_to_scalar_func(v)?,
            AstExpr::Row(v) => self.row_to_scalar_func(v)?,
            AstExpr::PatternIn(v) => {
                if v.sel.is_none() {
                    self.in_to_expression(v.list.len(), v.not)?;
                }
            }
            AstExpr::Position(n) => {
                self.require_plan_ctx(in_node, "")?;
                self.position_to_scalar_func(*n)?;
            }
            AstExpr::IsNull(v) => self.is_null_to_expression(v)?,
            AstExpr::IsTruth(v) => self.is_true_to_scalar_func(v)?,
            AstExpr::Default(_) => {
                if self.plan_ctx.is_none() && self.source_table.is_none() {
                    return Err(go_errorf(
                        "Unsupported expr %s when source table not provided",
                        go_args!["*ast.DefaultExpr"],
                    ));
                }
                // glue: stub of expressionRewriter.evalDefaultExprWithPlanCtx, blocked-by:D09.15 (column default values)
                return Err(blocked(
                    "D09.15",
                    "DEFAULT(col) in an expression (reading a column's default value)",
                ));
            }
            // TODO: Perhaps we don't need to transcode these back to generic integers/strings
            AstExpr::TrimDirection(d) => {
                let c = Constant::new(new_int_datum(*d as i64), new_field_type(TypeTiny));
                self.ctx_stack_append(Expr::Constant(c).into_ref(), EmptyName.clone());
            }
            AstExpr::TimeUnit(u) => {
                let c = Constant::new(new_string_datum(u.as_str()), new_field_type(TypeVarchar));
                self.ctx_stack_append(Expr::Constant(c).into_ref(), EmptyName.clone());
            }
            AstExpr::GetFormatSelector(s) => {
                let c = Constant::new(new_string_datum(s.as_str()), new_field_type(TypeVarchar));
                self.ctx_stack_append(Expr::Constant(c).into_ref(), EmptyName.clone());
            }
            AstExpr::SetCollation(v) => self.set_collation(v)?,
            AstExpr::Unported(_) => {
                // glue: blocked-by:D02.2 (an AST node the Rust parser does not model)
                return Err(blocked(
                    "D02.2",
                    "an expression node the parser does not model yet",
                ));
            }
            AstExpr::JSONSumCrc32(_) | AstExpr::MatchAgainst(_) => {
                return Err(go_errorf(
                    "UnknownType: %s",
                    go_args![node_type_name(in_node)],
                ));
            }
        }
        Ok(())
    }

    /// The `*ast.SetCollationExpr` branch of `Leave`.
    // glue: a branch of expressionRewriter.Leave
    fn set_collation(&mut self, v: &ast::SetCollationExpr) -> Result<(), Error> {
        let ectx = self.eval_ctx();
        let n = self.ctx_stack.len();
        let mut arg = Arc::clone(&self.ctx_stack[n - 1]);
        let arg_tp = arg.get_type(&*ectx).into_owned();
        if new_collation_enabled() {
            // TODO(bb7133): use charset.ValidCharsetAndCollation when its bug is fixed.
            let coll_info = get_collation_by_name(&v.collate)?;
            let mut chs = arg_tp.get_charset().to_owned();
            // if the field is json, the charset is always utf8mb4.
            if arg_tp.get_type() == TypeJSON {
                chs = mysql::UTF8MB4Charset.to_owned();
            }
            if !chs.is_empty() && coll_info.charset_name != chs {
                return Err(ErrCollationCharsetMismatch
                    .gen_with_stack_by_args(go_args![coll_info.name.clone(), chs]));
            }
        }
        // SetCollationExpr sets the collation explicitly, even when the evaluation type of the expression is non-string.
        if matches!(&*arg, Expr::Column(_)) || arg_tp.get_type() == TypeJSON {
            if arg_tp.get_type() == TypeEnum || arg_tp.get_type() == TypeSet {
                return Err(tidb_base::dbterror::plannererrors::ErrNotSupportedYet
                    .gen_with_stack_by_args(go_args!["use collate clause for enum or set"]));
            }
            // Wrap a cast here to avoid changing the original FieldType of the column expression.
            let mut expr_type = arg_tp.clone();
            // if arg type is json, we should cast it to longtext if there is collate clause.
            if arg_tp.get_type() == TypeJSON {
                expr_type = new_field_type(TypeLongBlob);
                expr_type.set_charset(mysql::UTF8MB4Charset);
            }
            expr_type.set_collate(&v.collate);
            let casted = build_cast_function(self.sctx, arg, expr_type)?;
            arg = Arc::clone(&casted);
            self.ctx_stack_pop(1);
            self.ctx_stack_append(casted, EmptyName.clone());
        } else {
            // For constant and scalar function, we can set its collate directly.
            // Go writes through the shared pointer; the node is the rewriter's own
            // (spec §3 item 9). A constant from a parameter marker keeps its type in Go's
            // `GetType` too (`with_type_mut` skips it).
            let n = self.ctx_stack.len();
            let collate = v.collate.clone();
            set_type_collate(&mut self.ctx_stack[n - 1], &collate);
            arg = Arc::clone(&self.ctx_stack[n - 1]);
        }
        let n = self.ctx_stack.len();
        set_coercibility(&self.ctx_stack[n - 1], CoercibilityExplicit);
        let arg_tp = arg.get_type(&*ectx).into_owned();
        set_charset_and_collation(
            &mut self.ctx_stack[n - 1],
            arg_tp.get_charset(),
            arg_tp.get_collate(),
        );
        Ok(())
    }

    /// newFunctionWithInit chooses which expression.NewFunctionImpl() will be used.
    ///
    /// `BuiltinFunctionUsageInc` is telemetry (spec §5 item 2).
    // go: expressionRewriter.newFunctionWithInit L2 (no builtin function usage telemetry)
    fn new_function_with_init(
        &mut self,
        func_name: &str,
        ret_type: FieldType,
        init: Option<ScalarFunctionCallBack>,
        args: Vec<ExprRef>,
    ) -> Result<ExprRef, Error> {
        if init.is_some() {
            new_function_with_init(self.sctx, func_name, ret_type, init, args)
        } else if self.disable_fold_counter > 0 {
            new_function_base(self.sctx, func_name, ret_type, args)
        } else if self.try_fold_counter > 0 {
            new_function_try_fold(self.sctx, func_name, ret_type, args)
        } else {
            new_function(self.sctx, func_name, ret_type, args)
        }
    }

    /// newFunction is being redirected to newFunctionWithInit.
    // go: expressionRewriter.newFunction L1
    fn new_function(
        &mut self,
        func_name: &str,
        ret_type: FieldType,
        args: Vec<ExprRef>,
    ) -> Result<ExprRef, Error> {
        self.new_function_with_init(func_name, ret_type, None, args)
    }

    // go: expressionRewriter.checkTimePrecision L1
    fn check_time_precision(&self, ft: &FieldType) -> Result<(), Error> {
        if ft.eval_type() == ETDuration && ft.get_decimal() > MaxFsp {
            return Err(ErrTooBigPrecision.gen_with_stack_by_args(go_args![
                ft.get_decimal() as i64,
                "CAST",
                MaxFsp as i64
            ]));
        }
        Ok(())
    }

    // go: expressionRewriter.useCache L1
    fn use_cache(&self) -> bool {
        self.sctx.is_use_cache()
    }

    // go: expressionRewriter.unaryOpToExpression L1
    fn unary_op_to_expression(&mut self, v: &ast::UnaryOperationExpr) -> Result<(), Error> {
        let stk_len = self.ctx_stack.len();
        let op = match v.op {
            Op::Plus => {
                // expression (+ a) is equal to a
                return Ok(());
            }
            Op::Minus => fnames::UnaryMinus,
            Op::BitNeg => fnames::BitNeg,
            Op::Not | Op::Not2 => fnames::UnaryNot,
            _ => {
                return Err(go_errorf("Unknown Unary Op %s", go_args!["opcode.Op"]));
            }
        };
        if get_row_len(&self.ctx_stack[stk_len - 1]) != 1 {
            return Err(ErrOperandColumns.gen_with_stack_by_args(go_args![1i64]));
        }
        let arg = Arc::clone(&self.ctx_stack[stk_len - 1]);
        self.ctx_stack[stk_len - 1] = self.new_function(op, FieldType::default(), vec![arg])?;
        self.ctx_name_stk[stk_len - 1] = EmptyName.clone();
        Ok(())
    }

    // go: expressionRewriter.binaryOpToExpression L1
    fn binary_op_to_expression(&mut self, v: &ast::BinaryOperationExpr) -> Result<(), Error> {
        let stk_len = self.ctx_stack.len();
        let l = Arc::clone(&self.ctx_stack[stk_len - 2]);
        let r = Arc::clone(&self.ctx_stack[stk_len - 1]);
        let function = match v.op {
            Op::EQ | Op::NE | Op::NullEQ | Op::GT | Op::GE | Op::LT | Op::LE => {
                self.construct_binary_op_function(&l, &r, op_name(v.op))?
            }
            _ => {
                let l_len = get_row_len(&l);
                let r_len = get_row_len(&r);
                if l_len != 1 || r_len != 1 {
                    return Err(ErrOperandColumns.gen_with_stack_by_args(go_args![1i64]));
                }
                self.new_function(op_name(v.op), new_field_type(TypeUnspecified), vec![l, r])?
            }
        };
        self.ctx_stack_pop(2);
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    // go: expressionRewriter.notToExpression L1
    fn not_to_expression(
        &mut self,
        has_not: bool,
        op: &str,
        tp: FieldType,
        args: Vec<ExprRef>,
    ) -> Result<ExprRef, Error> {
        let op_func = self.new_function(op, tp.clone(), args)?;
        if !has_not {
            return Ok(op_func);
        }
        self.new_function(fnames::UnaryNot, tp, vec![op_func])
    }

    // go: expressionRewriter.isNullToExpression L1
    fn is_null_to_expression(&mut self, v: &ast::IsNullExpr) -> Result<(), Error> {
        let stk_len = self.ctx_stack.len();
        if get_row_len(&self.ctx_stack[stk_len - 1]) != 1 {
            return Err(ErrOperandColumns.gen_with_stack_by_args(go_args![1i64]));
        }
        let arg = Arc::clone(&self.ctx_stack[stk_len - 1]);
        let function =
            self.not_to_expression(v.not, fnames::IsNull, FieldType::default(), vec![arg])?;
        self.ctx_stack_pop(1);
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    /// positionToScalarFunc. The Rust AST's position node has only the number (no
    /// parameter-marker form, spec §4).
    // go: expressionRewriter.positionToScalarFunc L2 (ORDER BY ? is not in the Rust AST, blocked-by:D02.2)
    fn position_to_scalar_func(&mut self, n: i64) -> Result<(), Error> {
        let pos = n as isize;
        let str_ = n.to_string();
        let _ = get_int_from_constant;
        let pc = self.plan_ctx.as_ref().expect("plan ctx");
        if pos > 0
            && (pos as usize) <= self.schema.columns.len()
            && !self.schema.columns[pos as usize - 1].is_hidden
        {
            let i = pos as usize - 1;
            let col = Expr::Column((*self.schema.columns[i]).clone()).into_ref();
            self.ctx_stack_append(col, Arc::clone(&self.names[i]));
            Ok(())
        } else {
            Err(ErrUnknownColumn
                .gen_with_stack_by_args(go_args![str_, clause_msg(pc.builder.cur_clause)]))
        }
    }

    // go: expressionRewriter.isTrueToScalarFunc L1
    fn is_true_to_scalar_func(&mut self, v: &ast::IsTruthExpr) -> Result<(), Error> {
        let stk_len = self.ctx_stack.len();
        let mut op = fnames::IsTruthWithoutNull;
        if v.true_ == 0 {
            op = fnames::IsFalsity;
        }
        if get_row_len(&self.ctx_stack[stk_len - 1]) != 1 {
            return Err(ErrOperandColumns.gen_with_stack_by_args(go_args![1i64]));
        }
        let arg = Arc::clone(&self.ctx_stack[stk_len - 1]);
        let function = self.not_to_expression(v.not, op, FieldType::default(), vec![arg])?;
        self.ctx_stack_pop(1);
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    /// inToExpression converts in expression to a scalar function. The argument lLen means the length of in list.
    /// The argument not means if the expression is not in. The tp stands for the expression type, which is always bool.
    /// a in (b, c, d) will be rewritten as `(a = b) or (a = c) or (a = d)`.
    ///
    /// Go's `args` aliases the top of `ctxStack`: the refined constants are written into
    /// the stack, as here.
    // go: expressionRewriter.inToExpression L1
    fn in_to_expression(&mut self, l_len: usize, not: bool) -> Result<(), Error> {
        let tp = FieldType::default();
        let ectx = self.eval_ctx();
        let stk_len = self.ctx_stack.len();
        let base = stk_len - l_len - 1;
        let l = get_row_len(&self.ctx_stack[base]);
        for i in 0..l_len {
            if l != get_row_len(&self.ctx_stack[stk_len - l_len + i]) {
                return Err(ErrOperandColumns.gen_with_stack_by_args(go_args![l as i64]));
            }
        }
        let left_ft = self.ctx_stack[base].get_type(&*ectx).into_owned();
        let (left_et, left_is_null) = (left_ft.eval_type(), left_ft.get_type() == TypeNull);
        if left_is_null {
            self.ctx_stack_pop(l_len + 1);
            self.ctx_stack_append(Expr::Constant(new_null()).into_ref(), EmptyName.clone());
            return Ok(());
        }
        if left_et == ETInt {
            for i in base + 1..stk_len {
                if !matches!(&*self.ctx_stack[i], Expr::Constant(_)) {
                    continue;
                }
                if maybe_over_optimized4_plan_cache(
                    self.sctx,
                    std::slice::from_ref(&self.ctx_stack[i]),
                ) {
                    if self.ctx_stack[i].get_type(&*ectx).eval_type() == ETInt {
                        continue; // no need to refine it
                    }
                    let c_str = self.ctx_stack[i].string_with_ctx(&*ectx, RedactLogDisable);
                    self.sctx.set_skip_plan_cache(&format!(
                        "'{}' may be converted to INT",
                        String::from_utf8_lossy(&c_str)
                    ));
                    remove_mutable_const(self.sctx, std::slice::from_mut(&mut self.ctx_stack[i]))?;
                }
                let Expr::Constant(c) = &*self.ctx_stack[i] else {
                    continue;
                };
                let (refined, is_exceptional) =
                    refine_compared_constant(self.sctx, left_ft.clone(), c, Op::EQ);
                if !is_exceptional {
                    self.ctx_stack[i] = Expr::Constant(refined).into_ref();
                }
            }
        }
        let mut all_same_type = true;
        for i in base + 1..stk_len {
            let arg = &self.ctx_stack[i];
            if arg.get_type(&*ectx).get_type() != TypeNull
                && get_accurate_cmp_type(&*ectx, &self.ctx_stack[base], arg) != left_et
            {
                all_same_type = false;
                break;
            }
        }
        let function = if all_same_type && l == 1 && l_len > 1 {
            let args = self.ctx_stack[base..].to_vec();
            self.not_to_expression(not, fnames::In, tp, args)?
        } else {
            // If we rewrite IN to EQ, we need to decide what's the collation EQ uses.
            let coll = self.derive_collation_for_in(l, l_len, base)?;
            self.cast_collation_for_in(l, l_len, stk_len, coll.as_ref())?;
            let mut eq_functions = Vec::with_capacity(l_len);
            let left = Arc::clone(&self.ctx_stack[base]);
            for i in stk_len - l_len..stk_len {
                let right = Arc::clone(&self.ctx_stack[i]);
                let expr = self.construct_binary_op_function(&left, &right, fnames::EQ)?;
                eq_functions.push(expr);
            }
            let mut f = compose_dnf_condition(self.sctx, &eq_functions).expect("non-empty");
            if not {
                f = self.new_function(fnames::UnaryNot, tp, vec![f])?;
            }
            f
        };
        self.ctx_stack_pop(l_len + 1);
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    /// deriveCollationForIn derives collation for in expression.
    /// We don't handle the cases if the element is a tuple, such as (a, b, c) in ((x1, y1, z1), (x2, y2, z2)).
    // go: expressionRewriter.deriveCollationForIn L1
    fn derive_collation_for_in(
        &self,
        col_len: usize,
        _elem_cnt: usize,
        base: usize,
    ) -> Result<Option<ExprCollation>, Error> {
        if col_len == 1 {
            // a in (x, y, z) => coll[0]
            let coll2 = check_and_derive_collation_from_exprs(
                self.sctx,
                "IN",
                ETInt,
                &self.ctx_stack[base..],
            )?;
            return Ok(Some(coll2));
        }
        Ok(None)
    }

    /// castCollationForIn casts collation info for arguments in the `in clause` to make sure the used collation is correct after we
    /// rewrite it to equal expression.
    // go: expressionRewriter.castCollationForIn L1
    fn cast_collation_for_in(
        &mut self,
        col_len: usize,
        elem_cnt: usize,
        stk_len: usize,
        coll: Option<&ExprCollation>,
    ) -> Result<(), Error> {
        // We don't handle the cases if the element is a tuple, such as (a, b, c) in ((x1, y1, z1), (x2, y2, z2)).
        if col_len != 1 {
            return Ok(());
        }
        if !new_collation_enabled() {
            // This function will apply CoercibilityExplicit to the casted expression, but some checks(during ColumnSubstituteImpl) is missed when the new
            // collation is disabled, then lead to panic.
            // To work around this issue, we can skip the function, it should be good since the collation is disabled.
            return Ok(());
        }
        let coll = coll.expect("derived for one column");
        let ectx = self.eval_ctx();
        for i in stk_len - elem_cnt..stk_len {
            // todo: consider refining the code and reusing expression.BuildCollationFunction here
            let ft = self.ctx_stack[i].get_type(&*ectx).into_owned();
            if ft.eval_type() == ETString {
                if let Expr::ScalarFunction(f) = &*self.ctx_stack[i]
                    && f.func_name.l == fnames::RowFunc
                {
                    continue;
                }
                // Don't convert it if it's charset is binary. So that we don't convert 0x12 to a string.
                if ft.get_collate() == coll.collation {
                    continue;
                }
                let mut tp = ft.clone();
                if ft.hybrid() {
                    if get_accurate_cmp_type(
                        &*ectx,
                        &self.ctx_stack[stk_len - elem_cnt - 1],
                        &self.ctx_stack[i],
                    ) != ETString
                    {
                        continue;
                    }
                    tp = new_field_type(TypeVarString);
                } else if coll.charset == CharsetBin {
                    // When cast character string to binary string, if we still use fixed length representation,
                    // then 0 padding will be used, which can affect later execution.
                    // On the other hand, we can not directly return origin expr back,
                    // since we need binary collation to do string comparison later.
                    // Here we use VarString type of cast, i.e `cast(a as binary)`, to avoid this problem.
                    tp.set_type(TypeVarString);
                }
                tp.set_charset(&coll.charset);
                tp.set_collate(&coll.collation);
                let arg = Arc::clone(&self.ctx_stack[i]);
                self.ctx_stack[i] = build_cast_function(self.sctx, arg, tp)?;
                set_coercibility(&self.ctx_stack[i], CoercibilityExplicit);
            }
        }
        Ok(())
    }

    // go: expressionRewriter.caseToExpression L1
    fn case_to_expression(&mut self, v: &ast::CaseExpr) -> Result<(), Error> {
        let stk_len = self.ctx_stack.len();
        let mut args_len = 2 * v.when_clauses.len();
        if v.else_clause.is_some() {
            args_len += 1;
        }
        check_args_not_multi_column_row(&self.ctx_stack[stk_len - args_len..])?;

        // value                          -> ctxStack[stkLen-argsLen-1]
        // when clause(condition, result) -> ctxStack[stkLen-argsLen:stkLen-1];
        // else clause                    -> ctxStack[stkLen-1]
        let args: Vec<ExprRef>;
        if v.value.is_some() {
            // args:  eq scalar func(args: value, condition1), result1,
            //        eq scalar func(args: value, condition2), result2,
            //        ...
            //        else clause
            let value = Arc::clone(&self.ctx_stack[stk_len - args_len - 1]);
            let mut a = Vec::with_capacity(args_len);
            let mut i = stk_len - args_len;
            while i + 1 < stk_len {
                let cond = Arc::clone(&self.ctx_stack[i]);
                let arg = self.new_function(
                    fnames::EQ,
                    new_field_type(TypeTiny),
                    vec![Arc::clone(&value), cond],
                )?;
                a.push(arg);
                a.push(Arc::clone(&self.ctx_stack[i + 1]));
                i += 2;
            }
            if v.else_clause.is_some() {
                a.push(Arc::clone(&self.ctx_stack[stk_len - 1]));
            }
            args = a;
            args_len += 1; // for trimming the value element later
        } else {
            // args:  condition1, result1,
            //        condition2, result2,
            //        ...
            //        else clause
            args = self.ctx_stack[stk_len - args_len..].to_vec();
        }
        let function = self.new_function(fnames::Case, FieldType::default(), args)?;
        self.ctx_stack_pop(args_len);
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    // go: expressionRewriter.patternLikeOrIlikeToExpression L1
    fn pattern_like_or_ilike_to_expression(
        &mut self,
        v: &ast::PatternLikeOrIlikeExpr,
    ) -> Result<(), Error> {
        let l = self.ctx_stack.len();
        check_args_not_multi_column_row(&self.ctx_stack[l - 2..])?;

        let (chs, col) = self.sctx.get_charset_info();
        let ectx = self.eval_ctx();
        let mut function: Option<ExprRef> = None;
        let mut field_type = FieldType::default();
        let mut is_pattern_exact_match = false;
        // Treat predicate 'like' or 'ilike' the same way as predicate '=' when it is an exact match and new collation is not enabled.
        if let Expr::Constant(pat_expression) = &*self.ctx_stack[l - 1]
            && !new_collation_enabled()
        {
            let pat_string =
                pat_expression.eval_string(&*ectx, &EvalState::new(), Row::default())?;
            if let Some(pat_string) = pat_string {
                let (pat_value, pat_types) = compile_pattern(&pat_string, v.escape);
                if is_exact_match(&pat_types)
                    && self.ctx_stack[l - 2].get_type(&*ectx).eval_type() == ETString
                {
                    let op = if v.not { fnames::NE } else { fnames::EQ };
                    let pat: String = pat_value.iter().collect();
                    default_type_for_value(
                        &GoValue::String(pat.as_bytes().to_vec()),
                        &mut field_type,
                        &chs,
                        &col,
                    );
                    let lhs = Arc::clone(&self.ctx_stack[l - 2]);
                    let c = Expr::Constant(Constant::new(
                        new_string_datum(pat.into_bytes()),
                        field_type.clone(),
                    ))
                    .into_ref();
                    function = Some(self.construct_binary_op_function(&lhs, &c, op)?);
                    is_pattern_exact_match = true;
                }
            }
        }
        if !is_pattern_exact_match {
            let func_name = if v.is_like {
                fnames::Like
            } else {
                fnames::Ilike
            };
            default_type_for_value(
                &GoValue::Int(i64::from(v.escape)),
                &mut field_type,
                &chs,
                &col,
            );
            let args = vec![
                Arc::clone(&self.ctx_stack[l - 2]),
                Arc::clone(&self.ctx_stack[l - 1]),
                Expr::Constant(Constant::new(
                    new_int_datum(i64::from(v.escape)),
                    field_type,
                ))
                .into_ref(),
            ];
            function =
                Some(self.not_to_expression(v.not, func_name, FieldType::default(), args)?);
        }

        self.ctx_stack_pop(2);
        self.ctx_stack_append(function.expect("built"), EmptyName.clone());
        Ok(())
    }

    // go: expressionRewriter.regexpToScalarFunc L1
    fn regexp_to_scalar_func(&mut self, v: &ast::PatternRegexpExpr) -> Result<(), Error> {
        let l = self.ctx_stack.len();
        check_args_not_multi_column_row(&self.ctx_stack[l - 2..])?;
        let args = self.ctx_stack[l - 2..].to_vec();
        let function = self.not_to_expression(v.not, fnames::Regexp, FieldType::default(), args)?;
        self.ctx_stack_pop(2);
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    // go: expressionRewriter.rowToScalarFunc L1
    fn row_to_scalar_func(&mut self, v: &ast::RowExpr) -> Result<(), Error> {
        let stk_len = self.ctx_stack.len();
        let length = v.values.len();
        let rows: Vec<ExprRef> = self.ctx_stack[stk_len - length..].to_vec();
        self.ctx_stack_pop(length);
        let tp = rows[0].get_type(&*self.eval_ctx()).into_owned();
        let function = self.new_function(fnames::RowFunc, tp, rows)?;
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    // go: expressionRewriter.wrapExpWithCast L1
    fn wrap_exp_with_cast(&self) -> Result<(ExprRef, ExprRef, ExprRef), Error> {
        let stk_len = self.ctx_stack.len();
        let (mut expr, mut lexp, mut rexp) = (
            Arc::clone(&self.ctx_stack[stk_len - 3]),
            Arc::clone(&self.ctx_stack[stk_len - 2]),
            Arc::clone(&self.ctx_stack[stk_len - 1]),
        );
        let ectx = self.eval_ctx();
        let et = resolve_type4_between(&*ectx, [&*expr, &*lexp, &*rexp]);
        let cast_func: fn(&dyn BuildContext, ExprRef) -> Result<ExprRef, Error> = match et {
            ETInt => wrap_with_cast_as_int,
            ETReal => wrap_with_cast_as_real,
            ETDecimal => wrap_with_cast_as_decimal,
            ETString => {
                let f = |ctx: &dyn BuildContext, e: ExprRef| -> Result<ExprRef, Error> {
                    // string kind expression do not need cast
                    if e.get_type(&*ctx.get_eval_ctx())
                        .eval_type()
                        .is_string_kind()
                    {
                        return Ok(e);
                    }
                    wrap_with_cast_as_string(ctx, e)
                };
                expr = f(self.sctx, expr)?;
                lexp = f(self.sctx, lexp)?;
                rexp = f(self.sctx, rexp)?;
                return Ok((expr, lexp, rexp));
            }
            ETDuration => {
                expr = wrap_with_cast_as_time(self.sctx, expr, new_field_type(TypeDuration))?;
                lexp = wrap_with_cast_as_time(self.sctx, lexp, new_field_type(TypeDuration))?;
                rexp = wrap_with_cast_as_time(self.sctx, rexp, new_field_type(TypeDuration))?;
                return Ok((expr, lexp, rexp));
            }
            ETDatetime => {
                expr = wrap_with_cast_as_time(self.sctx, expr, new_field_type(TypeDatetime))?;
                lexp = wrap_with_cast_as_time(self.sctx, lexp, new_field_type(TypeDatetime))?;
                rexp = wrap_with_cast_as_time(self.sctx, rexp, new_field_type(TypeDatetime))?;
                return Ok((expr, lexp, rexp));
            }
            _ => return Ok((expr, lexp, rexp)),
        };

        expr = cast_func(self.sctx, expr)?;
        lexp = cast_func(self.sctx, lexp)?;
        rexp = cast_func(self.sctx, rexp)?;
        Ok((expr, lexp, rexp))
    }

    // go: expressionRewriter.betweenToExpression L1
    fn between_to_expression(&mut self, v: &ast::BetweenExpr) -> Result<(), Error> {
        let stk_len = self.ctx_stack.len();
        check_args_not_multi_column_row(&self.ctx_stack[stk_len - 3..])?;

        let (mut expr, mut lexp, mut rexp) = self.wrap_exp_with_cast()?;

        let coll = check_and_derive_collation_from_exprs(
            self.sctx,
            "BETWEEN",
            ETInt,
            &[Arc::clone(&expr), Arc::clone(&lexp), Arc::clone(&rexp)],
        )?;

        // Handle enum or set. We need to know their real type to decide whether to cast them.
        let ectx = self.eval_ctx();
        let lt = get_accurate_cmp_type(&*ectx, &expr, &lexp);
        let rt = get_accurate_cmp_type(&*ectx, &expr, &rexp);
        let enum_or_set_real_type_is_str = lt != ETInt && rt != ETInt;

        expr = build_cast_collation_function(self.sctx, expr, &coll, enum_or_set_real_type_is_str)?;
        lexp = build_cast_collation_function(self.sctx, lexp, &coll, enum_or_set_real_type_is_str)?;
        rexp = build_cast_collation_function(self.sctx, rexp, &coll, enum_or_set_real_type_is_str)?;

        let l = new_function(
            self.sctx,
            fnames::GE,
            FieldType::default(),
            vec![Arc::clone(&expr), lexp],
        )?;
        let r = new_function(
            self.sctx,
            fnames::LE,
            FieldType::default(),
            vec![expr, rexp],
        )?;
        let mut function = self.new_function(fnames::LogicAnd, FieldType::default(), vec![l, r])?;
        if v.not {
            function = self.new_function(fnames::UnaryNot, FieldType::default(), vec![function])?;
        }
        self.ctx_stack_pop(3);
        self.ctx_stack_append(function, EmptyName.clone());
        Ok(())
    }

    /// rewriteFuncCall handles a FuncCallExpr and generates a customized function.
    /// It should return true if for the given FuncCallExpr a rewrite is performed so that original behavior is skipped.
    /// Otherwise it should return false to indicate (the caller) that original behavior needs to be performed.
    // go: expressionRewriter.rewriteFuncCall L1
    fn rewrite_func_call(&mut self, v: &ast::FuncCallExpr) -> Result<bool, Error> {
        let ectx = self.eval_ctx();
        match v.fn_name.l().as_str() {
            // when column is not null, ifnull on such column can be optimized to a cast.
            fnames::Ifnull => {
                if v.args.len() != 2 {
                    return Err(ErrIncorrectParameterCount
                        .gen_with_stack_by_args(go_args![v.fn_name.o.clone()]));
                }
                let stack_len = self.ctx_stack.len();
                let lhs = Arc::clone(&self.ctx_stack[stack_len - 2]);
                let rhs = Arc::clone(&self.ctx_stack[stack_len - 1]);
                let lhs_tp = lhs.get_type(&*ectx).into_owned();
                let rhs_tp = rhs.get_type(&*ectx).into_owned();
                let is_enum_set = lhs_tp.get_type() == TypeEnum || lhs_tp.get_type() == TypeSet;
                // if expr1 is a column with not null flag, then we can optimize it as a cast.
                if let Expr::Column(col) = &*lhs
                    && !is_enum_set
                    && has_not_null_flag(col.ret_type.get_flag())
                {
                    let mut ret_tp = infer_type4_control_funcs(
                        self.sctx,
                        fnames::Ifnull,
                        &[Arc::clone(&lhs), Arc::clone(&rhs)],
                    )?;
                    ret_tp.add_flag(
                        (lhs_tp.get_flag() & NotNullFlag) | (rhs_tp.get_flag() & NotNullFlag),
                    );
                    if lhs_tp.get_type() == TypeNull && rhs_tp.get_type() == TypeNull {
                        ret_tp.set_type(TypeNull);
                        ret_tp.set_flen(0);
                        ret_tp.set_decimal(0);
                        set_bin_chs_cln_flag(&mut ret_tp);
                    }
                    self.ctx_stack_pop(v.args.len());
                    let casted = build_cast_function(self.sctx, lhs, ret_tp)?;
                    self.ctx_stack_append(casted, EmptyName.clone());
                    return Ok(true);
                }

                Ok(false)
            }
            fnames::Nullif => {
                if v.args.len() != 2 {
                    return Err(ErrIncorrectParameterCount
                        .gen_with_stack_by_args(go_args![v.fn_name.o.clone()]));
                }
                let stack_len = self.ctx_stack.len();
                let param1 = Arc::clone(&self.ctx_stack[stack_len - 2]);
                let param2 = Arc::clone(&self.ctx_stack[stack_len - 1]);
                // param1 = param2
                let func_compare =
                    self.construct_binary_op_function(&param1, &param2, fnames::EQ)?;
                // NULL
                let mut null_tp = new_field_type(TypeNull);
                let (flen, decimal) = mysql::get_default_field_length_and_decimal(TypeNull);
                null_tp.set_flen(flen);
                null_tp.set_decimal(decimal);
                let param_null =
                    Expr::Constant(Constant::new(new_datum(GoValue::Nil), null_tp)).into_ref();
                // if(param1 = param2, NULL, param1)
                let func_if = self.new_function(
                    fnames::If,
                    FieldType::default(),
                    vec![func_compare, param_null, param1],
                )?;
                self.ctx_stack_pop(v.args.len());
                self.ctx_stack_append(func_if, EmptyName.clone());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    // go: expressionRewriter.funcCallToExpression L1
    fn func_call_to_expression(&mut self, v: &ast::FuncCallExpr) -> Result<(), Error> {
        let stack_len = self.ctx_stack.len();
        let args: Vec<ExprRef> = self.ctx_stack[stack_len - v.args.len()..].to_vec();
        check_args_not_multi_column_row(&args)?;

        if self.rewrite_func_call(v)? {
            return Ok(());
        }

        let fn_l = v.fn_name.l();
        self.ctx_stack_pop(v.args.len());
        let ok = is_deferred_functions(self.sctx, &fn_l);
        if self.use_cache() && ok {
            // When the expression is unix_timestamp and the number of argument is not zero,
            // we deal with it as normal expression.
            if fn_l == fnames::UnixTimestamp && !v.args.is_empty() {
                let function = self.new_function(&fn_l, FieldType::default(), args)?;
                self.ctx_stack_append(function, EmptyName.clone());
            } else {
                let function = new_function_base(self.sctx, &fn_l, FieldType::default(), args)?;
                let tp = function.get_type(&*self.eval_ctx()).into_owned();
                let mut c = Constant::new(new_datum(GoValue::Nil), tp);
                c.deferred_expr = Some(function);
                self.ctx_stack_append(Expr::Constant(c).into_ref(), EmptyName.clone());
            }
        } else {
            let function = self.new_function(&fn_l, FieldType::default(), args)?;
            self.ctx_stack_append(function, EmptyName.clone());
        }
        Ok(())
    }

    // go-exception: R29 expressionRewriter.collectPrivsForCount (replaced by single-point table-read SelectPriv registration at DataSource/PointGet boundaries; specs/D04.1-privilege/spec.md §5.3 item 2, §6.2 item 3)

    /// Now TableName in expression only used by sequence function like nextval(seq).
    /// The function arg should be evaluated as a table name rather than normal column name like mysql does.
    // go: expressionRewriter.toTable L1
    #[allow(clippy::wrong_self_convention)] // the Go method name
    fn to_table(&mut self, v: &ast::TableName) {
        let mut full_name = v.name.l();
        if !v.schema.o.is_empty() {
            full_name = format!("{}.{}", v.schema.l(), full_name);
        }
        let val = Constant::new(
            new_datum(GoValue::String(full_name.into_bytes())),
            new_field_type(TypeString),
        );
        self.ctx_stack_append(Expr::Constant(val).into_ref(), EmptyName.clone());
    }

    // go: expressionRewriter.toParamMarker L1
    #[allow(clippy::wrong_self_convention)] // the Go method name
    fn to_param_marker(&mut self, v: &ast::ParamMarkerExpr) -> Result<(), Error> {
        let mut value = param_marker_expression(self.sctx, v.order(), false)?;
        init_constant_repertoire(&*self.eval_ctx(), &mut value);
        let mut tp = std::mem::take(&mut value.ret_type);
        self.adjust_utf8mb4_collation(&mut tp);
        value.ret_type = tp;
        self.ctx_stack_append(Expr::Constant(value).into_ref(), EmptyName.clone());
        Ok(())
    }

    // go: expressionRewriter.clause L1
    fn clause(&self) -> ClauseCode {
        if let Some(pc) = &self.plan_ctx {
            return pc.builder.cur_clause;
        }
        ClauseCode::ExpressionClause
    }

    /// toColumn.
    ///
    /// The outer schemas (`b.outerSchemas`) are only non-empty inside a subquery (D09.6,
    /// spec §4): that loop never finds a column here.
    // go: expressionRewriter.toColumn L2 (no outer schemas outside subqueries; UNION's plan is not built yet)
    #[allow(clippy::wrong_self_convention)] // the Go method name
    fn to_column(&mut self, v: &ast::ColumnName) -> Result<(), Error> {
        let idx = match find_field_name(self.names, v) {
            Ok(i) => i,
            Err(_) => {
                return Err(ErrAmbiguous.gen_with_stack_by_args(go_args![
                    v.name.o.clone(),
                    clause_msg(ClauseCode::FieldList)
                ]));
            }
        };
        if idx >= 0 {
            let i = idx as usize;
            let column = &self.schema.columns[i];
            if column.is_hidden {
                return Err(ErrUnknownColumn.gen_with_stack_by_args(go_args![
                    v.name.o.clone(),
                    clause_msg(self.clause())
                ]));
            }
            let col = Expr::Column((**column).clone()).into_ref();
            let name = Arc::clone(&self.names[i]);
            self.ctx_stack_append(col, Arc::clone(&name));
            self.precollect_col_names(&name);
            return Ok(());
        } else if self.plan_ctx.is_none()
            && let Some(src) = self.source_table
            && (v.table.o.is_empty() || src.name.l == v.table.l())
        {
            // glue: blocked-by:D09.2 (part 2: the column of buildSimpleExpr's source table, ParseSimpleExpr)
            let _ = src;
            return Err(blocked(
                "D09.2",
                "a column of the source table of a simple expression (ParseSimpleExpr)",
            ));
        }

        if self.plan_ctx.is_none() {
            return Err(ErrUnknownColumn.gen_with_stack_by_args(go_args![
                column_name_string(v),
                clause_msg(self.clause())
            ]));
        }

        let plan = self.plan_ctx.as_ref().expect("plan ctx").plan;
        if let Some((col, name)) = find_field_name_from_natural_using_join(self.arena, plan, v)? {
            self.ctx_stack_append(col, Arc::clone(&name));
            self.precollect_col_names(&name);
            return Ok(());
        }
        // for i := len(planCtx.builder.outerSchemas) - 1; i >= 0; i--: empty (see the doc).
        // `*logicalop.LogicalUnionAll` with a table qualifier: UNION is not built yet
        // (D09.5), so the plan is never one.
        let pc = self.plan_ctx.as_mut().expect("plan ctx");
        if pc.builder.cur_clause == ClauseCode::GlobalOrderByClause {
            pc.builder.cur_clause = ClauseCode::OrderByClause;
        }
        let _ = column_name_orig_col_name;
        Err(ErrUnknownColumn.gen_with_stack_by_args(go_args![
            column_name_string(v),
            clause_msg(pc.builder.cur_clause)
        ]))
    }

    /// precollectColNames.
    ///
    /// `infoschema.TableIsView` needs the builder's InfoSchema (not there yet; views are not
    /// built, D09.1) and `b.nameMapCTE` the CTE builder (D09.5): neither applies.
    // go: expressionRewriter.precollectColNames L2 (no view or CTE names yet)
    fn precollect_col_names(&mut self, field_name: &FieldName) {
        let Some(pc) = self.plan_ctx.as_mut() else {
            return;
        };
        let col_name = VisitedColumn {
            name: field_name.orig_col_name.l.clone(),
            table: field_name.orig_tbl_name.l.clone(),
            schema: field_name.db_name.l.clone(),
        };

        // When checking a derived table, its colName.Schema.L would be empty, we can just skip it.
        // Because the column privilege is checked in the definition of derived table.
        if !col_name.name.is_empty() && !col_name.table.is_empty() && !col_name.schema.is_empty() {
            pc.col_names_for_lazily_privilege_check.push(col_name);
        }
    }
}

/// `arg.GetType().SetCollate(c)` on the rewriter's own node (spec §3 item 9). Go's
/// `Constant.GetType` of a parameter marker returns a fresh type, so the write is lost
/// there, as in `with_type_mut`.
// glue: Expression.GetType().SetCollate
fn set_type_collate(e: &mut ExprRef, collate: &str) {
    match Arc::make_mut(e) {
        Expr::Column(c) => c.ret_type_mut().set_collate(collate),
        Expr::CorrelatedColumn(c) => c.column.ret_type_mut().set_collate(collate),
        Expr::Constant(c) => {
            if c.param_marker.is_none() {
                c.ret_type.set_collate(collate);
            }
        }
        Expr::ScalarFunction(sf) => sf.ret_type_mut().set_collate(collate),
    }
}

/// findFieldNameFromNaturalUsingJoin.
///
/// Walks down Limit / Selection / TopN / Sort to a join's full schema (the merged columns
/// of NATURAL JOIN / USING). Joins are D09.3: no plan reaches one yet, so the walk ends at
/// the first other operator with Go's "not found" (spec §4).
// go: findFieldNameFromNaturalUsingJoin L2 (joins are blocked-by:D09.3; MaxOneRow and Apply come with D09.3 / D09.6)
#[allow(clippy::only_used_in_recursion)] // the join arm (D09.3) reads `v`
pub fn find_field_name_from_natural_using_join(
    arena: Option<&PlanArena>,
    p: Option<PlanId>,
    v: &ast::ColumnName,
) -> Result<Option<(ExprRef, Arc<FieldName>)>, Error> {
    let (Some(arena), Some(p)) = (arena, p) else {
        return Ok(None);
    };
    let plan = arena.get(p);
    match plan.kind() {
        LogicalKind::Limit | LogicalKind::Selection | LogicalKind::TopN | LogicalKind::Sort => {
            find_field_name_from_natural_using_join(Some(arena), Some(plan.children()[0]), v)
        }
        _ => Ok(None),
    }
}
