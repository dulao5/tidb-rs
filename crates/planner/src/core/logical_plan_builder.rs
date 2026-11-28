//! go-file: pkg/planner/core/logical_plan_builder.go
//!
//! The SELECT builder (D09.1 part 1, VS1-21; design: D09.1 spec §3): `buildSelect` and what
//! it needs for a single-table query block — the data source, the WHERE / HAVING selection,
//! the projection with wildcard expansion, ORDER BY and LIMIT — plus `resolveHavingAndOrderBy`
//! (D09.4), and the LIMIT helpers of the point get fast path (VS1-11).
//!
//! - Go appends select fields to the AST (`sel.Fields.Fields`, wildcard expansion and the
//!   auxiliary fields of HAVING / ORDER BY) and restores them at the end. The Rust AST is
//!   borrowed, so the builder keeps its own field list ([`SelectFieldW`]) whose entries borrow
//!   the AST's nodes or own the column-name nodes it creates (spec §3 item 2).
//! - Joins of two sources with ON / USING / NATURAL (the join builder, D09.3 part A; spec
//!   `specs/D09.3-join-build/spec.md`) are built here; LATERAL and the apply builder (D09.3
//!   part C), subqueries (D09.6), ROLLUP and correlated aggregates (D09.4), CTEs and
//!   set operations (D09.5), views, virtual generated columns, locks, hints (D13.3),
//!   partitioned tables (D09.12) and the other branches the slice does not need are boundary
//!   stubs at the place Go takes them (spec §5).

#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::{blocked, blocked_by};
use tidb_base::dbterror::plannererrors::{
    ErrAggregateInOrderNotSelect, ErrAggregateOrderNonAggQuery, ErrAmbiguous, ErrBadTable,
    ErrDupFieldName, ErrFieldInGroupingNotGroupBy, ErrFieldInOrderNotSelect, ErrFieldNotInGroupBy,
    ErrIllegalReference, ErrInvalidWildCard, ErrMixOfGroupFuncAndFields, ErrNotSupportedYet,
    ErrPartitionClauseOnNonpartitioned, ErrTableaccessDenied, ErrUnknown, ErrUnknownColumn,
    ErrUnsupportedType, ErrWindowCircularityInWindowGraph, ErrWindowDuplicateName,
    ErrWindowFrameEndIllegal, ErrWindowFrameIllegal, ErrWindowFrameStartIllegal,
    ErrWindowFunctionIgnoresFrame, ErrWindowInvalidWindowFuncAliasUse,
    ErrWindowInvalidWindowFuncUse, ErrWindowNoChildPartitioning, ErrWindowNoInherentFrame,
    ErrWindowNoRedefineOrderBy, ErrWindowNoSuchWindow, ErrWindowRangeBoundNotConstant,
    ErrWindowRangeFrameNumericType, ErrWindowRangeFrameOrderType, ErrWindowRangeFrameTemporalType,
    ErrWindowRowsIntervalUse, ErrWrongArguments, ErrWrongGroupField,
};
use tidb_base::go_args;
use tidb_base::gostd::is_one_of;
use tidb_base::mysql::{
    DefaultCharset, DefaultCollationName, ExtendedPriv, MaxRealWidth, NotNullFlag, RangeGraph,
    SQLMode, SelectPriv, TypeDouble, TypeNull, TypeTiny, TypeVarchar, has_not_null_flag,
    has_pri_key_flag,
};
use tidb_chunk::Row;
use tidb_expression::aggregation::{
    need_frame, new_agg_func_desc, new_window_func_desc, use_default_frame,
};
use tidb_expression::builtin_cast::try_push_cast_into_control_function_for_hybrid_type;
use tidb_expression::builtin_compare::{get_accurate_cmp_type, get_cmp_function};
use tidb_expression::constant::{Constant, new_zero};
use tidb_expression::expression::{EvalState, split_cnf_items};
use tidb_expression::scalar_function::{new_function, new_function_base};
use tidb_expression::simple_rewriter::{
    column_name_orig_col_name, column_name_string, find_field_name,
};
use tidb_expression::util::{
    ParamMarkerInPrepareChecker, PositionExpr, const_expr_consider_plan_cache, extract_columns,
    get_string_from_constant, param_marker_expression, pos_from_position_expr,
};
use tidb_expression::{
    Column, Expr as PExpr, ExprRef, Schema, column2_exprs, eval_bool, gc_column_expr_is_tidb_shard,
    merge_schema, new_schema,
};
use tidb_infoschema::metadef::is_mem_db;
use tidb_infoschema::tables::find_primary_index;
use tidb_model::{
    ColumnInfo, ExtraHandleID, ExtraHandleName, ExtraPhysTblID, StatePublic,
    TableCacheStatusEnable, TableInfo, TempTableLocal, new_extra_handle_col_info,
};
use tidb_parser::ast::{self, Expr, expression_deep_equal_with};
use tidb_parser::model::CIStr;
use tidb_parser::restore_window_spec_raw;
use tidb_parser::types::ETString;
use tidb_parser::types::UnspecifiedLength;
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::{BuildContext, ctx_with_handle_truncate_err_level};
use tidb_types::convert::str_to_uint;
use tidb_types::datum::new_string_datum;
use tidb_types::etc::{FieldType, is_type_numeric, is_type_temporal};
use tidb_types::field_name::{EmptyName, FieldName, NameSlice};
use tidb_types::field_type::{default_type_for_value, new_field_type, set_bin_chs_cln_flag};

use crate::arena::{PlanArena, PlanId, with_detached};
use crate::base::{LogicalKind, LogicalPlan};
use crate::core::expression_rewriter::{
    AggMapper, ByAddr, CheckSelectColumnPrivOption, ClauseCode, ColReplacements, RewritePlan,
    VisitedColumn, WindowMapper, clause_msg, eval_ast_expr, eval_ast_expr_with_plan_ctx, node_addr,
    value_expr_go_value,
};
use crate::core::find_best_task::compare_bool;
use crate::core::plan_cache_utils::table_is_view;
use crate::core::planbuilder::{
    PlanBuilder, VisitInfo, filter_path_by_isolation_read, get_possible_access_paths, has_agg_flag,
    has_window_flag, is_for_update_read_select_lock, split_where,
};
use crate::core::point_get_plan::value_expr_datum;
use crate::core::preprocess::try_lock_mdl_and_update_schema_if_necessary;
use crate::core::resolve::ContextRef;
use crate::core::rule::logical_rules::{
    FlagBuildKeyInfo, FlagConstantPropagation, FlagDeriveTopNFromWindow, FlagEliminateAgg,
    FlagEliminateOuterJoin, FlagEliminateProjection, FlagGcSubstitute, FlagJoinReOrder,
    FlagMaxMinEliminate, FlagPredicatePushDown, FlagPredicateSimplification, FlagPushDownAgg,
    FlagPushDownTopN, FlagSkewDistinctAgg,
};
use crate::core::util::{AggregateFuncExtractor, WindowFuncExtractor, table_has_dirty_content};
use crate::hint::{PlanHints, parse_plan_hints};
use crate::logicalop::{
    DataSource, FrameBound, JoinType, LogicalAggregation, LogicalJoin, LogicalLimit,
    LogicalProjection, LogicalSelection, LogicalSort, LogicalTableDual, LogicalTopN, LogicalWindow,
    WindowFrame,
};
use crate::planctx::PlanCtx;
use crate::property::SortItem;
use crate::util::column::index_info2_full_cols;
use crate::util::handle_cols::{new_common_handle_cols, new_int_handle_cols};
use crate::util::null_misc::reset_not_null_flag;
use crate::util::{ByItems, HandleCols};

/// ErrExprInSelect is in select fields for the error of ErrFieldNotInGroupBy.
pub const ErrExprInSelect: &str = "SELECT list";
/// ErrExprInOrderBy is in order by items for the error of ErrFieldNotInGroupBy.
pub const ErrExprInOrderBy: &str = "ORDER BY";

/// getUintFromNode gets uint64 value from ast.Node.
/// For ordinary statement, node should be uint64 constant value.
/// For prepared statement, node is string. We should convert it to uint64.
///
/// Returns `(uVal, isNull, isExpectedType)`. Go's `ParamMarkerExpr.InExecute` lives on the
/// AST node (false during PREPARE, true during EXECUTE); the Rust AST keeps no value or flag
/// on the marker, so callers pass `in_execute` (`StmtContext.in_prepared_plan_building`) and
/// the parameter value is read from `PlanCacheParams` by `m.order()`.
// go: getUintFromNode L2 (in_execute passed by caller; parameter value read from PlanCacheParams)
pub fn get_uint_from_node(
    ctx: &dyn BuildContext,
    n: &Expr,
    must_int64_or_uint64: bool,
    in_execute: bool,
) -> (u64, bool, bool) {
    match n {
        // val = v.GetValue()
        Expr::Value(v) => match &v.datum {
            ast::Datum::Uint(v) => (*v, false, true),
            ast::Datum::Int(v) if *v >= 0 => (*v as u64, false, true),
            // TRUE / FALSE are int64 datums in Go (Datum.SetValue of a bool).
            ast::Datum::Bool(b) => (u64::from(*b), false, true),
            ast::Datum::Str(v) => {
                let (u_val, err) = str_to_uint(&ctx.get_eval_ctx().type_ctx(), v.as_bytes(), false);
                if err.is_err() {
                    return (0, false, false);
                }
                (u_val, false, true)
            }
            _ => (0, false, false),
        },
        Expr::ParamMarker(m) => {
            if !in_execute {
                return (0, false, true);
            }
            if must_int64_or_uint64 {
                let datum = ctx
                    .get_eval_ctx()
                    .get_param_value(m.order() as i64)
                    .unwrap_or_default();
                let (expected, _) = check_param_type_int64_or_uint64(&datum);
                if !expected {
                    return (0, false, false);
                }
            }
            let Ok(param) = param_marker_expression(ctx, m.order(), false) else {
                return (0, false, false);
            };
            let Ok((s, is_null)) =
                get_string_from_constant(&*ctx.get_eval_ctx(), &PExpr::Constant(param))
            else {
                return (0, false, false);
            };
            if is_null {
                return (0, true, true);
            }
            let (u_val, err) = str_to_uint(&ctx.get_eval_ctx().type_ctx(), &s, false);
            if err.is_err() {
                return (0, false, false);
            }
            (u_val, false, true)
        }
        _ => (0, false, false),
    }
}

// go: extractLimitCountOffset L1
pub fn extract_limit_count_offset(
    ctx: &dyn BuildContext,
    limit: &ast::Limit,
    in_execute: bool,
) -> Result<(u64, u64), Error> {
    let mut offset = 0;
    // limit.Count != nil: always set by the parser.
    let (c, _, is_expected_type) = get_uint_from_node(ctx, &limit.count, true, in_execute);
    if !is_expected_type {
        return Err(ErrWrongArguments.gen_with_stack_by_args(tidb_base::go_args!["LIMIT"]));
    }
    let count = c;
    if let Some(o) = &limit.offset {
        let (o, _, is_expected_type) = get_uint_from_node(ctx, o, true, in_execute);
        if !is_expected_type {
            return Err(ErrWrongArguments.gen_with_stack_by_args(tidb_base::go_args!["LIMIT"]));
        }
        offset = o;
    }
    Ok((count, offset))
}

/// appendVisitInfo.
// go: appendVisitInfo L1
pub fn append_visit_info(
    vi: &mut Vec<VisitInfo>,
    privilege: u64,
    db: &str,
    tbl: &str,
    col: &str,
    err: Option<Error>,
) {
    vi.push(VisitInfo {
        privilege,
        db: db.to_owned(),
        table: tbl.to_owned(),
        column: col.to_owned(),
        err,
        ..Default::default()
    });
}

impl PlanBuilder<'_> {
    /// Registers the table-read `SELECT` privilege entry (`column = "*"`, error `1142`) at a
    /// data-source boundary so queries without column references (e.g. `SELECT 1 FROM t`,
    /// `SELECT COUNT(*) FROM t`, `SUM(1)`, `EXISTS(SELECT 1 FROM t)`) cannot bypass table
    /// privilege checks (specs/D04.1-privilege/spec.md §5.2 item 1, ruling R29).
    // glue: R29 table-read SelectPriv single-point registration
    pub fn append_table_select_visit_info(&mut self, db: &str, table: &str) {
        let gs = |s: &str| tidb_base::gofmt::Arg::from(s.to_owned());
        let err = ErrTableaccessDenied.fast_gen_by_args(vec![
            gs("SELECT"),
            gs(&self.user_name),
            gs(&self.host_name),
            gs(table),
        ]);
        append_visit_info(
            &mut self.visit_info,
            SelectPriv.0,
            db,
            table,
            "*",
            Some(err),
        );
    }
}

// ---------------------------------------------------------------- select fields

/// The expression of a [`SelectFieldW`]: a node of the statement's AST, or a column name the
/// builder created (Go allocates an `*ast.ColumnNameExpr`).
#[derive(Debug)]
pub enum FieldExpr<'a> {
    /// A node of the statement's AST.
    Ast(&'a Expr),
    /// A node the builder created.
    Built(Box<Expr>),
}

impl FieldExpr<'_> {
    /// The node.
    // glue: accessor of the builder's select field
    pub fn get(&self) -> &Expr {
        match self {
            FieldExpr::Ast(e) => e,
            FieldExpr::Built(e) => e,
        }
    }
}

/// A select field of the query block being built: Go's `*ast.SelectField`, including the
/// fields only the builder sets (`Auxiliary`, `AuxiliaryColInAgg`, `AuxiliaryColInOrderBy`,
/// `IsUnfoldFromWildCard`). See the module doc.
#[derive(Debug)]
pub struct SelectFieldW<'a> {
    /// WildCard.
    pub wild_card: Option<&'a ast::WildCardField>,
    /// Expr.
    pub expr: Option<FieldExpr<'a>>,
    /// AsName.
    pub as_name: ast::CIStr,
    /// The field's text (`Text()`).
    pub text: Cow<'a, str>,
    /// Auxiliary means if this field is added by the planner.
    pub auxiliary: bool,
    /// AuxiliaryColInAgg means if this column is added to the select fields because it is
    /// referred by an aggregate function in HAVING / ORDER BY.
    pub auxiliary_col_in_agg: bool,
    /// AuxiliaryColInOrderBy means if this column is added to the select fields because it is
    /// referred by ORDER BY.
    pub auxiliary_col_in_order_by: bool,
    /// IsUnfoldFromWildCard means if this field is unfolded from a wildcard.
    pub is_unfold_from_wild_card: bool,
}

impl<'a> SelectFieldW<'a> {
    /// The field of the AST node `f`; `sql` is the statement's text (the node keeps a span).
    // glue: the builder's view of an ast.SelectField
    pub fn of_ast(f: &'a ast::SelectField, sql: &'a str) -> SelectFieldW<'a> {
        SelectFieldW {
            wild_card: f.wild_card.as_deref(),
            expr: f.expr.as_ref().map(FieldExpr::Ast),
            as_name: f.as_name.clone(),
            text: Cow::Borrowed(
                f.text
                    .and_then(|sp| sql.get(sp.start()..sp.end()))
                    .unwrap_or(""),
            ),
            auxiliary: false,
            auxiliary_col_in_agg: false,
            auxiliary_col_in_order_by: false,
            is_unfold_from_wild_card: false,
        }
    }

    /// An auxiliary field with `expr`.
    // glue: Go's &ast.SelectField{Auxiliary: true, Expr: ...}
    fn auxiliary(expr: FieldExpr<'a>) -> SelectFieldW<'a> {
        SelectFieldW {
            wild_card: None,
            expr: Some(expr),
            as_name: ast::CIStr::default(),
            text: Cow::Borrowed(""),
            auxiliary: true,
            auxiliary_col_in_agg: false,
            auxiliary_col_in_order_by: false,
            is_unfold_from_wild_card: false,
        }
    }

    /// `field.Expr`.
    // glue: accessor of the builder's select field
    pub fn expr(&self) -> Option<&Expr> {
        self.expr.as_ref().map(FieldExpr::get)
    }

    /// Match checks if this field matches the column name expression (`ast.SelectField.Match`).
    // go: pkg/parser/ast/dml.go SelectField.Match L1
    pub fn match_column(&self, col: &ast::ColumnName, ignore_as_name: bool) -> bool {
        // if col specify a table name, resolve from table source directly.
        if col.table.l().is_empty() {
            if self.as_name.l().is_empty() || ignore_as_name {
                match self.expr() {
                    Some(Expr::Column(cur_col)) => return cur_col.name.l() == col.name.l(),
                    Some(Expr::FuncCall(_)) => {
                        // Fix issue 7331
                        // If there are some function calls in SelectField, we check if
                        // ColumnNameExpr in GroupByClause matches one of these function calls.
                        // Example: select concat(k1,k2) from t group by `concat(k1,k2)`,
                        // `concat(k1,k2)` matches with function call concat(k1, k2).
                        return self.text.to_lowercase() == col.name.l();
                    }
                    // a expression without as name can't be matched.
                    _ => return false,
                }
            }
            return self.as_name.l() == col.name.l();
        }
        false
    }
}

/// `ast.ColumnName.Match`: means that if a match b, e.g. t.a can match test.t.a but
/// test.t.a can't match t.a. Because column a want column from database test exactly.
// go: pkg/parser/ast/expressions.go ColumnName.Match L1
pub fn column_name_match(a: &ast::ColumnName, b: &ast::ColumnName) -> bool {
    if (a.schema.l().is_empty() || a.schema.l() == b.schema.l())
        && (a.table.l().is_empty() || a.table.l() == b.table.l())
    {
        return a.name.l() == b.name.l();
    }
    false
}

/// `parser.SpecFieldPattern.ReplaceAllStringFunc(text, parser.TrimComment)`: removes the
/// MySQL special comment markers (`/*!`, `/*!50100`, `/*!M50100`, `*/`) from a field's text.
// glue: parser.SpecFieldPattern with parser.TrimComment (yy_parser.go, D02.2)
fn trim_spec_field_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"*/") {
            i += 2;
            continue;
        }
        if b[i..].starts_with(b"/*!") {
            // (\/\*!(M?[0-9]{5,6})?: the optional version, greedy.
            let mut j = i + 3;
            if j < b.len() && b[j] == b'M' {
                j += 1;
            }
            let digits = b[j..]
                .iter()
                .take(6)
                .take_while(|c| c.is_ascii_digit())
                .count();
            i = if digits >= 5 { j + digits } else { i + 3 };
            continue;
        }
        let ch = text[i..].chars().next().expect("char");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The schema of plan `p`.
// glue: p.Schema() over the arena (N1)
fn schema_of(arena: &PlanArena, p: PlanId) -> &Schema {
    arena.get(p).schema(arena)
}

/// The output names of plan `p`.
// glue: p.OutputNames() over the arena (N1)
fn names_of(arena: &PlanArena, p: PlanId) -> &NameSlice {
    arena.get(p).output_names(arena)
}

/// The plan a rewrite resolves names against (D09.2 spec §3 item 2).
// glue: the plan argument of b.rewrite
fn rewrite_plan(arena: &PlanArena, p: PlanId) -> RewritePlan<'_> {
    RewritePlan {
        id: Some(p),
        arena: Some(arena),
        schema: schema_of(arena, p),
        names: names_of(arena, p),
    }
}

/// A model `CIStr` of an AST one.
// glue: the AST's CIStr (original string only) as the model's
fn ci(s: &ast::CIStr) -> CIStr {
    CIStr::new(&s.o)
}

/// `&ast.ColumnName{Schema: name.DBName, Table: name.TblName, Name: name.ColName}`.
// glue: an AST column name of a field name
fn column_name_of(name: &FieldName) -> ast::ColumnName {
    ast::ColumnName {
        schema: ast::CIStr::new(name.db_name.o.clone()),
        table: ast::CIStr::new(name.tbl_name.o.clone()),
        name: ast::CIStr::new(name.col_name.o.clone()),
    }
}

/// Whether a subquery is in `e` (the subquery nodes are leaves of [`expr_children`]).
// glue: an ast.Visitor looking for subqueries
fn has_subquery(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Subquery(_) | Expr::Exists(_) | Expr::CompareSubquery(_)
    ) || matches!(e, Expr::PatternIn(p) if p.sel.is_some())
        || expr_children(e).into_iter().any(has_subquery)
}

/// The expression children of `e` in the order of its Go `Accept`. A subquery
/// (`SubqueryExpr`) is a leaf; the window specification of a window function and the
/// ORDER BY of an aggregate are not expression children (the visitors that need them walk
/// them).
///
/// `VALUES(col)` is a leaf too: Go's `ValuesExpr.Accept` visits its `ColumnNameExpr`, but the
/// Rust AST keeps a bare column name there, not an expression node. The only effect is on
/// the HAVING / ORDER BY resolver and the ONLY_FULL_GROUP_BY walk, which would resolve that
/// column first: a statement with `VALUES(col)` stops at the rewriter's boundary
/// (`blocked-by:D09.15`, INSERT ... ON DUPLICATE KEY UPDATE only) instead of, for an unknown
/// column, Go's error, and the router's fallback then gives Go's answer.
// glue: the child order of each ast node's Accept (the Rust AST has no visitor, D02.2)
fn expr_children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Binary(b) => vec![&b.l, &b.r],
        Expr::Unary(u) => vec![&u.v],
        Expr::FuncCall(f) => f.args.iter().collect(),
        Expr::FuncCast(c) => vec![&c.expr],
        Expr::JSONSumCrc32(c) => vec![&c.expr],
        Expr::Aggregate(a) => a.args.iter().collect(),
        Expr::Window(w) => w.args.iter().collect(),
        Expr::Paren(p) => vec![&**p],
        Expr::PatternIn(p) => {
            let mut v = vec![&p.expr];
            v.extend(p.list.iter());
            v.extend(p.sel.iter());
            v
        }
        Expr::PatternLike(p) => vec![&p.expr, &p.pattern],
        Expr::PatternRegexp(p) => vec![&p.expr, &p.pattern],
        Expr::Between(b) => vec![&b.expr, &b.left, &b.right],
        Expr::IsNull(x) => vec![&x.expr],
        Expr::IsTruth(x) => vec![&x.expr],
        Expr::Case(c) => {
            let mut v: Vec<&Expr> = c.value.iter().collect();
            for w in &c.when_clauses {
                v.push(&w.expr);
                v.push(&w.result);
            }
            v.extend(c.else_clause.iter());
            v
        }
        Expr::Exists(x) => vec![&x.sel],
        Expr::CompareSubquery(x) => vec![&x.l, &x.r],
        Expr::Variable(v) => v.value.iter().collect(),
        Expr::Row(r) => r.values.iter().collect(),
        Expr::SetCollation(s) => vec![&s.expr],
        Expr::MatchAgainst(m) => vec![&m.against],
        Expr::Column(_)
        | Expr::Value(_)
        | Expr::ParamMarker(_)
        | Expr::Subquery(_)
        | Expr::Values(_)
        | Expr::Default(_)
        | Expr::TableName(_)
        | Expr::TimeUnit(_)
        | Expr::GetFormatSelector(_)
        | Expr::TrimDirection(_)
        | Expr::Position(_)
        | Expr::Unported(_) => Vec::new(),
    }
}

/// The output names of `p` (and of the plans below it that share them) with `TblName` set to
/// `as_name`, except the hidden ones.
///
/// Go writes `name.TblName = x.AsName` through the shared `*types.FieldName`s, so every plan
/// of the derived table that holds the same names sees the new table name (`resolveFromPlan`
/// walks down through Selection / Sort / Limit and reads them). The names are shared `Arc`s
/// here; this replaces each renamed name by its new value in every plan of the subtree that
/// holds the same `Arc` (D09.1 spec §3 item 4, M-FT).
// glue: Go's in-place write through shared *types.FieldName pointers
fn set_tbl_name_shared(arena: &mut PlanArena, p: PlanId, as_name: &CIStr) {
    let renamed: Vec<(Arc<FieldName>, Arc<FieldName>)> = names_of(arena, p)
        .iter()
        .filter(|name| !name.hidden)
        .map(|name| {
            let mut n = FieldName::clone(name);
            n.tbl_name = as_name.clone();
            (Arc::clone(name), Arc::new(n))
        })
        .collect();
    let mut stack = vec![p];
    while let Some(id) = stack.pop() {
        let node = arena.get(id);
        stack.extend(node.children().iter().copied());
        if node.schema_producer().is_none() {
            continue;
        }
        let names = node.output_names(arena);
        if !names
            .iter()
            .any(|n| renamed.iter().any(|(old, _)| Arc::ptr_eq(n, old)))
        {
            continue;
        }
        let new_names: NameSlice = names
            .iter()
            .map(|n| {
                renamed
                    .iter()
                    .find(|(old, _)| Arc::ptr_eq(n, old))
                    .map_or_else(|| Arc::clone(n), |(_, new)| Arc::clone(new))
            })
            .collect();
        arena.set_output_names(id, new_names);
    }
}

// ---------------------------------------------------------------- building

impl<'ast> PlanBuilder<'ast> {
    // go: PlanBuilder.buildTableRefs L1 [M-CTX]
    pub fn build_table_refs(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        from: Option<&'ast ast::TableRefsClause>,
    ) -> Result<PlanId, Error> {
        let Some(from) = from else {
            return Ok(self.build_table_dual(ctx));
        };
        // defer: reset cte.recursiveRef of b.outerCTEs (no CTEs here, D09.5).
        self.build_result_set_node(ctx, ResultSetRef::Join(&from.table_refs), false)
    }

    /// buildResultSetNode. The LATERAL derived tables, the column alias list of `AS dt(c1,
    /// ...)` and `PlannerSelectBlockAsName` (hints, D13.3) are not in the Rust AST or not
    /// ported (spec §5); a set operation is `blocked-by:D09.5`.
    // go: PlanBuilder.buildResultSetNode L2 (LATERAL and the column alias list are not in the Rust AST; set operations are blocked-by:D09.5)
    pub fn build_result_set_node(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        node: ResultSetRef<'ast>,
        is_cte: bool,
    ) -> Result<PlanId, Error> {
        // b.isCTE = isCTE (CTEs are D09.5).
        let _ = is_cte;
        match node {
            ResultSetRef::Join(x) => self.build_join(ctx, x),
            ResultSetRef::Node(ast::ResultSetNode::Join(x)) => self.build_join(ctx, x),
            ResultSetRef::Node(ast::ResultSetNode::TableSource(x)) => {
                let mut is_table_name = false;
                let p = match &x.source {
                    ast::ResultSetNode::Select(v) => {
                        // ci := b.prepareCTECheckForSubQuery(): no CTE is being built (D09.5).
                        self.opt_flag |= FlagConstantPropagation;
                        self.build_select(ctx, v)?
                    }
                    ast::ResultSetNode::SetOpr(_) => {
                        // glue: blocked-by:D09.5 (set operations, buildSetOpr)
                        return Err(blocked("D09.5", "a set operation (buildSetOpr)"));
                    }
                    ast::ResultSetNode::TableName(v) => {
                        is_table_name = true;
                        self.build_data_source(ctx, v, &x.as_name)?
                    }
                    other => {
                        return Err(ErrUnsupportedType
                            .gen_with_stack_by_args(go_args![format!("{other:?}")]));
                    }
                };
                let _ = is_table_name;

                if !x.as_name.l().is_empty() {
                    // Non-LATERAL: preserve original behavior — update TblName in place.
                    set_tbl_name_shared(&mut ctx.arena, p, &ci(&x.as_name));
                }
                // Duplicate column name in one table is not allowed.
                // "select * from (select 1, 1) as a;" is duplicate
                let mut dup_names: Vec<&str> =
                    Vec::with_capacity(schema_of(&ctx.arena, p).columns.len());
                for name in names_of(&ctx.arena, p) {
                    let col_name = name.col_name.o.as_str();
                    if dup_names.contains(&col_name) {
                        return Err(
                            ErrDupFieldName.gen_with_stack_by_args(go_args![col_name.to_owned()])
                        );
                    }
                    dup_names.push(col_name);
                }
                Ok(p)
            }
            ResultSetRef::Node(ast::ResultSetNode::Select(x)) => self.build_select(ctx, x),
            ResultSetRef::Node(ast::ResultSetNode::SetOpr(_)) => {
                // glue: blocked-by:D09.5 (set operations, buildSetOpr)
                Err(blocked("D09.5", "a set operation (buildSetOpr)"))
            }
            ResultSetRef::Node(ast::ResultSetNode::Unported(_)) => {
                // glue: blocked-by:D02.2 (a FROM clause the Rust parser builds as a placeholder)
                Err(blocked(
                    "D02.2",
                    "a result set node the Rust parser does not build",
                ))
            }
            ResultSetRef::Node(ast::ResultSetNode::TableName(_)) => Err(ErrUnsupportedType
                .gen_with_stack(
                    "Unsupported ast.ResultSetNode(%T) for buildResultSetNode()",
                    go_args!["*ast.TableName"],
                )),
        }
    }

    /// buildJoin.
    ///
    /// L2 (each unreachable or not-yet-ported branch is named, none is skipped silently):
    /// - LATERAL (`isLateral`, `buildLateralJoin`, the outer-schema stack): the parser never
    ///   builds a LATERAL table source yet (`TableSource.lateral` is always false, D02.2);
    ///   when it does, the branch stops at `blocked-by:D09.3` (the apply and LATERAL part)
    ///   where Go pushes the outer schemas.
    /// - The recursive CTE on the right side of a LEFT JOIN (`:818-820`): Rust has no
    ///   `LogicalCTETable` yet (CTEs, D09.5), so the right plan is never one.
    /// - The `LogicalApply` arms of the full-schema merge: there is no `LogicalApply` yet
    ///   (the apply builder, D09.3 part C; correlated subqueries, D09.6).
    /// - `newPlan != joinPlan` after the ON rewrite (1105 "ON condition doesn't support
    ///   subqueries yet"): the Rust rewriter never returns a new plan; a subquery in ON stops
    ///   in the rewriter at `blocked-by:D09.6`.
    /// - `FULL OUTER JOIN`: v8.5.8's grammar has no FULL join type, the Rust parser builds
    ///   one (`ast::JoinType::FullJoin`), so it stops at `blocked-by:D02.2` instead of falling
    ///   into the inner join branch.
    /// - The join is put into the arena before the ON rewrite, because the rewriter resolves
    ///   names against it (its full schema included); the plan ids are allocated in Go's
    ///   order (left, right, join, then the selection of an inner join's ON).
    // go: PlanBuilder.buildJoin L2 (see above: LATERAL, CTE and apply branches are unreachable; FULL JOIN is blocked-by:D02.2)
    pub fn build_join(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        join_node: &'ast ast::Join,
    ) -> Result<PlanId, Error> {
        // We will construct a "Join" node for some statements like "INSERT",
        // "DELETE", "UPDATE", "REPLACE". For this scenario "joinNode.Right" is nil
        // and we only build the left "ResultSetNode".
        // (Go's `Left` is never nil; the Rust AST's is an `Option`, checked once here for both
        // cases.)
        let Some(left) = &join_node.left else {
            // glue: blocked-by:D02.2 (a join without a left side from the Rust parser)
            return Err(blocked("D02.2", "a join without a left side"));
        };
        let Some(right) = &join_node.right else {
            return self.build_result_set_node(ctx, ResultSetRef::Node(left), false);
        };

        // Detect whether the right subtree contains any LATERAL table source.
        // This is used only to decide whether to push outerSchemas before building
        // the right side, so that nested LATERAL sources can resolve outer columns.
        let is_lateral = contains_lateral_table_source(right);

        self.opt_flag |= FlagPredicatePushDown;
        // Don't enable join reorder for LATERAL (similar to StraightJoin)
        // LATERAL has order dependencies: right side can reference left side
        if !is_lateral {
            self.opt_flag |= FlagJoinReOrder;
        }
        self.opt_flag |= FlagPredicateSimplification;

        let left_plan = self.build_result_set_node(ctx, ResultSetRef::Node(left), false)?;

        // For LATERAL: Push left side schema onto outerSchemas stack
        if is_lateral {
            // glue: blocked-by:D09.3 (LATERAL derived tables: the outer schema stack and buildLateralJoin, the apply part)
            return Err(blocked(
                "D09.3",
                "a LATERAL derived table (buildLateralJoin)",
            ));
        }

        let right_plan = self.build_result_set_node(ctx, ResultSetRef::Node(right), false)?;

        // The recursive part in CTE must not be on the right side of a LEFT JOIN: the right
        // plan is never a LogicalCTETable here (CTEs, D09.5; see the doc comment).

        let handle_map1 = self.handle_helper.pop_map();
        let handle_map2 = self.handle_helper.pop_map();
        self.handle_helper
            .merge_and_push(handle_map1.as_ref(), handle_map2.as_ref());

        let mut join_plan = LogicalJoin::init(ctx, self.get_select_offset());
        join_plan.straight_join = join_node.straight_join || self.in_straight_join;
        join_plan.set_children(vec![left_plan, right_plan]);
        let left_len = schema_of(&ctx.arena, left_plan).len();
        let right_len = schema_of(&ctx.arena, right_plan).len();
        let mut join_schema = merge_schema(
            Some(schema_of(&ctx.arena, left_plan)),
            Some(schema_of(&ctx.arena, right_plan)),
        )
        .expect("both schemas are set");
        let mut output_names: NameSlice = Vec::with_capacity(left_len + right_len);
        output_names.extend(names_of(&ctx.arena, left_plan).iter().cloned());
        output_names.extend(names_of(&ctx.arena, right_plan).iter().cloned());

        // Set join type.
        match join_node.tp {
            ast::JoinType::LeftJoin => {
                // left outer join need to be checked elimination
                self.opt_flag |= FlagEliminateOuterJoin;
                join_plan.join_type = JoinType::LeftOuterJoin;
                let len = join_schema.len();
                reset_not_null_flag(&mut join_schema, left_len, len);
            }
            ast::JoinType::RightJoin => {
                // right outer join need to be checked elimination
                self.opt_flag |= FlagEliminateOuterJoin;
                join_plan.join_type = JoinType::RightOuterJoin;
                reset_not_null_flag(&mut join_schema, 0, left_len);
            }
            ast::JoinType::FullJoin => {
                // glue: blocked-by:D02.2 (the parser builds FULL OUTER JOIN, which v8.5.8's grammar rejects)
                return Err(blocked(
                    "D02.2",
                    "FULL OUTER JOIN (v8.5.8's parser rejects it; the Rust parser builds it)",
                ));
            }
            ast::JoinType::CrossJoin => {
                join_plan.join_type = JoinType::InnerJoin;
            }
        }
        join_plan.sp.set_schema(Some(join_schema));
        join_plan.sp.set_output_names(output_names);

        // Merge sub-plan's FullSchema into this join plan.
        // Please read the comment of LogicalJoin.FullSchema for the details.
        // (The LogicalApply arms: there is no LogicalApply yet, see the doc comment.)
        let full_of = |p: PlanId| -> (&Schema, &NameSlice) {
            if let Some(x) = ctx.arena.downcast_ref::<LogicalJoin>(p)
                && let Some(full_schema) = &x.full_schema
            {
                return (full_schema, &x.full_names);
            }
            (schema_of(&ctx.arena, p), names_of(&ctx.arena, p))
        };
        let (mut l_full_schema, mut l_full_names) = full_of(left_plan);
        let (mut r_full_schema, mut r_full_names) = full_of(right_plan);
        if join_node.tp == ast::JoinType::RightJoin {
            // Make sure lFullSchema means outer full schema and rFullSchema means inner full schema.
            std::mem::swap(&mut l_full_schema, &mut r_full_schema);
            std::mem::swap(&mut l_full_names, &mut r_full_names);
        }
        let mut full_schema =
            merge_schema(Some(l_full_schema), Some(r_full_schema)).expect("both schemas are set");

        // Clear NotNull flag for the inner side schema if it's an outer join.
        if join_node.tp == ast::JoinType::LeftJoin || join_node.tp == ast::JoinType::RightJoin {
            let len = full_schema.len();
            reset_not_null_flag(&mut full_schema, l_full_schema.len(), len);
        }

        // Merge sub-plan's FullNames into this join plan, similar to the FullSchema logic above.
        let mut full_names: NameSlice = Vec::with_capacity(l_full_names.len() + r_full_names.len());
        for l_name in l_full_names {
            full_names.push(Arc::new(FieldName::clone(l_name)));
        }
        for r_name in r_full_names {
            full_names.push(Arc::new(FieldName::clone(r_name)));
        }
        join_plan.full_schema = Some(full_schema);
        join_plan.full_names = full_names;

        // Set preferred join algorithm if some join hints is specified by user.
        join_plan.set_preferred_join_type_and_order(ctx, self.table_hints());

        // "NATURAL JOIN" doesn't have "ON" or "USING" conditions.
        //
        // The "NATURAL [LEFT] JOIN" of two tables is defined to be semantically
        // equivalent to an "INNER JOIN" or a "LEFT JOIN" with a "USING" clause
        // that names all columns that exist in both tables.
        //
        // See https://dev.mysql.com/doc/refman/5.7/en/join.html for more detail.
        if join_node.natural_join {
            self.build_natural_join(ctx, &mut join_plan, left_plan, right_plan, join_node)?;
        } else if !join_node.using.is_empty() {
            self.build_using_clause(ctx, &mut join_plan, left_plan, right_plan, join_node)?;
        } else if let Some(on) = &join_node.on {
            let join_id = ctx.arena.alloc(Box::new(join_plan));
            self.cur_clause = ClauseCode::OnClause;
            let origin_col_priv = self.check_col_priv;
            self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
            let res = self.rewrite(
                &*ctx.expr_ctx,
                &on.expr,
                rewrite_plan(&ctx.arena, join_id),
                None,
                false,
            );
            self.check_col_priv = origin_col_priv;
            let on_expr = res?.expect("an ON condition rewrites to an expression");
            // newPlan != joinPlan: the rewriter returns no new plan (see the doc comment).
            let on_condition = split_cnf_items(&on_expr);
            // Keep these expressions as a LogicalSelection upon the inner join, in order to apply
            // possible decorrelate optimizations. The ON clause is actually treated as a WHERE clause now.
            let join_type = ctx
                .arena
                .downcast_ref::<LogicalJoin>(join_id)
                .expect("the join just built")
                .join_type;
            if join_type == JoinType::InnerJoin {
                let mut sel = LogicalSelection::init(on_condition, ctx, self.get_select_offset());
                sel.set_children(vec![join_id]);
                return Ok(ctx.arena.alloc(Box::new(sel)));
            }
            with_detached(ctx, join_id, |node, ctx| {
                node.as_any_mut()
                    .downcast_mut::<LogicalJoin>()
                    .expect("the join just built")
                    .attach_on_conds(ctx, &on_condition)
            })?;
            return Ok(join_id);
        } else if join_plan.join_type == JoinType::InnerJoin {
            // If a inner join without "ON" or "USING" clause, it's a cartesian
            // product over the join tables.
            join_plan.cartesian_join = true;
        }

        Ok(ctx.arena.alloc(Box::new(join_plan)))
    }

    /// TableHints returns the table hints of the query block being built. The builder keeps
    /// no hint stack: `buildSelect` stops at a non-empty hint list (`blocked-by:D02.2` /
    /// D13.3), so the hints Go pushes for the current SELECT are always empty, which is what
    /// this returns (Go returns nil only outside any SELECT, where no join is built).
    // go: PlanBuilder.TableHints L2 (no hint stack: the current SELECT's hints are always empty, see buildSelect)
    fn table_hints(&self) -> Option<PlanHints> {
        Some(PlanHints::default())
    }

    /// buildUsingClause eliminate the redundant columns and ordering columns based
    /// on the "USING" clause.
    ///
    /// According to the standard SQL, columns are ordered in the following way:
    ///  1. coalesced common columns of "leftPlan" and "rightPlan", in the order they
    ///     appears in "leftPlan".
    ///  2. the rest columns in "leftPlan", in the order they appears in "leftPlan".
    ///  3. the rest columns in "rightPlan", in the order they appears in "rightPlan".
    ///
    /// L2: the filter keeps the USING order (M-MAPORDER, see `coalesce_common_columns`); the
    /// `inUpdateStmt || inDeleteStmt` branch is the write path's (UPDATE / DELETE planning,
    /// D09.15), which this read-only builder never runs, so it has neither flag.
    // go: PlanBuilder.buildUsingClause L2 (ordered filter; the UPDATE / DELETE branch is the write path's)
    fn build_using_clause(
        &mut self,
        ctx: &PlanCtx<'_>,
        p: &mut LogicalJoin,
        left_plan: PlanId,
        right_plan: PlanId,
        join: &ast::Join,
    ) -> Result<(), Error> {
        let mut filter: Vec<(String, bool)> = Vec::with_capacity(join.using.len());
        for col in &join.using {
            let l = col.name.l();
            if !filter.iter().any(|(c, _)| *c == l) {
                filter.push((l, true));
            }
        }
        self.coalesce_common_columns(ctx, p, left_plan, right_plan, join.tp, Some(filter))?;
        // We do not need to coalesce columns for update and delete: not built here (see above).
        Ok(())
    }

    /// buildNaturalJoin builds natural join output schema. It finds out all the common columns
    /// then using the same mechanism as buildUsingClause to eliminate redundant columns and build join conditions.
    /// According to standard SQL, producing this display order:
    ///
    ///  All the common columns
    ///  Every column in the first (left) table that is not a common column
    ///  Every column in the second (right) table that is not a common column
    // go: PlanBuilder.buildNaturalJoin L2 (the UPDATE / DELETE branch is the write path's, see buildUsingClause)
    fn build_natural_join(
        &mut self,
        ctx: &PlanCtx<'_>,
        p: &mut LogicalJoin,
        left_plan: PlanId,
        right_plan: PlanId,
        join: &ast::Join,
    ) -> Result<(), Error> {
        self.coalesce_common_columns(ctx, p, left_plan, right_plan, join.tp, None)?;
        // We do not need to coalesce columns for update and delete: not built here.
        Ok(())
    }

    /// coalesceCommonColumns is used by buildUsingClause and buildNaturalJoin. The filter is used by buildUsingClause.
    ///
    /// L2:
    /// - The filter is an ordered list of `(column, not yet found)` in USING order instead of
    ///   a map (M-MAPORDER): with several unknown USING columns Go reports any one of them;
    ///   this reports the first in USING order.
    /// - Go moves the common columns in place with `copy` on the cloned column slices; for a
    ///   RIGHT JOIN `lColumns` is the same array as `rsc.Columns`. Here the moves are
    ///   `rotate_right` on the same two vectors and the swap exchanges references to them,
    ///   so `lc` is always the original left table's column and `rc` the right one's.
    /// - `b.nameMapCTE` is always empty (CTEs, D09.5); `TableIsView` returns a `blocked`
    ///   lookup error instead of treating it as "not a view".
    /// - Go restores `checkColPriv` in a `defer`; here a drop guard (M-DEFER).
    // go: PlanBuilder.coalesceCommonColumns L2 (see above: ordered filter, no CTE names, drop guard)
    fn coalesce_common_columns(
        &mut self,
        ctx: &PlanCtx<'_>,
        p: &mut LogicalJoin,
        left_plan: PlanId,
        right_plan: PlanId,
        join_tp: ast::JoinType,
        filter: Option<Vec<(String, bool)>>,
    ) -> Result<(), Error> {
        let mut lsc = schema_of(&ctx.arena, left_plan).clone_schema();
        let mut rsc = schema_of(&ctx.arena, right_plan).clone_schema();
        if join_tp == ast::JoinType::LeftJoin {
            let len = rsc.len();
            reset_not_null_flag(&mut rsc, 0, len);
        } else if join_tp == ast::JoinType::RightJoin {
            let len = lsc.len();
            reset_not_null_flag(&mut lsc, 0, len);
        }
        let mut lsc_columns = std::mem::take(&mut lsc.columns);
        let mut rsc_columns = std::mem::take(&mut rsc.columns);
        let mut l_names: NameSlice = names_of(&ctx.arena, left_plan).clone();
        let mut r_names: NameSlice = names_of(&ctx.arena, right_plan).clone();
        let right_join = join_tp == ast::JoinType::RightJoin;
        if right_join {
            std::mem::swap(&mut l_names, &mut r_names);
        }

        // Check using clause with ambiguous columns.
        if let Some(filter) = &filter {
            let check_ambiguous = |names: &NameSlice| -> Result<(), Error> {
                let mut column_name_in_filter: HashSet<&str> = HashSet::new();
                for name in names {
                    let l = name.col_name.l.as_str();
                    if !filter.iter().any(|(c, _)| c == l) {
                        continue;
                    }
                    if !column_name_in_filter.insert(l) {
                        return Err(ErrAmbiguous
                            .gen_with_stack_by_args(go_args![l.to_owned(), "from clause"]));
                    }
                }
                Ok(())
            };
            check_ambiguous(&l_names)?;
            check_ambiguous(&r_names)?;
        } else {
            let (l_columns, r_columns) = if right_join {
                (&rsc_columns, &lsc_columns)
            } else {
                (&lsc_columns, &rsc_columns)
            };
            // Even with no using filter, we still should check the checkAmbiguous name before we try to find the common column from both side.
            // (t3 cross join t4) natural join t1
            //  t1 natural join (t3 cross join t4)
            // t3 and t4 may generate the same name column from cross join.
            // for every common column of natural join, the name from right or left should be exactly one.
            let mut common_names: Vec<&str> = Vec::with_capacity(l_names.len());
            let mut l_name_map: HashMap<&str, usize> = HashMap::with_capacity(l_names.len());
            let mut r_name_map: HashMap<&str, usize> = HashMap::with_capacity(r_names.len());
            for (i, name) in l_names.iter().enumerate() {
                // Natural join should ignore _tidb_rowid
                if name.col_name.l == "_tidb_rowid" {
                    continue;
                }
                if l_columns[i].is_hidden {
                    continue;
                }
                // record left map
                *l_name_map.entry(name.col_name.l.as_str()).or_insert(0) += 1;
            }
            for (i, name) in r_names.iter().enumerate() {
                // Natural join should ignore _tidb_rowid
                if name.col_name.l == "_tidb_rowid" {
                    continue;
                }
                if r_columns[i].is_hidden {
                    continue;
                }
                // record right map
                *r_name_map.entry(name.col_name.l.as_str()).or_insert(0) += 1;
                // check left map
                if let Some(&cnt) = l_name_map.get(name.col_name.l.as_str()) {
                    if cnt > 1 {
                        return Err(ErrAmbiguous.gen_with_stack_by_args(go_args![
                            name.col_name.l.clone(),
                            "from clause"
                        ]));
                    }
                    common_names.push(name.col_name.l.as_str());
                }
            }
            // check right map
            for common_name in common_names {
                if r_name_map.get(common_name).copied().unwrap_or(0) > 1 {
                    return Err(ErrAmbiguous
                        .gen_with_stack_by_args(go_args![common_name.to_owned(), "from clause"]));
                }
            }
        }

        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let mut b = RestoreCheckColPriv {
            b: self,
            origin: origin_col_priv,
        };
        b.coalesce_common_columns_rest(
            ctx,
            p,
            join_tp,
            filter,
            &mut lsc_columns,
            &mut rsc_columns,
            l_names,
            r_names,
        )
    }

    /// The part of `coalesceCommonColumns` after `checkColPriv` is switched (its `defer` is
    /// the caller's [`RestoreCheckColPriv`] guard).
    // go-part: PlanBuilder.coalesceCommonColumns
    #[allow(clippy::too_many_arguments)]
    fn coalesce_common_columns_rest(
        &mut self,
        ctx: &PlanCtx<'_>,
        p: &mut LogicalJoin,
        join_tp: ast::JoinType,
        mut filter: Option<Vec<(String, bool)>>,
        lsc_columns: &mut Vec<Arc<Column>>,
        rsc_columns: &mut Vec<Arc<Column>>,
        mut l_names: NameSlice,
        mut r_names: NameSlice,
    ) -> Result<(), Error> {
        let right_join = join_tp == ast::JoinType::RightJoin;
        let (l_columns, r_columns) = if right_join {
            (&mut *rsc_columns, &mut *lsc_columns)
        } else {
            (&mut *lsc_columns, &mut *rsc_columns)
        };
        // Find out all the common columns and put them ahead.
        let mut common_len = 0usize;
        for i in 0..l_names.len() {
            // Natural join should ignore _tidb_rowid
            if l_names[i].col_name.l == "_tidb_rowid" {
                continue;
            }
            // Hidden columns are internal-only and shuold not participate in NATURAL/USING column matching.
            if l_columns[i].is_hidden {
                continue;
            }
            for j in common_len..r_names.len() {
                if r_columns[j].is_hidden {
                    continue;
                }
                if l_names[i].col_name.l != r_names[j].col_name.l {
                    continue;
                }

                if let Some(filter) = filter.as_mut().filter(|f| !f.is_empty()) {
                    let entry = filter.iter_mut().find(|(c, _)| *c == l_names[i].col_name.l);
                    match entry {
                        Some((_, not_found)) if *not_found => {
                            // Mark this column exist.
                            *not_found = false;
                        }
                        _ => break,
                    }
                }

                l_columns[common_len..=i].rotate_right(1);
                l_names[common_len..=i].rotate_right(1);
                r_columns[common_len..=j].rotate_right(1);
                r_names[common_len..=j].rotate_right(1);

                for field_name in [&l_names[common_len], &r_names[common_len]] {
                    let mut col_name = (
                        field_name.orig_col_name.clone(),
                        field_name.orig_tbl_name.clone(),
                        field_name.db_name.clone(),
                    );
                    if let Some(is) = &self.is
                        && table_is_view(&*is.0, &field_name.db_name, &field_name.tbl_name)?
                    {
                        col_name.0 = field_name.col_name.clone();
                        col_name.1 = field_name.tbl_name.clone();
                    }
                    let (name, table, schema) = col_name;
                    if !name.l.is_empty() && !table.l.is_empty() && !schema.l.is_empty() {
                        // The column privilege is checked in the definition of CTE: b.nameMapCTE
                        // is always empty here (CTEs, D09.5).
                        self.append_col_names_to_visit_info(&[VisitedColumn {
                            schema: schema.l,
                            table: table.l,
                            name: name.l,
                        }]);
                    }
                }

                common_len += 1;
                break;
            }
        }

        if let Some(filter) = &filter
            && !filter.is_empty()
            && filter.len() != common_len
        {
            for (col, not_exist) in filter {
                if *not_exist {
                    return Err(ErrUnknownColumn
                        .gen_with_stack_by_args(go_args![col.clone(), "from clause"]));
                }
            }
        }

        let mut schema_cols: Vec<Arc<Column>> =
            Vec::with_capacity(l_columns.len() + r_columns.len() - common_len);
        schema_cols.extend(l_columns.iter().cloned());
        schema_cols.extend(r_columns[common_len..].iter().cloned());
        let mut names: NameSlice = Vec::with_capacity(schema_cols.len());
        names.extend(l_names.iter().cloned());
        names.extend(r_names[common_len..].iter().cloned());

        let mut conds: Vec<ExprRef> = Vec::with_capacity(common_len);
        for i in 0..common_len {
            let (lc, rc) = (&lsc_columns[i], &rsc_columns[i]);
            let cond = new_function(
                &*ctx.expr_ctx,
                ast::funcs::EQ,
                new_field_type(TypeTiny),
                vec![
                    Arc::new(PExpr::Column(Column::clone(lc))),
                    Arc::new(PExpr::Column(Column::clone(rc))),
                ],
            )?;
            conds.push(cond);
            if let Some(full_schema) = &p.full_schema {
                // since FullSchema is derived from left and right schema in upper layer, so rc/lc must be in FullSchema.
                let idx = if right_join {
                    full_schema.column_index(lc)
                } else {
                    full_schema.column_index(rc)
                };
                let name = &mut p.full_names[usize::try_from(idx).expect("in FullSchema")];
                // FullNames are this join's own copies (buildJoin), never shared (M-FT).
                Arc::make_mut(name).redundant = true;
            }
        }

        p.sp.set_schema(Some(new_schema(schema_cols)));
        p.sp.set_output_names(names);

        conds.append(&mut p.other_conditions);
        p.other_conditions = conds;

        Ok(())
    }

    /// buildJoinFuncDepend builds the functional dependencies of the top join's ON
    /// equalities for `ONLY_FULL_GROUP_BY`. Go's nil map is an empty map.
    // go: PlanBuilder.buildJoinFuncDepend L1
    fn build_join_func_depend(
        &self,
        arena: &PlanArena,
        p: PlanId,
        from: ResultSetRef<'_>,
    ) -> Result<HashMap<FieldNamePtr, FieldNamePtr>, Error> {
        let x: &ast::Join = match from {
            ResultSetRef::Join(x) => x,
            ResultSetRef::Node(ast::ResultSetNode::Join(x)) => x,
            ResultSetRef::Node(_) => return Ok(HashMap::new()),
        };
        let Some(on) = &x.on else {
            return Ok(HashMap::new());
        };
        let on_conditions = split_where(Some(&on.expr));
        let mut col_depend_map = HashMap::with_capacity(on_conditions.len());
        for cond in on_conditions {
            let Some((mut l_col, mut r_col)) = build_func_depend_col(arena, p, cond)? else {
                continue;
            };
            let l_tbl = match &x.left {
                Some(left) => {
                    let l_name = names_of(arena, p)
                        .iter()
                        .find(|n| field_name_ptr(n) == l_col)
                        .expect("buildFuncDependCol returns an output name of p");
                    self.tbl_info_from_col(ResultSetRef::Node(left), l_name)?
                }
                None => None,
            };
            if l_tbl.is_none() {
                std::mem::swap(&mut l_col, &mut r_col);
            }
            match x.tp {
                ast::JoinType::CrossJoin => {
                    col_depend_map.insert(l_col, r_col);
                    col_depend_map.insert(r_col, l_col);
                }
                ast::JoinType::LeftJoin => {
                    col_depend_map.insert(r_col, l_col);
                }
                ast::JoinType::RightJoin => {
                    col_depend_map.insert(l_col, r_col);
                }
                // Go's switch has no other case (a FULL join stops in buildJoin first).
                ast::JoinType::FullJoin => {}
            }
        }
        Ok(col_depend_map)
    }

    /// buildSelection.
    // go: PlanBuilder.buildSelection L1 [M-CTX]
    pub fn build_selection(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        mut p: PlanId,
        where_: &Expr,
        agg_mapper: Option<&AggMapper<'_>>,
    ) -> Result<PlanId, Error> {
        self.opt_flag |= FlagPredicatePushDown;
        self.opt_flag |= FlagDeriveTopNFromWindow;
        self.opt_flag |= FlagPredicateSimplification;
        if self.cur_clause != ClauseCode::HavingClause {
            self.cur_clause = ClauseCode::WhereClause;
        }
        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let res = self.build_selection_inner(ctx, &mut p, where_, agg_mapper);
        self.check_col_priv = origin_col_priv;
        res
    }

    // go-part: PlanBuilder.buildSelection
    fn build_selection_inner(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: &mut PlanId,
        where_: &Expr,
        agg_mapper: Option<&AggMapper<'_>>,
    ) -> Result<PlanId, Error> {
        let conditions = split_where(Some(where_));
        let mut expressions: Vec<ExprRef> = Vec::with_capacity(conditions.len());
        let mut selection = LogicalSelection::init(Vec::new(), ctx, self.get_select_offset());
        for cond in conditions {
            let expr = self.rewrite(
                &*ctx.expr_ctx,
                cond,
                rewrite_plan(&ctx.arena, *p),
                agg_mapper,
                false,
            )?;
            // b.replaceGroupingFunc(expr): the query block has no Expand (ROLLUP is
            // blocked-by:D09.4), so it returns expr.
            let Some(expr) = expr else {
                continue;
            };
            expressions.push(expr);
        }
        let mut cnf_expres: Vec<ExprRef> = Vec::new();
        let use_cache = ctx.vars.stmt_ctx().plan_cache_tracker.use_cache();
        for expr in &expressions {
            let cnf_items = split_cnf_items(expr);
            for item in cnf_items {
                if let Some(con) = item.as_constant()
                    && const_expr_consider_plan_cache(&item, use_cache)
                {
                    let _ = con;
                    let (ret, _) = eval_bool(
                        &*ctx.eval_ctx(),
                        &EvalState::new(),
                        std::slice::from_ref(&item),
                        Row::default(),
                    )?;
                    if ret {
                        continue;
                    }
                    // If there is condition which is always false, return dual plan directly.
                    let mut dual = LogicalTableDual::init(ctx, self.get_select_offset());
                    dual.sp.set_output_names(names_of(&ctx.arena, *p).clone());
                    // dual.SetSchema(p.Schema()): Go shares the *Schema; a copy here (no
                    // rule writes it while both are in the tree, M-FT).
                    dual.sp
                        .set_schema(Some(schema_of(&ctx.arena, *p).clone_schema()));
                    return Ok(ctx.arena.alloc(Box::new(dual)));
                }
                cnf_expres.push(item);
            }
        }
        if cnf_expres.is_empty() {
            return Ok(*p);
        }
        let eval_ctx = ctx.eval_ctx();
        // check expr field types.
        for expr in cnf_expres.iter_mut() {
            if expr.get_type(&*eval_ctx).eval_type() == ETString {
                let mut tp = FieldType::default();
                tp.set_type(TypeDouble);
                tp.set_flag(expr.get_type(&*eval_ctx).get_flag());
                tp.set_flen(MaxRealWidth);
                tp.set_decimal(UnspecifiedLength);
                set_bin_chs_cln_flag(&mut tp);
                *expr = try_push_cast_into_control_function_for_hybrid_type(
                    &*ctx.expr_ctx,
                    Arc::clone(expr),
                    &tp,
                );
            }
        }
        selection.conditions = cnf_expres;
        selection.set_children(vec![*p]);
        Ok(ctx.arena.alloc(Box::new(selection)))
    }

    /// buildProjectionFieldNameFromColumns builds the field name, table name and database name when field expression is a column reference.
    // go: PlanBuilder.buildProjectionFieldNameFromColumns L1
    fn build_projection_field_name_from_columns(
        &self,
        orig_field: &SelectFieldW<'_>,
        col_name_field: &ast::ColumnName,
        name: &FieldName,
    ) -> (CIStr, CIStr, CIStr, CIStr, CIStr) {
        let (orig_tbl_name, orig_col_name, db_name) = (
            name.orig_tbl_name.clone(),
            name.orig_col_name.clone(),
            name.db_name.clone(),
        );
        let col_name = if orig_field.as_name.l().is_empty() {
            ci(&col_name_field.name)
        } else {
            ci(&orig_field.as_name)
        };
        // tblName is the zero value here, so Go always takes the first branch.
        let tbl_name = name.tbl_name.clone();
        (col_name, orig_col_name, tbl_name, orig_tbl_name, db_name)
    }

    /// buildProjectionFieldNameFromExpressions builds the field name when field expression is a normal expression.
    // go: PlanBuilder.buildProjectionFieldNameFromExpressions L1
    fn build_projection_field_name_from_expressions(
        &self,
        field: &SelectFieldW<'_>,
    ) -> Result<CIStr, Error> {
        let fexpr = field.expr().expect("field expression");
        if let Expr::Aggregate(agg) = fexpr
            && agg.f == ast::funcs::AggFuncFirstRow
        {
            // When the query is select t.a from t group by a; The Column Name should be a but not t.a;
            if let Some(Expr::Column(c)) = agg.args.first() {
                return Ok(ci(&c.name));
            }
        }

        let inner_expr = get_inner_from_parentheses_and_unary_plus(fexpr);
        // When used to produce a result set column, NAME_CONST() causes the column to have the given name.
        // See https://dev.mysql.com/doc/refman/5.7/en/miscellaneous-functions.html#function_name-const for details
        if let Expr::FuncCall(func_call) = inner_expr
            && func_call.fn_name.l() == ast::funcs::NameConst
        {
            // evalAstExpr(b.ctx.GetExprCtx(), funcCall.Args[0]) is a boundary stub (the
            // plan-less rewriter, D09.2 part 2): its error is the boundary, not Go's
            // ErrWrongArguments of a failed evaluation.
            let e = eval_ast_expr().expect_err("a boundary stub");
            if blocked_by(&e).is_some() {
                return Err(e);
            }
            return Err(ErrWrongArguments.gen_with_stack_by_args(go_args!["NAME_CONST"]));
        }
        let Expr::Value(value_expr) = inner_expr else {
            // Non-literal: Output as inputed, except that comments need to be removed.
            return Ok(CIStr::new(&trim_spec_field_comments(&field.text)));
        };

        // Literal: Need special processing
        match &value_expr.datum {
            ast::Datum::Str(s) => {
                let mut proj_name: &str = s;
                let proj_offset = value_expr.projection_offset;
                if proj_offset >= 0 {
                    proj_name = proj_name.get(..proj_offset as usize).unwrap_or(proj_name);
                }
                // See #3686, #3994:
                // For string literals, string content is used as column name. Non-graph initial characters are trimmed.
                let field_name = proj_name.trim_start_matches(|r: char| !is_one_of(&RangeGraph, r));
                Ok(CIStr::new(field_name))
            }
            // See #4053, #3685
            ast::Datum::Null => Ok(CIStr::new("NULL")),
            // Don't rewrite BIT literal or HEX literals
            ast::Datum::Hex(_) | ast::Datum::Bit(_) => Ok(CIStr::new(&field.text)),
            // See #9683
            // TRUE or FALSE can be a int64
            ast::Datum::Bool(b) => Ok(CIStr::new(if *b { "TRUE" } else { "FALSE" })),
            _ => {
                let field_name = field.text.trim_start_matches(['\t', '\n', ' ', '+', '(']);
                let field_name = field_name.trim_end_matches(['\t', '\n', ' ', ')']);
                Ok(CIStr::new(field_name))
            }
        }
    }

    /// buildProjectionField builds the field object according to SelectField in projection.
    /// Correlated columns exist only in subqueries (D09.6), so the correlated-column arm is
    /// not here. A redundant column of a NATURAL / USING join that is missing from the join's
    /// full schema panics, as Go's nil name does.
    // go: PlanBuilder.buildProjectionField L2 (correlated columns are subqueries')
    fn build_projection_field(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        field: &SelectFieldW<'_>,
        expr: Option<&ExprRef>,
    ) -> Result<(Option<Arc<Column>>, FieldName), Error> {
        let inner_node = get_inner_from_parentheses_and_unary_plus(field.expr().expect("expr"));
        let col = expr.and_then(|e| e.as_column());
        let only_col_name = |col_name: CIStr| {
            (
                col_name,
                CIStr::default(),
                CIStr::default(),
                CIStr::default(),
                CIStr::default(),
            )
        };
        let (col_name, orig_col_name, tbl_name, orig_tbl_name, db_name) =
            if let (Expr::Column(col_name_field), Some(col)) = (inner_node, col) {
                // Field is a column reference.
                let idx = schema_of(&ctx.arena, p).column_index(col);
                // The column maybe the one from join's redundant part.
                let name = if idx == -1 {
                    find_col_from_natural_using_join(&ctx.arena, p, col)
                        .expect("a column of the redundant side of a NATURAL / USING join")
                } else {
                    Arc::clone(&names_of(&ctx.arena, p)[idx as usize])
                };
                self.build_projection_field_name_from_columns(field, col_name_field, &name)
            } else if let Expr::Column(_) = inner_node {
                // !isCol: a correlated column (subqueries, D09.6) or another expression.
                if !field.as_name.l().is_empty() {
                    only_col_name(ci(&field.as_name))
                } else {
                    only_col_name(self.build_projection_field_name_from_expressions(field)?)
                }
            } else if !field.as_name.l().is_empty() {
                // Field has alias.
                only_col_name(ci(&field.as_name))
            } else {
                // Other: field is an expression.
                only_col_name(self.build_projection_field_name_from_expressions(field)?)
            };
        let name = FieldName {
            tbl_name,
            orig_tbl_name,
            col_name,
            orig_col_name,
            db_name,
            ..FieldName::default()
        };
        if let Some(col) = col {
            return Ok((Some(Arc::new(col.clone())), name));
        }
        let Some(expr) = expr else {
            return Ok((None, name));
        };
        // invalid unique id
        let correlated_col_unique_id = 0;
        // for expr projection, we should record the map relationship <hashcode, uniqueID> down.
        let new_col = Column {
            unique_id: ctx.vars.alloc_plan_column_id(),
            ret_type: Arc::new(expr.get_type(&*ctx.eval_ctx()).into_owned()),
            correlated_col_unique_id,
            ..Column::default()
        };
        if ctx.vars.optimizer_enable_new_only_full_group_by_check {
            // b.ctx.GetSessionVars().MapHashCode2UniqueID4ExtendedCol[...] = newCol.UniqueID
            // glue: blocked-by:D09.14 (the new ONLY_FULL_GROUP_BY check: functional dependencies, pkg/planner/funcdep)
            return Err(blocked(
                "D09.14",
                "tidb_enable_new_only_full_group_by_check (functional dependencies)",
            ));
        }
        new_col.collation_info.set_coercibility(expr.coercibility());
        Ok((Some(Arc::new(new_col)), name))
    }

    /// preprocessUserVarTypes: rewrites the user variable assignments (`@a := expr`) of the
    /// select fields first (Go's `userVarTypeProcessor`). The rewriter's user variables are
    /// `blocked-by:D08.9` (D09.2 spec §4).
    // go: PlanBuilder.preprocessUserVarTypes L1 [M-CTX]
    fn preprocess_user_var_types(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        fields: &[SelectFieldW<'_>],
        mapper: Option<&AggMapper<'_>>,
    ) -> Result<(), Error> {
        let mut processor = UserVarTypeProcessor { plan: p, mapper };
        for field in fields {
            processor.accept(self, ctx, field.expr().expect("expr"))?;
        }
        Ok(())
    }

    /// buildProjection returns a Projection plan and non-aux columns length.
    // go: PlanBuilder.buildProjection L1 [M-CTX]
    #[allow(clippy::too_many_arguments)]
    pub fn build_projection(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        fields: &[SelectFieldW<'ast>],
        mapper: Option<&AggMapper<'_>>,
        mut window_mapper: Option<&mut WindowMapper<'ast>>,
        consider_window: bool,
        expand_generate_column: bool,
    ) -> Result<(PlanId, Vec<ExprRef>, usize), Error> {
        self.preprocess_user_var_types(ctx, p, fields, mapper)?;
        self.opt_flag |= FlagEliminateProjection;
        self.cur_clause = ClauseCode::FieldList;
        let mut proj = LogicalProjection::init(
            Vec::with_capacity(fields.len()),
            ctx,
            self.get_select_offset(),
        );
        let mut schema = new_schema(Vec::with_capacity(fields.len()));
        let mut old_len = 0;
        let mut new_names: NameSlice = Vec::with_capacity(fields.len());
        for (i, field) in fields.iter().enumerate() {
            if !field.auxiliary {
                old_len += 1;
            }

            let fexpr = field.expr().expect("expr");
            let is_window_func_field = has_window_flag(fexpr);
            // Although window functions occurs in the select fields, but it has to be processed after having clause.
            // So when we build the projection for select fields, we need to skip the window function.
            // When `considerWindow` is false, we will only build fields for non-window functions, so we add fake placeholders.
            // for window functions. These fake placeholders will be erased in column pruning.
            // When `considerWindow` is true, all the non-window fields have been built, so we just use the schema columns.
            if consider_window && !is_window_func_field {
                let col = Arc::clone(&schema_of(&ctx.arena, p).columns[i]);
                proj.exprs
                    .push(PExpr::Column(Column::clone(&col)).into_ref());
                schema.append([col]);
                new_names.push(Arc::clone(&names_of(&ctx.arena, p)[i]));
                continue;
            } else if !consider_window && is_window_func_field {
                let expr = PExpr::Constant(new_zero()).into_ref();
                proj.exprs.push(Arc::clone(&expr));
                let (col, name) = self.build_projection_field(ctx, p, field, Some(&expr))?;
                schema.append(col);
                new_names.push(Arc::new(name));
                continue;
            }
            let origin_col_priv = self.check_col_priv;
            if field.is_unfold_from_wild_card {
                self.check_col_priv = CheckSelectColumnPrivOption::ReportTableErrOption;
            } else {
                self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
            }
            let new_expr = self.rewrite_with_preprocess(
                &*ctx.expr_ctx,
                fexpr,
                rewrite_plan(&ctx.arena, p),
                mapper,
                window_mapper.as_deref(),
                true,
            );
            self.check_col_priv = origin_col_priv;
            let new_expr = new_expr?.expect("asScalar rewrite returns an expression");

            // for case: select a+1, b, sum(b), grouping(a) from t group by a, b with rollup.
            // the column inside aggregate (only sum(b) here) should be resolved to original source column,
            // while for others, just use expanded columns if exists: a'+ 1, b', group(gid)
            // (b.replaceGroupingFunc: no Expand here, ROLLUP is blocked-by:D09.4.)

            // For window functions in the order by clause, we will append an field for it.
            // We need rewrite the window mapper here so order by clause could find the added field.
            if consider_window
                && is_window_func_field
                && field.auxiliary
                && let Some(FieldExpr::Ast(Expr::Window(window_expr))) = &field.expr
                && let Some(wm) = window_mapper.as_deref_mut()
            {
                wm.insert(ByAddr::new(&**window_expr), i as isize);
            }

            // p = np: outside subqueries the plan never changes (D09.2 spec §3 item 2).
            proj.exprs.push(Arc::clone(&new_expr));

            let (col, name) = self.build_projection_field(ctx, p, field, Some(&new_expr))?;
            schema.append(col);
            new_names.push(Arc::new(name));
        }
        // implicitly project expand grouping set cols, if not used later, it will being pruned out in logical column pruner.
        // (b.implicitProjectGroupingSetCols: no Expand here, ROLLUP is blocked-by:D09.4.)

        proj.sp.set_schema(Some(schema));
        proj.sp.set_output_names(new_names);
        if expand_generate_column {
            // Sometimes we need to add some fields to the projection so that we can use generate column substitute
            // optimization. For example: select a+1 from t order by a+1, with a virtual generate column c as (a+1) and
            // an index on c. We need to add c into the projection so that we can replace a+1 with c.
            collect_generate_column(&ctx.arena, p)?;
        }
        proj.set_children(vec![p]);
        // delay the only-full-group-by-check in create view statement to later query.
        // (b.isCreateView is CREATE VIEW's, D09.15.)
        if ctx.vars.optimizer_enable_new_only_full_group_by_check
            && SQLMode(ctx.vars.sql_mode).has_only_full_group_by()
        {
            // glue: blocked-by:D09.14 (proj.ExtractFD: functional dependencies, pkg/planner/funcdep)
            // log-skip: PlanBuilder.buildProjection:1880 functional dependency maintenance is blocked-by:D09.14
            // log-skip: PlanBuilder.buildProjection:1945 functional dependency maintenance is blocked-by:D09.14
            return Err(blocked(
                "D09.14",
                "the new ONLY_FULL_GROUP_BY check (LogicalProjection.ExtractFD)",
            ));
        }
        let exprs = proj.exprs.clone();
        Ok((ctx.arena.alloc(Box::new(proj)), exprs, old_len))
    }

    // go: PlanBuilder.buildSort L1 [M-CTX]
    pub fn build_sort(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        by_items: &[ast::ByItem],
        agg_mapper: Option<&AggMapper<'_>>,
        window_mapper: Option<&WindowMapper<'_>>,
    ) -> Result<PlanId, Error> {
        self.build_sort_with_check(ctx, p, by_items, agg_mapper, window_mapper, None, 0, false)
    }

    // go: PlanBuilder.buildSortWithCheck L1 [M-CTX]
    #[allow(clippy::too_many_arguments)]
    pub fn build_sort_with_check(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        by_items: &[ast::ByItem],
        agg_mapper: Option<&AggMapper<'_>>,
        window_mapper: Option<&WindowMapper<'_>>,
        proj_exprs: Option<&[ExprRef]>,
        old_len: usize,
        has_distinct: bool,
    ) -> Result<PlanId, Error> {
        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let res = self.build_sort_inner(
            ctx,
            p,
            by_items,
            agg_mapper,
            window_mapper,
            proj_exprs,
            old_len,
            has_distinct,
        );
        self.check_col_priv = origin_col_priv;
        res
    }

    // go-part: PlanBuilder.buildSortWithCheck
    #[allow(clippy::too_many_arguments)]
    fn build_sort_inner(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        by_items: &[ast::ByItem],
        agg_mapper: Option<&AggMapper<'_>>,
        window_mapper: Option<&WindowMapper<'_>>,
        proj_exprs: Option<&[ExprRef]>,
        old_len: usize,
        has_distinct: bool,
    ) -> Result<PlanId, Error> {
        // A LogicalUnionAll (set operations, D09.5) is not built yet.
        self.cur_clause = ClauseCode::OrderByClause;
        let mut sort = LogicalSort::init(Vec::new(), ctx, self.get_select_offset());
        let mut exprs: Vec<ByItems> = Vec::with_capacity(by_items.len());
        for (i, item) in by_items.iter().enumerate() {
            ItemTransformer.accept(&item.expr)?;
            let it = self
                .rewrite_with_preprocess(
                    &*ctx.expr_ctx,
                    &item.expr,
                    rewrite_plan(&ctx.arena, p),
                    agg_mapper,
                    window_mapper,
                    true,
                )?
                .expect("asScalar rewrite returns an expression");
            // for case: select a+1, b, sum(b) from t group by a+1, b with rollup order by a+1.
            // (b.replaceGroupingFunc: no Expand here, ROLLUP is blocked-by:D09.4.)

            // check whether ORDER BY items show up in SELECT DISTINCT fields, see #12442
            if has_distinct && let Some(orig) = proj_exprs {
                self.check_order_by_in_distinct(ctx, item, i, &it, p, orig, old_len)?;
            }

            // p = np: outside subqueries the plan never changes (D09.2 spec §3 item 2).
            exprs.push(ByItems {
                expr: it,
                desc: item.desc,
            });
        }
        sort.by_items = exprs;
        sort.set_children(vec![p]);
        Ok(ctx.arena.alloc(Box::new(sort)))
    }

    // go: PlanBuilder.buildLimit L1 [M-CTX]
    pub fn build_limit(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        src: PlanId,
        limit: &ast::Limit,
    ) -> Result<PlanId, Error> {
        self.opt_flag |= FlagPushDownTopN;
        // flag it if cte contain limit (no CTE is being built, D09.5)
        let (mut count, offset) = extract_limit_count_offset(
            &*ctx.expr_ctx,
            limit,
            ctx.vars.stmt_ctx().in_prepared_plan_building,
        )?;

        if count > u64::MAX - offset {
            count = u64::MAX - offset;
        }
        if offset + count == 0 {
            let mut table_dual = LogicalTableDual::init(ctx, self.get_select_offset());
            table_dual.row_count = 0;
            table_dual
                .sp
                .set_schema(Some(schema_of(&ctx.arena, src).clone_schema()));
            table_dual
                .sp
                .set_output_names(names_of(&ctx.arena, src).clone());
            return Ok(ctx.arena.alloc(Box::new(table_dual)));
        }
        let mut li = LogicalLimit::init(offset, count, ctx, self.get_select_offset());
        // hint.PreferLimitToCop: the statement has no table hints (D13.3).
        li.prefer_limit_to_cop = false;
        li.set_children(vec![src]);
        Ok(ctx.arena.alloc(Box::new(li)))
    }
}

/// A result set node to build: Go's `ast.ResultSetNode` interface, whose `*ast.Join` the
/// Rust AST holds outside the enum in a FROM clause (`TableRefsClause.TableRefs`).
#[derive(Clone, Copy, Debug)]
pub enum ResultSetRef<'a> {
    /// A `*ast.Join`.
    Join(&'a ast::Join),
    /// Another node.
    Node(&'a ast::ResultSetNode),
}

/// `collectGenerateColumn` (the generated column substitution rule, D09.13) collects the
/// virtual generated columns that an index covers. The data source stops at virtual
/// generated columns (a boundary stub), so no data source here has one: the map stays empty.
// glue: the generated column rule's collectGenerateColumn (blocked-by:D09.13 only for virtual generated columns, which the data source does not build yet)
fn collect_generate_column(arena: &PlanArena, lp: PlanId) -> Result<(), Error> {
    let node = arena.get(lp);
    for &child in node.children() {
        collect_generate_column(arena, child)?;
    }
    if let Some(ds) = arena.downcast_ref::<DataSource>(lp)
        && ds
            .table_info
            .columns
            .iter()
            .flatten()
            .any(|c| c.is_generated() && !c.generated_stored)
    {
        return Err(blocked(
            "D09.13",
            "collectGenerateColumn (virtual generated columns)",
        ));
    }
    Ok(())
}

/// userVarTypeProcessor.
struct UserVarTypeProcessor<'m, 'k> {
    plan: PlanId,
    mapper: Option<&'m AggMapper<'k>>,
}

impl UserVarTypeProcessor<'_, '_> {
    /// `Accept` with this visitor: `Enter`, the children unless skipped, `Leave`.
    // glue: ast.Node.Accept (the Rust AST has no visitor, D02.2)
    fn accept(
        &mut self,
        b: &mut PlanBuilder<'_>,
        ctx: &mut PlanCtx<'_>,
        e: &Expr,
    ) -> Result<(), Error> {
        if !self.enter(b, ctx, e)? {
            for c in expr_children(e) {
                self.accept(b, ctx, c)?;
            }
        }
        let _ = self.leave();
        Ok(())
    }

    /// Returns whether to skip the children; Go's `p.err` is the `Result`.
    // go: userVarTypeProcessor.Enter L1
    fn enter(
        &mut self,
        b: &mut PlanBuilder<'_>,
        ctx: &mut PlanCtx<'_>,
        e: &Expr,
    ) -> Result<bool, Error> {
        let Expr::Variable(v) = e else {
            return Ok(false);
        };
        if v.is_system || v.value.is_none() {
            return Ok(true);
        }
        b.rewrite(
            &*ctx.expr_ctx,
            e,
            rewrite_plan(&ctx.arena, self.plan),
            self.mapper,
            true,
        )?;
        Ok(true)
    }

    /// `return in, p.err == nil`: an error has already returned from `enter`.
    // go: userVarTypeProcessor.Leave L1
    fn leave(&self) -> bool {
        true
    }
}

/// itemTransformer transforms ParamMarkerExpr to PositionExpr in the context of ByItem.
/// Its `Leave` returns false, which stops the walk below the root, so Go replaces only a
/// top-level `?` (`ORDER BY ?`).
struct ItemTransformer;

impl ItemTransformer {
    /// `item.Expr.Accept(transformer)`.
    // glue: ast.Node.Accept (the Rust AST has no visitor, D02.2)
    fn accept(&self, e: &Expr) -> Result<(), Error> {
        self.enter(e)?;
        let _ = self.leave();
        Ok(())
    }

    /// The Rust position node has no parameter-marker form (D02.2), so `ORDER BY ?` is a
    /// boundary stub, as in the rewriter (D09.2 spec §4).
    // go: itemTransformer.Enter L2 (ORDER BY ? is blocked-by:D02.2)
    fn enter(&self, e: &Expr) -> Result<(), Error> {
        if let Expr::ParamMarker(_) = e {
            // newNode := expression.ConstructPositionExpr(n)
            // glue: blocked-by:D02.2 (ORDER BY ?: the position node of the Rust AST has no parameter marker)
            return Err(blocked(
                "D02.2",
                "ORDER BY ? (ConstructPositionExpr over a parameter marker)",
            ));
        }
        Ok(())
    }

    // go: itemTransformer.Leave L1
    fn leave(&self) -> bool {
        false
    }
}

/// aggOrderByResolver is currently resolving expressions of order by clause
/// in aggregate function GROUP_CONCAT.
struct AggOrderByResolver<'a> {
    ctx: &'a dyn BuildContext,
    in_execute: bool,
    args: &'a [Expr],
    expr_depth: usize,
}

impl<'a> AggOrderByResolver<'a> {
    /// Walk `expr` with this resolver and return the resolved expression (`retExpr`).
    // glue: ast.Node.Accept for aggOrderByResolver (the Rust AST is immutable)
    fn resolve(&mut self, expr: &'a Expr) -> Result<&'a Expr, Error> {
        self.expr_depth = 0;
        let skip = self.enter(expr)?;
        if !skip {
            // Only top-level PositionExpr / ParamMarker are replaced; child expressions are
            // unchanged (matching Go's aggOrderByResolver which only transforms top-level
            // ParamMarkerExpr and PositionExpr).
            for c in expr_children(expr) {
                let _ = self.resolve_child(c)?;
            }
        }
        self.leave(expr)
    }

    // glue: recursive AST walk helper for aggOrderByResolver.resolve (immutable AST)
    fn resolve_child(&mut self, expr: &'a Expr) -> Result<&'a Expr, Error> {
        let skip = self.enter(expr)?;
        if !skip {
            for c in expr_children(expr) {
                let _ = self.resolve_child(c)?;
            }
        }
        self.leave(expr)
    }

    // go: aggOrderByResolver.Enter L2 (GROUP_CONCAT ORDER BY ? is blocked-by:D02.2: Rust ast::Expr::Position has no parameter marker form)
    fn enter(&mut self, in_node: &'a Expr) -> Result<bool, Error> {
        self.expr_depth += 1;
        if let Expr::ParamMarker(_) = in_node
            && self.expr_depth == 1
        {
            let (_, is_null, is_expected_type) =
                get_uint_from_node(self.ctx, in_node, false, self.in_execute);
            // For constant uint expression in top level, it should be treated as position expression.
            if !is_null && is_expected_type {
                // glue: blocked-by:D02.2 (GROUP_CONCAT ORDER BY ?: Rust ast::Expr::Position has no parameter marker)
                return Err(blocked(
                    "D02.2",
                    "GROUP_CONCAT(... ORDER BY ?) (ConstructPositionExpr over a parameter marker)",
                ));
            }
        }
        Ok(false)
    }

    // go: aggOrderByResolver.Leave L1
    fn leave(&mut self, in_node: &'a Expr) -> Result<&'a Expr, Error> {
        if let Expr::Position(n) = in_node {
            let pos_expr = PositionExpr::from_num(*n);
            let (pos, is_null) = pos_from_position_expr(self.ctx, &pos_expr)?;
            if is_null {
                return Ok(in_node);
            }
            if pos < 1 || pos as usize > self.args.len() {
                let err_pos = pos.to_string();
                return Err(ErrUnknownColumn.fast_gen_by_args(go_args![err_pos, "order clause"]));
            }
            return Ok(&self.args[pos as usize - 1]);
        }
        Ok(in_node)
    }
}

/// Walk `expr` with an [`AggregateFuncExtractor`].
// glue: ast.Node.Accept for AggregateFuncExtractor (the Rust AST has no visitor, D02.2)
fn accept_agg_extractor<'a>(extractor: &mut AggregateFuncExtractor<'a>, expr: &'a Expr) {
    if !extractor.enter(expr) {
        for c in expr_children(expr) {
            accept_agg_extractor(extractor, c);
        }
        match expr {
            Expr::Aggregate(a) => {
                if let Some(o) = &a.order {
                    for item in &o.items {
                        accept_agg_extractor(extractor, &item.expr);
                    }
                }
            }
            Expr::Window(w) => {
                if let Some(pb) = &w.spec.partition_by {
                    for item in &pb.items {
                        accept_agg_extractor(extractor, &item.expr);
                    }
                }
                if let Some(ob) = &w.spec.order_by {
                    for item in &ob.items {
                        accept_agg_extractor(extractor, &item.expr);
                    }
                }
                if let Some(fr) = &w.spec.frame {
                    if let Some(e) = &fr.extent.start.expr {
                        accept_agg_extractor(extractor, e);
                    }
                    if let Some(e) = &fr.extent.end.expr {
                        accept_agg_extractor(extractor, e);
                    }
                }
            }
            _ => {}
        }
    }
    let _ = extractor.leave(expr);
}

impl<'ast> PlanBuilder<'ast> {
    /// buildAggregation builds a LogicalAggregation plan and returns the plan id and
    /// `aggIndexMap` (mapping input index in `agg_func_list` to deduplicated index in
    /// `plan4Agg.agg_funcs`).
    // go: PlanBuilder.buildAggregation L2 (ROLLUP Expand is Part C; correlatedAggMap is Part D; the LogicalApply arm of findJoinFullSchema comes with LogicalApply; immutable AST uses agg_col_replacements)
    pub fn build_aggregation<'a>(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        agg_func_list: &[&'a ast::AggregateFuncExpr],
        gby_items: Vec<ExprRef>,
        fields: &'a [SelectFieldW<'ast>],
        agg_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<(PlanId, HashMap<usize, usize>), Error> {
        self.opt_flag |= FlagBuildKeyInfo;
        self.opt_flag |= FlagPushDownAgg;
        // We may apply aggregation eliminate optimization.
        // So we add the flagMaxMinEliminate to try to convert max/min to topn and flagPushDownTopN to handle the newly added topn operator.
        self.opt_flag |= FlagMaxMinEliminate;
        self.opt_flag |= FlagPushDownTopN;
        // when we eliminate the max and min we may add `is not null` filter.
        self.opt_flag |= FlagPredicatePushDown;
        self.opt_flag |= FlagEliminateAgg;
        self.opt_flag |= FlagEliminateProjection;

        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let res = self.build_aggregation_inner(
            ctx,
            p,
            agg_func_list,
            gby_items,
            fields,
            agg_col_replacements,
        );
        self.check_col_priv = origin_col_priv;
        res
    }

    // go-part: PlanBuilder.buildAggregation
    fn build_aggregation_inner<'a>(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        agg_func_list: &[&'a ast::AggregateFuncExpr],
        gby_items: Vec<ExprRef>,
        fields: &'a [SelectFieldW<'ast>],
        agg_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<(PlanId, HashMap<usize, usize>), Error> {
        if ctx.vars.enable_skew_distinct_agg {
            self.opt_flag |= FlagSkewDistinctAgg;
        }
        // flag it if cte contain aggregation (no CTE is built yet, D09.5)
        // rollupExpand: ROLLUP Expand is Part C (blocked before reaching here).

        let mut plan4_agg = LogicalAggregation::init(
            Vec::with_capacity(agg_func_list.len()),
            Vec::new(),
            ctx,
            self.get_select_offset(),
        );
        // TableHints(): statement has no table hints (D13.3).

        let p_schema_len = schema_of(&ctx.arena, p).len();
        let mut schema4_agg = new_schema(Vec::with_capacity(agg_func_list.len() + p_schema_len));
        let mut names: NameSlice = Vec::with_capacity(agg_func_list.len() + p_schema_len);
        // aggIdxMap maps the old index to new index after applying common aggregation functions elimination.
        let mut agg_index_map: HashMap<usize, usize> = HashMap::new();

        // Build local column-alias replacement lookup (`ByAddr<ColumnName> -> &Expr`) for
        // immutable AST rewriting inside aggregates (D09.4 spec §4.3).
        let col_repl_map: ColReplacements<'_> = agg_col_replacements
            .iter()
            .filter_map(|(k, &idx)| fields.get(idx)?.expr().map(|e| (*k, e)))
            .collect();
        let col_repl = (!col_repl_map.is_empty()).then_some(&col_repl_map);

        let mut all_aggs_first_row = true;
        let in_execute = ctx.vars.stmt_ctx().in_prepared_plan_building;
        for (i, agg_func) in agg_func_list.iter().enumerate() {
            let mut new_arg_list = Vec::with_capacity(agg_func.args.len());
            for arg in &agg_func.args {
                let new_arg = self
                    .rewrite_with_col_replacements(
                        &*ctx.expr_ctx,
                        arg,
                        rewrite_plan(&ctx.arena, p),
                        None,
                        col_repl,
                        true,
                    )?
                    .expect("asScalar rewrite returns an expression");
                new_arg_list.push(new_arg);
            }
            let mut new_func =
                new_agg_func_desc(&*ctx.expr_ctx, &agg_func.f, new_arg_list, agg_func.distinct)?;
            if new_func.base.name != ast::funcs::AggFuncFirstRow {
                all_aggs_first_row = false;
            }
            if let Some(order) = &agg_func.order {
                let true_args = &agg_func.args[..agg_func.args.len().saturating_sub(1)]; // the last argument is SEPARATOR, remove it.
                let mut resolver = AggOrderByResolver {
                    ctx: &*ctx.expr_ctx,
                    in_execute,
                    args: true_args,
                    expr_depth: 0,
                };
                for by_item in &order.items {
                    let ret_expr = resolver.resolve(&by_item.expr)?;
                    let new_by_item = self
                        .rewrite_with_col_replacements(
                            &*ctx.expr_ctx,
                            ret_expr,
                            rewrite_plan(&ctx.arena, p),
                            None,
                            col_repl,
                            true,
                        )?
                        .expect("asScalar rewrite returns an expression");
                    new_func.order_by_items.push(ByItems {
                        expr: new_by_item,
                        desc: by_item.desc,
                    });
                }
            }
            // combine identical aggregate functions
            let mut combined = false;
            {
                let eval_ctx = ctx.eval_ctx();
                for j in 0..i {
                    let old_idx = agg_index_map[&j];
                    if plan4_agg.agg_funcs[old_idx].equal(&*eval_ctx, &new_func) {
                        agg_index_map.insert(i, old_idx);
                        combined = true;
                        break;
                    }
                }
            }
            // create new columns for aggregate functions which show up first
            if !combined {
                let position = plan4_agg.agg_funcs.len();
                agg_index_map.insert(i, position);
                let ret_tp = Arc::clone(&new_func.base.ret_tp);
                plan4_agg.agg_funcs.push(new_func);
                let column = Column {
                    unique_id: ctx.vars.alloc_plan_column_id(),
                    ret_type: ret_tp,
                    ..Column::default()
                };
                schema4_agg.append([Arc::new(column)]);
                names.push(EmptyName.clone());
            }
        }
        let p_cols: Vec<Arc<Column>> = schema_of(&ctx.arena, p).columns.clone();
        let p_names: NameSlice = names_of(&ctx.arena, p).clone();
        for (i, col) in p_cols.iter().enumerate() {
            let new_func = new_agg_func_desc(
                &*ctx.expr_ctx,
                ast::funcs::AggFuncFirstRow,
                vec![PExpr::Column(Column::clone(col)).into_ref()],
                false,
            )?;
            let ret_tp = Arc::clone(&new_func.base.ret_tp);
            plan4_agg.agg_funcs.push(new_func);
            let mut new_col = Column::clone(col);
            new_col.ret_type = ret_tp;
            schema4_agg.append([Arc::new(new_col)]);
            names.push(Arc::clone(&p_names[i]));
        }
        // findJoinFullSchema walks through LogicalSelection wrappers and handles
        // both LogicalJoin and LogicalApply, so USING/NATURAL JOIN redundant columns
        // are included in the aggregate schema regardless of wrapping.
        if let Some((full_schema, full_names)) = find_join_full_schema(&ctx.arena, p) {
            let p_schema = schema_of(&ctx.arena, p);
            for (i, col) in full_schema.columns.iter().enumerate() {
                if p_schema.contains(col) {
                    continue;
                }
                let new_func = new_agg_func_desc(
                    &*ctx.expr_ctx,
                    ast::funcs::AggFuncFirstRow,
                    vec![PExpr::Column(Column::clone(col)).into_ref()],
                    false,
                )?;
                let ret_tp = Arc::clone(&new_func.base.ret_tp);
                plan4_agg.agg_funcs.push(new_func);
                let mut new_col = Column::clone(col);
                new_col.ret_type = ret_tp;
                schema4_agg.append([Arc::new(new_col)]);
                names.push(Arc::clone(&full_names[i]));
            }
        }
        let has_group_by = !gby_items.is_empty();
        for (i, agg_func) in plan4_agg.agg_funcs.iter_mut().enumerate() {
            agg_func.update_not_null_flag4_ret_type(has_group_by, all_aggs_first_row)?;
            Arc::make_mut(&mut schema4_agg.columns[i]).ret_type = Arc::clone(&agg_func.base.ret_tp);
        }
        plan4_agg.sp.set_output_names(names);
        plan4_agg.set_children(vec![p]);
        plan4_agg.group_by_items = gby_items;
        plan4_agg.sp.set_schema(Some(schema4_agg));
        Ok((ctx.arena.alloc(Box::new(plan4_agg)), agg_index_map))
    }

    /// buildDistinct builds a LogicalAggregation for SELECT DISTINCT.
    // go: PlanBuilder.buildDistinct L1 [M-PLAN, M-CTX]
    pub fn build_distinct(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        child: PlanId,
        length: usize,
    ) -> Result<PlanId, Error> {
        self.opt_flag |= FlagBuildKeyInfo;
        self.opt_flag |= FlagPushDownAgg;
        // flag it if cte contain distinct (no CTE is built yet, D09.5)
        let child_qb = ctx.arena.get(child).query_block_offset();
        let child_schema_clone = schema_of(&ctx.arena, child).clone_schema();
        let child_names = names_of(&ctx.arena, child).clone();
        let group_by_items = column2_exprs(&child_schema_clone.columns[..length]);
        let mut plan4_agg = LogicalAggregation::init(
            Vec::with_capacity(child_schema_clone.len()),
            group_by_items,
            ctx,
            child_qb,
        );
        // TableHints(): statement has no table hints (D13.3).
        for col in &child_schema_clone.columns {
            let agg_desc = new_agg_func_desc(
                &*ctx.expr_ctx,
                ast::funcs::AggFuncFirstRow,
                vec![PExpr::Column(Column::clone(col)).into_ref()],
                false,
            )?;
            plan4_agg.agg_funcs.push(agg_desc);
        }
        plan4_agg.set_children(vec![child]);
        let mut agg_schema = child_schema_clone;
        // Distinct will be rewritten as first_row, we reset the type here since the return type
        // of first_row is not always the same as the column arg of first_row.
        for (i, col) in agg_schema.columns.iter_mut().enumerate() {
            Arc::make_mut(col).ret_type = Arc::clone(&plan4_agg.agg_funcs[i].base.ret_tp);
        }
        plan4_agg.sp.set_schema(Some(agg_schema));
        plan4_agg.sp.set_output_names(child_names);
        Ok(ctx.arena.alloc(Box::new(plan4_agg)))
    }

    /// checkOrderByInDistinct checks whether ORDER BY has conflicts with DISTINCT.
    // go: PlanBuilder.checkOrderByInDistinct L1 [M-CTX]
    #[allow(clippy::too_many_arguments)]
    pub fn check_order_by_in_distinct(
        &self,
        ctx: &PlanCtx<'_>,
        by_item: &ast::ByItem,
        idx: usize,
        expr: &ExprRef,
        p: PlanId,
        original_exprs: &[ExprRef],
        length: usize,
    ) -> Result<(), Error> {
        let eval_ctx = ctx.eval_ctx();
        let schema = schema_of(&ctx.arena, p);
        for (j, orig_expr) in original_exprs.iter().enumerate().take(length) {
            // both check original expression & as name
            if expr.equal(&*eval_ctx, orig_expr)
                || expr.equal(
                    &*eval_ctx,
                    &PExpr::Column(Column::clone(&schema.columns[j])),
                )
            {
                return Ok(());
            }
        }

        // Check if referenced columns of expressions in ORDER BY whole match some fields in DISTINCT,
        // both original expression and alias can be referenced.
        let cols = extract_columns(expr);
        'check_referenced: for col in &cols {
            for (j, orig_expr) in original_exprs.iter().enumerate().take(length) {
                if col.equal(orig_expr)
                    || col.equal(&PExpr::Column(Column::clone(&schema.columns[j])))
                {
                    continue 'check_referenced;
                }
            }

            // Failed cases
            if matches!(by_item.expr, Expr::Aggregate(_)) {
                return Err(ErrAggregateInOrderNotSelect
                    .gen_with_stack_by_args(go_args![idx as i64 + 1, "DISTINCT"]));
            }
            return Err(ErrFieldInOrderNotSelect.gen_with_stack_by_args(go_args![
                idx as i64 + 1,
                col.orig_name.clone(),
                "DISTINCT"
            ]));
        }
        Ok(())
    }

    // go: PlanBuilder.extractAggFuncsInExprs L2 (skip map starts empty; correlated subqueries are blocked-by:D09.6)
    pub fn extract_agg_funcs_in_exprs<'a>(
        &self,
        exprs: &'a [Expr],
    ) -> (Vec<&'a ast::AggregateFuncExpr>, AggMapper<'a>) {
        let mut extractor = AggregateFuncExtractor::default();
        for expr in exprs {
            accept_agg_extractor(&mut extractor, expr);
        }
        let agg_list = extractor.agg_funcs;
        let mut total_agg_mapper = AggMapper::with_capacity(agg_list.len());
        for (i, agg) in agg_list.iter().enumerate() {
            total_agg_mapper.insert(ByAddr::new(*agg), i as isize);
        }
        (agg_list, total_agg_mapper)
    }

    // go: PlanBuilder.extractAggFuncsInSelectFields L2 (skip map starts empty; correlated subqueries are blocked-by:D09.6)
    pub fn extract_agg_funcs_in_select_fields<'a>(
        &self,
        fields: &'a [SelectFieldW<'_>],
    ) -> (Vec<&'a ast::AggregateFuncExpr>, AggMapper<'a>) {
        let mut extractor = AggregateFuncExtractor::default();
        for f in fields {
            if let Some(expr) = f.expr() {
                accept_agg_extractor(&mut extractor, expr);
            }
        }
        let agg_list = extractor.agg_funcs;
        let mut total_agg_mapper = AggMapper::with_capacity(agg_list.len());
        for (i, agg) in agg_list.iter().enumerate() {
            total_agg_mapper.insert(ByAddr::new(*agg), i as isize);
        }
        (agg_list, total_agg_mapper)
    }

    // go: PlanBuilder.extractAggFuncsInByItems L2 (skip map starts empty; correlated subqueries are blocked-by:D09.6)
    pub fn extract_agg_funcs_in_by_items<'a>(
        &self,
        by_items: &'a [ast::ByItem],
    ) -> Vec<&'a ast::AggregateFuncExpr> {
        let mut extractor = AggregateFuncExtractor::default();
        for f in by_items {
            accept_agg_extractor(&mut extractor, &f.expr);
        }
        extractor.agg_funcs
    }
}

/// CheckParamTypeInt64orUint64 check param type for plan cache limit, only allow int64 and
/// uint64 now. The Rust parameter marker carries no value: the caller passes the parameter's
/// value (`SessionVars.PlanCacheParams` at the marker's order).
// go: CheckParamTypeInt64orUint64 L2 (the parameter's value instead of the marker)
pub fn check_param_type_int64_or_uint64(val: &tidb_types::datum::Datum) -> (bool, u64) {
    use tidb_types::datum::{KindInt64, KindUint64};
    let k = val.kind();
    if k == KindInt64 {
        let v = val.get_int64();
        if v >= 0 {
            return (true, v as u64);
        }
    } else if k == KindUint64 {
        return (true, val.get_uint64());
    }
    (false, 0)
}

// ---------------------------------------------------------------- HAVING / ORDER BY

/// resolveFromSelectFields returns the index of v in fields
// go: resolveFromSelectFields L1
fn resolve_from_select_fields(
    v: &ast::ColumnName,
    fields: &[SelectFieldW<'_>],
    ignore_as_name: bool,
) -> Result<isize, Error> {
    let mut matched_expr: Option<&ast::ColumnName> = None;
    let mut index: isize = -1;
    for (i, field) in fields.iter().enumerate() {
        if field.auxiliary {
            continue;
        }
        if field.match_column(v, ignore_as_name) {
            let Some(Expr::Column(cur_col)) = field.expr() else {
                return Ok(i as isize);
            };
            match matched_expr {
                None => {
                    matched_expr = Some(cur_col);
                    index = i as isize;
                }
                Some(m) => {
                    if !column_name_match(m, cur_col) && !column_name_match(cur_col, m) {
                        return Err(ErrAmbiguous.gen_with_stack_by_args(go_args![
                            cur_col.name.l(),
                            clause_msg(ClauseCode::FieldList)
                        ]));
                    }
                }
            }
        }
    }
    Ok(index)
}

/// A resolved GROUP BY item expression under an immutable AST (D09.4 spec §4.3): either
/// the original AST expression (`Ast`) or a select field it resolved to (`Field(idx)`).
#[derive(Clone, Copy, Debug)]
pub enum ResolvedExpr<'a> {
    /// Original expression in the statement AST (possibly with inner column-alias
    /// replacements recorded in `gby_col_replacements`).
    Ast(&'a Expr),
    /// Resolved to `fields[idx].expr()`.
    Field(usize),
}

impl<'a> ResolvedExpr<'a> {
    /// Returns the underlying AST expression.
    // glue: accessor for ResolvedExpr
    fn get<'f>(&self, fields: &'f [SelectFieldW<'a>]) -> &'f Expr
    where
        'a: 'f,
    {
        match *self {
            ResolvedExpr::Ast(e) => e,
            ResolvedExpr::Field(idx) => fields[idx].expr().expect("resolved field has expr"),
        }
    }
}

/// havingWindowAndOrderbyExprResolver visits Expr tree.
/// It converts ColumnNameExpr to AggregateFuncExpr and collects AggregateFuncExpr.
///
/// `outerSchemas` / `outerNames` are not here: they are non-empty only inside a subquery
/// (D09.6). Go's `err` field is the `Result` of the walk.
struct HavingWindowAndOrderbyExprResolver<'r, 'ast> {
    in_agg_func: bool,
    in_window_func: bool,
    in_window_spec: bool,
    in_expr: bool,
    p: PlanId,
    arena: &'r PlanArena,
    select_fields: Vec<SelectFieldW<'ast>>,
    agg_mapper: AggMapper<'ast>,
    col_mapper: &'r mut HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    agg_col_replacements: &'r mut HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    gby_items: &'r [ResolvedExpr<'ast>],
    gby_col_replacements: &'r HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    cur_clause: ClauseCode,
    prev_clause: Vec<ClauseCode>,
}

impl<'ast> HavingWindowAndOrderbyExprResolver<'_, 'ast> {
    // go: havingWindowAndOrderbyExprResolver.pushCurClause L1
    fn push_cur_clause(&mut self, new_clause: ClauseCode) {
        self.prev_clause.push(self.cur_clause);
        self.cur_clause = new_clause;
    }

    // go: havingWindowAndOrderbyExprResolver.popCurClause L1
    fn pop_cur_clause(&mut self) {
        self.cur_clause = self.prev_clause.pop().expect("a pushed clause");
    }

    /// `n.Accept(a)`: `Enter`, the children in Go's order, `Leave`. The window
    /// specification of a window function and the ORDER BY of an aggregate are walked after
    /// the arguments, as their `Accept` does.
    // glue: ast.Node.Accept (the Rust AST has no visitor, D02.2)
    fn accept(&mut self, n: &'ast Expr) -> Result<(), Error> {
        if !self.enter(n) {
            for c in expr_children(n) {
                self.accept(c)?;
            }
            match n {
                Expr::Window(w) => self.accept_window_spec(&w.spec)?,
                Expr::Aggregate(a) => {
                    if let Some(o) = &a.order {
                        self.accept_order_by_clause(o)?;
                    }
                }
                _ => {}
            }
        }
        self.leave(n)
    }

    /// `WindowSpec.Accept`: `Enter` sets `inWindowSpec`; PARTITION BY, ORDER BY and the
    /// frame; `Leave` clears it.
    // glue: ast.WindowSpec.Accept with this visitor
    fn accept_window_spec(&mut self, spec: &'ast ast::WindowSpec) -> Result<(), Error> {
        self.in_window_spec = true;
        if let Some(pb) = &spec.partition_by {
            self.push_cur_clause(ClauseCode::PartitionByClause);
            self.accept_by_items(&pb.items)?;
            self.pop_cur_clause();
        }
        if let Some(o) = &spec.order_by {
            self.accept_order_by_clause(o)?;
        }
        if let Some(f) = &spec.frame {
            // FrameClause and FrameBound: Enter's default branch.
            self.in_expr = true;
            for bound in [&f.extent.start, &f.extent.end] {
                if let Some(e) = &bound.expr {
                    self.accept(e)?;
                }
            }
        }
        self.in_window_spec = false;
        Ok(())
    }

    /// `OrderByClause.Accept`: a window's ORDER BY switches the clause.
    // glue: ast.OrderByClause.Accept with this visitor
    fn accept_order_by_clause(&mut self, o: &'ast ast::OrderByClause) -> Result<(), Error> {
        let in_window_spec = self.in_window_spec;
        if in_window_spec {
            self.push_cur_clause(ClauseCode::WindowOrderByClause);
        }
        self.accept_by_items(&o.items)?;
        if in_window_spec {
            self.pop_cur_clause();
        }
        Ok(())
    }

    /// `ByItem.Accept`: `Enter`'s default branch, then the expression.
    // glue: ast.ByItem.Accept with this visitor
    fn accept_by_items(&mut self, items: &'ast [ast::ByItem]) -> Result<(), Error> {
        for item in items {
            self.in_expr = true;
            self.accept(&item.expr)?;
        }
        Ok(())
    }

    /// Enter implements Visitor interface. Returns whether to skip the children.
    // go: havingWindowAndOrderbyExprResolver.Enter L1
    fn enter(&mut self, n: &Expr) -> bool {
        match n {
            Expr::Aggregate(_) => self.in_agg_func = true,
            Expr::Window(_) => self.in_window_func = true,
            Expr::ParamMarker(_) | Expr::Column(_) => {}
            Expr::Subquery(_) | Expr::Exists(_) => {
                // Enter a new context, skip it.
                // For example: select sum(c) + c + exists(select c from t) from t;
                return true;
            }
            _ => self.in_expr = true,
        }
        false
    }

    // go: havingWindowAndOrderbyExprResolver.resolveFromPlan L2 (no LogicalMaxOneRow or LogicalApply arm yet: subqueries and the apply part)
    fn resolve_from_plan(
        &mut self,
        v: &ast::ColumnName,
        p: PlanId,
        resolve_fields_first: bool,
    ) -> Result<isize, Error> {
        let arena = self.arena;
        let mut idx = find_field_name(names_of(arena, p), v)?;
        let (mut schema_cols, mut output_names) =
            (&schema_of(arena, p).columns, names_of(arena, p));
        if idx < 0 {
            // For SQL like `select t2.a from t1 join t2 using(a) where t2.a > 0
            // order by t2.a`, the query plan will be `join->selection->sort`. The
            // schema of selection will be `[t1.a]`, thus we need to recursively
            // retrieve the `t2.a` from the underlying join.
            match arena.get(p).kind() {
                LogicalKind::Limit
                | LogicalKind::Selection
                | LogicalKind::TopN
                | LogicalKind::Sort => {
                    let child = arena.get(p).children()[0];
                    return self.resolve_from_plan(v, child, resolve_fields_first);
                }
                LogicalKind::Join => {
                    let x = arena
                        .downcast_ref::<LogicalJoin>(p)
                        .expect("a Join is a LogicalJoin");
                    if !x.full_names.is_empty() {
                        idx = find_field_name(&x.full_names, v)?;
                        schema_cols = &x
                            .full_schema
                            .as_ref()
                            .expect("a join with FullNames has a FullSchema")
                            .columns;
                        output_names = &x.full_names;
                    }
                }
                // LogicalMaxOneRow and LogicalApply are not built yet (subqueries D09.6,
                // the apply part of the join block).
                _ => {}
            }
            if idx < 0 {
                // nowhere to be found.
                return Ok(-1);
            }
        }
        let idx = idx as usize;
        let col = &schema_cols[idx];
        if col.is_hidden {
            return Err(ErrUnknownColumn.gen_with_stack_by_args(go_args![
                column_name_string(v),
                clause_msg(self.cur_clause)
            ]));
        }
        let name = &output_names[idx];
        let new_col_name = column_name_of(name);
        for (i, field) in self.select_fields.iter().enumerate() {
            if let Some(Expr::Column(c)) = field.expr()
                && column_name_match(c, &new_col_name)
            {
                return Ok(i as isize);
            }
        }
        // From https://github.com/pingcap/tidb/issues/51107
        // You should make the column in the having clause as the correlated column
        // which is not relation with select's fields and GroupBy's fields.
        // For SQLs like:
        //     SELECT * FROM `t1` WHERE NOT (`t1`.`col_1`>= (
        //          SELECT `t2`.`col_7`
        //             FROM (`t1`)
        //             JOIN `t2`
        //             WHERE ISNULL(`t2`.`col_3`) HAVING `t1`.`col_6`>1951988)
        //     ) ;
        //
        // if resolveFieldsFirst is false, the groupby is not nil.
        if resolve_fields_first && self.cur_clause == ClauseCode::HavingClause {
            return Ok(-1);
        }
        let mut sf = SelectFieldW::auxiliary(FieldExpr::Built(Box::new(Expr::Column(Box::new(
            new_col_name,
        )))));
        // appended with new select fields. set them with flag.
        if self.in_agg_func {
            // should skip check in FD for only full group by.
            sf.auxiliary_col_in_agg = true;
        } else if self.cur_clause == ClauseCode::OrderByClause {
            // should skip check in FD for only full group by only when group by item are empty.
            sf.auxiliary_col_in_order_by = true;
        }
        // sf.Expr.SetType(col.GetStaticType()): the AST type of a column name is not read.
        self.select_fields.push(sf);
        Ok(self.select_fields.len() as isize - 1)
    }

    /// Leave implements Visitor interface.
    ///
    /// Inside an aggregate, Go replaces the column name in place with `selectFields[index].Expr`
    /// (`select a+1 as b from t having sum(b) < 0` aggregates `a+1`). Because the Rust AST is
    /// immutable, this records `v -> index` in `agg_col_replacements` for `buildAggregation`
    /// (D09.4 spec §4.3).
    // go: havingWindowAndOrderbyExprResolver.Leave L1
    fn leave(&mut self, n: &'ast Expr) -> Result<(), Error> {
        match n {
            Expr::Aggregate(v) => {
                self.in_agg_func = false;
                self.agg_mapper
                    .insert(ByAddr::new(&**v), self.select_fields.len() as isize);
                let mut f = SelectFieldW::auxiliary(FieldExpr::Ast(n));
                f.as_name = ast::CIStr::new(format!("sel_agg_{}", self.select_fields.len()));
                self.select_fields.push(f);
            }
            Expr::Window(v) => {
                self.in_window_func = false;
                if self.cur_clause == ClauseCode::HavingClause {
                    return Err(ErrWindowInvalidWindowFuncUse
                        .gen_with_stack_by_args(go_args![v.name.to_lowercase()]));
                }
                if self.cur_clause == ClauseCode::OrderByClause {
                    let mut f = SelectFieldW::auxiliary(FieldExpr::Ast(n));
                    f.as_name = ast::CIStr::new(format!("sel_window_{}", self.select_fields.len()));
                    self.select_fields.push(f);
                }
            }
            Expr::Column(v) => return self.leave_column(v),
            _ => {}
        }
        Ok(())
    }

    // go-part: havingWindowAndOrderbyExprResolver.Leave
    fn leave_column(&mut self, v: &'ast ast::ColumnName) -> Result<(), Error> {
        let mut resolve_fields_first = true;
        if self.in_agg_func
            || self.in_window_func
            || self.in_window_spec
            || (self.cur_clause == ClauseCode::OrderByClause && self.in_expr)
            || self.cur_clause == ClauseCode::FieldList
        {
            resolve_fields_first = false;
        }
        if !self.in_agg_func && self.cur_clause != ClauseCode::OrderByClause {
            for item in self.gby_items {
                let mut expr = item.get(&self.select_fields);
                if let Expr::Column(c) = expr
                    && let Some(&idx) = self.gby_col_replacements.get(&node_addr(&**c))
                {
                    expr = self.select_fields[idx]
                        .expr()
                        .expect("resolved field has expr");
                }
                if let Expr::Column(col) = expr
                    && (column_name_match(v, col) || column_name_match(col, v))
                {
                    resolve_fields_first = false;
                    break;
                }
            }
        }
        let mut index: isize;
        if resolve_fields_first {
            index = resolve_from_select_fields(v, &self.select_fields, false)?;
            if index != -1
                && self.cur_clause == ClauseCode::HavingClause
                && self.select_fields[index as usize]
                    .expr()
                    .is_some_and(has_window_flag)
            {
                return Err(ErrWindowInvalidWindowFuncAliasUse
                    .gen_with_stack_by_args(go_args![v.name.o.clone()]));
            }
            if index == -1 {
                if self.cur_clause == ClauseCode::OrderByClause {
                    index = self.resolve_from_plan(v, self.p, resolve_fields_first)?;
                } else if self.cur_clause == ClauseCode::HavingClause && !v.table.l().is_empty() {
                    // For SQLs like:
                    //   select a from t b having b.a;
                    index = self.resolve_from_plan(v, self.p, resolve_fields_first)?;
                    if index != -1 {
                        // For SQLs like:
                        //   select a+1 from t having t.a;
                        let field = &self.select_fields[index as usize];
                        if field.auxiliary {
                            // having can't use auxiliary field
                            index = -1;
                        }
                    }
                } else {
                    index = resolve_from_select_fields(v, &self.select_fields, true)?;
                }
            }
        } else {
            // We should ignore the err when resolving from schema. Because we could resolve successfully
            // when considering select fields. (A boundary is not an error to ignore.)
            index = match self.resolve_from_plan(v, self.p, resolve_fields_first) {
                Ok(i) => i,
                Err(e) if blocked_by(&e).is_some() => return Err(e),
                Err(_) => -1,
            };
            if index == -1
                && self.cur_clause != ClauseCode::FieldList
                && self.cur_clause != ClauseCode::WindowOrderByClause
                && self.cur_clause != ClauseCode::PartitionByClause
            {
                index = resolve_from_select_fields(v, &self.select_fields, false)?;
                if index != -1
                    && self.cur_clause == ClauseCode::HavingClause
                    && self.select_fields[index as usize]
                        .expr()
                        .is_some_and(has_window_flag)
                {
                    return Err(ErrWindowInvalidWindowFuncAliasUse
                        .gen_with_stack_by_args(go_args![v.name.o.clone()]));
                }
            }
        }
        if index == -1 {
            // If we can't find it any where, it may be a correlated columns.
            // (a.outerNames is empty outside subqueries, D09.6.)
            return Err(ErrUnknownColumn.gen_with_stack_by_args(go_args![
                column_name_orig_col_name(v),
                clause_msg(self.cur_clause)
            ]));
        }
        if self.in_agg_func {
            self.agg_col_replacements
                .insert(ByAddr::new(v), index as usize);
            return Ok(());
        }
        self.col_mapper.insert(ByAddr::new(v), index as usize);
        Ok(())
    }
}

/// colResolverForOnlyFullGroupBy visits Expr tree to find out if an Expr tree is an aggregation function.
/// If so, find out the first column name that not in an aggregation function.
struct ColResolverForOnlyFullGroupBy {
    non_agg_cols: Vec<ast::ColumnName>,
    expr_idx: usize,
    non_agg_col_idxs: Vec<usize>,
    has_agg_func_or_any_value: bool,
    first_order_by_agg_col_idx: isize,
    cur_clause: ClauseCode,
}

impl ColResolverForOnlyFullGroupBy {
    /// `Accept` with this visitor.
    // glue: ast.Node.Accept (the Rust AST has no visitor, D02.2)
    fn accept(&mut self, node: &Expr) {
        if !self.enter(node) {
            for c in expr_children(node) {
                self.accept(c);
            }
        }
        let _ = self.leave(node);
    }

    /// Returns whether to skip the children.
    // go: colResolverForOnlyFullGroupBy.Enter L1
    fn enter(&mut self, node: &Expr) -> bool {
        match node {
            Expr::Aggregate(_) => {
                self.has_agg_func_or_any_value = true;
                if self.cur_clause == ClauseCode::OrderByClause {
                    // Go overwrites on every aggregate in ORDER BY, reporting the last one
                    // (go-bug-kept, #428, D09.4 spec §8 item 3).
                    self.first_order_by_agg_col_idx = self.expr_idx as isize;
                }
                return true;
            }
            Expr::FuncCall(t) => {
                // enable function `any_value` in aggregation even `ONLY_FULL_GROUP_BY` is set
                if t.fn_name.l() == ast::funcs::AnyValue {
                    self.has_agg_func_or_any_value = true;
                    return true;
                }
            }
            Expr::Column(t) => {
                self.non_agg_cols.push(ast::ColumnName::clone(t));
                self.non_agg_col_idxs.push(self.expr_idx);
                return true;
            }
            Expr::Subquery(_) => return true,
            _ => {}
        }
        false
    }

    // go: colResolverForOnlyFullGroupBy.Leave L1
    fn leave(&mut self, _node: &Expr) -> bool {
        true
    }
}

/// Identity key for a `*types.FieldName` in `p.OutputNames()` (`Arc::as_ptr`).
type FieldNamePtr = usize;

// glue: pointer identity key for FieldName in ONLY_FULL_GROUP_BY sets
fn field_name_ptr(name: &Arc<FieldName>) -> FieldNamePtr {
    Arc::as_ptr(name) as usize
}

/// Ordered set of `FieldName` entries keyed by pointer identity (`M-MAPORDER`).
#[derive(Default)]
struct OrderedFieldNameSet {
    seen: HashSet<FieldNamePtr>,
    entries: Vec<(FieldNamePtr, Arc<FieldName>)>,
}

impl OrderedFieldNameSet {
    // glue: deterministic insertion helper for OrderedFieldNameSet (M-MAPORDER)
    fn insert(&mut self, name: &Arc<FieldName>) {
        let key = field_name_ptr(name);
        if self.seen.insert(key) {
            self.entries.push((key, Arc::clone(name)));
        }
    }
}

/// ErrExprLoc is for generating the ErrFieldNotInGroupBy error info.
#[derive(Clone, Copy, Debug)]
struct ErrExprLoc {
    offset: usize,
    loc: &'static str,
}

/// Ordered map of `FieldName -> ErrExprLoc` keyed by pointer identity (`M-MAPORDER`):
/// preserves first-insertion order while updating `ErrExprLoc` in place on duplicate keys
/// (matching Go's map overwrite when ORDER BY overwrites SELECT for the same column).
#[derive(Default)]
struct OrderedErrExprLocMap {
    index: HashMap<FieldNamePtr, usize>,
    entries: Vec<(FieldNamePtr, Arc<FieldName>, ErrExprLoc)>,
}

impl OrderedErrExprLocMap {
    // glue: deterministic insert-or-overwrite helper for OrderedErrExprLocMap (M-MAPORDER)
    fn insert(&mut self, name: &Arc<FieldName>, loc: ErrExprLoc) {
        let key = field_name_ptr(name);
        if let Some(&pos) = self.index.get(&key) {
            self.entries[pos].2 = loc;
        } else {
            let pos = self.entries.len();
            self.index.insert(key, pos);
            self.entries.push((key, Arc::clone(name), loc));
        }
    }

    // glue: emptiness check for OrderedErrExprLocMap (M-MAPORDER)
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// colNameResolver collects all columns from an expression node (skipping subqueries and
/// aggregate functions) for `ONLY_FULL_GROUP_BY`.
struct ColNameResolver<'a> {
    arena: &'a PlanArena,
    p: PlanId,
    names: &'a mut OrderedFieldNameSet,
}

impl<'a> ColNameResolver<'a> {
    // glue: ast.Node.Accept for colNameResolver (the Rust AST has no visitor, D02.2)
    fn accept(&mut self, n: &Expr) -> Result<(), Error> {
        if !self.enter(n) {
            for c in expr_children(n) {
                self.accept(c)?;
            }
        }
        self.leave(n)
    }

    // go: colNameResolver.Enter L1
    fn enter(&mut self, in_node: &Expr) -> bool {
        matches!(
            in_node,
            Expr::Column(_) | Expr::Subquery(_) | Expr::Aggregate(_)
        )
    }

    // go: colNameResolver.Leave L1
    fn leave(&mut self, in_node: &Expr) -> Result<(), Error> {
        if let Expr::Column(v) = in_node {
            let output_names = names_of(self.arena, self.p);
            match find_field_name(output_names, v) {
                Ok(idx) if idx >= 0 => {
                    self.names.insert(&output_names[idx as usize]);
                }
                Err(e) if blocked_by(&e).is_some() => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
}

// go: allColFromExprNode L1 [M-MAPORDER]
fn all_col_from_expr_node(
    arena: &PlanArena,
    p: PlanId,
    n: &Expr,
    names: &mut OrderedFieldNameSet,
) -> Result<(), Error> {
    let mut extractor = ColNameResolver { arena, p, names };
    extractor.accept(n)
}

/// Whether `e` satisfies Go's `ast.ValueExpr` interface (`*driver.ValueExpr` or
/// `*driver.ParamMarkerExpr`).
// glue: Go ast.ValueExpr type-assertion helper for extractSingeValueColNamesFromWhere
fn is_ast_value_expr(e: &Expr) -> bool {
    matches!(e, Expr::Value(_) | Expr::ParamMarker(_))
}

// go: addGbyOrSingleValueColName L1
fn add_gby_or_single_value_col_name(
    arena: &PlanArena,
    p: PlanId,
    col_name: &ast::ColumnName,
    gby_or_single_value_col_names: &mut HashSet<FieldNamePtr>,
) -> Result<(), Error> {
    let output_names = names_of(arena, p);
    let idx = match find_field_name(output_names, col_name) {
        Ok(i) => i,
        Err(e) if blocked_by(&e).is_some() => return Err(e),
        Err(_) => return Ok(()),
    };
    if idx < 0 {
        return Ok(());
    }
    gby_or_single_value_col_names.insert(field_name_ptr(&output_names[idx as usize]));
    Ok(())
}

// go: extractSingeValueColNamesFromWhere L1
fn extract_singe_value_col_names_from_where(
    arena: &PlanArena,
    p: PlanId,
    where_: Option<&Expr>,
    gby_or_single_value_col_names: &mut HashSet<FieldNamePtr>,
) -> Result<(), Error> {
    let where_conditions = split_where(where_);
    for cond in where_conditions {
        let Expr::Binary(bin_op_expr) = cond else {
            continue;
        };
        if bin_op_expr.op != ast::Op::EQ {
            continue;
        }
        if let Expr::Column(col_expr) = &bin_op_expr.l {
            // a = value
            if is_ast_value_expr(&bin_op_expr.r) {
                add_gby_or_single_value_col_name(
                    arena,
                    p,
                    col_expr,
                    gby_or_single_value_col_names,
                )?;
            }
            // a = - value
            if let Expr::Unary(u) = &bin_op_expr.r
                && is_ast_value_expr(&u.v)
            {
                add_gby_or_single_value_col_name(
                    arena,
                    p,
                    col_expr,
                    gby_or_single_value_col_names,
                )?;
            }
        } else if let Expr::Column(col_expr) = &bin_op_expr.r {
            // value = a
            if is_ast_value_expr(&bin_op_expr.l) {
                add_gby_or_single_value_col_name(
                    arena,
                    p,
                    col_expr,
                    gby_or_single_value_col_names,
                )?;
            }
            // - value = a
            if let Expr::Unary(u) = &bin_op_expr.l
                && is_ast_value_expr(&u.v)
            {
                add_gby_or_single_value_col_name(
                    arena,
                    p,
                    col_expr,
                    gby_or_single_value_col_names,
                )?;
            }
        }
    }
    Ok(())
}

// go: buildFuncDependCol L1
fn build_func_depend_col(
    arena: &PlanArena,
    p: PlanId,
    cond: &Expr,
) -> Result<Option<(FieldNamePtr, FieldNamePtr)>, Error> {
    let Expr::Binary(bin_op_expr) = cond else {
        return Ok(None);
    };
    if bin_op_expr.op != ast::Op::EQ {
        return Ok(None);
    }
    let Expr::Column(l_col_expr) = &bin_op_expr.l else {
        return Ok(None);
    };
    let Expr::Column(r_col_expr) = &bin_op_expr.r else {
        return Ok(None);
    };
    let output_names = names_of(arena, p);
    let l_idx = find_field_name(output_names, l_col_expr)?;
    let r_idx = find_field_name(output_names, r_col_expr)?;
    if l_idx == -1 {
        return Err(ErrUnknownColumn
            .gen_with_stack_by_args(go_args![column_name_string(l_col_expr), "where clause"]));
    }
    if r_idx == -1 {
        return Err(ErrUnknownColumn
            .gen_with_stack_by_args(go_args![column_name_string(r_col_expr), "where clause"]));
    }
    Ok(Some((
        field_name_ptr(&output_names[l_idx as usize]),
        field_name_ptr(&output_names[r_idx as usize]),
    )))
}

// go: buildWhereFuncDepend L1
fn build_where_func_depend(
    arena: &PlanArena,
    p: PlanId,
    where_: Option<&Expr>,
) -> Result<HashMap<FieldNamePtr, FieldNamePtr>, Error> {
    let where_conditions = split_where(where_);
    let mut col_depend_map = HashMap::with_capacity(2 * where_conditions.len());
    for cond in where_conditions {
        let Some((l_col, r_col)) = build_func_depend_col(arena, p, cond)? else {
            continue;
        };
        col_depend_map.insert(l_col, r_col);
        col_depend_map.insert(r_col, l_col);
    }
    Ok(col_depend_map)
}

// go: checkColFuncDepend L1
fn check_col_func_depend(
    arena: &PlanArena,
    p: PlanId,
    name: &FieldName,
    tbl_info: &TableInfo,
    gby_or_single_value_col_names: &HashSet<FieldNamePtr>,
    where_depend_names: &HashMap<FieldNamePtr, FieldNamePtr>,
    join_depend_names: &HashMap<FieldNamePtr, FieldNamePtr>,
) -> Result<bool, Error> {
    let output_names = names_of(arena, p);
    let tbl_cols = tbl_info.columns.as_deref().unwrap_or(&[]);
    for index in tbl_info.indices.iter().flatten() {
        if !index.unique {
            continue;
        }
        let mut func_depend = true;
        // if all columns of some unique/pri indexes are determined, all columns left are check-passed.
        for index_col in index.columns.iter().flatten() {
            let i_col_info = &tbl_cols[index_col.offset as usize];
            if !has_not_null_flag(i_col_info.get_flag()) {
                func_depend = false;
                break;
            }
            let cn = ast::ColumnName {
                schema: ast::CIStr::new(name.db_name.o.clone()),
                table: ast::CIStr::new(name.tbl_name.o.clone()),
                name: ast::CIStr::new(i_col_info.name.o.clone()),
            };
            let i_idx = match find_field_name(output_names, &cn) {
                Ok(i) => i,
                Err(e) if blocked_by(&e).is_some() => return Err(e),
                Err(_) => {
                    func_depend = false;
                    break;
                }
            };
            if i_idx < 0 {
                func_depend = false;
                break;
            }
            let i_name = field_name_ptr(&output_names[i_idx as usize]);
            if gby_or_single_value_col_names.contains(&i_name) {
                continue;
            }
            if let Some(w_col) = where_depend_names.get(&i_name)
                && gby_or_single_value_col_names.contains(w_col)
            {
                continue;
            }
            if let Some(j_col) = join_depend_names.get(&i_name)
                && gby_or_single_value_col_names.contains(j_col)
            {
                continue;
            }
            func_depend = false;
            break;
        }
        if func_depend {
            return Ok(true);
        }
    }
    let mut primary_func_depend = true;
    let mut has_primary_field = false;
    for col_info in tbl_cols {
        if !has_pri_key_flag(col_info.get_flag()) {
            continue;
        }
        has_primary_field = true;
        let pk_name = ast::ColumnName {
            schema: ast::CIStr::new(name.db_name.o.clone()),
            table: ast::CIStr::new(name.tbl_name.o.clone()),
            name: ast::CIStr::new(col_info.name.o.clone()),
        };
        let p_idx = match find_field_name(output_names, &pk_name) {
            Ok(i) => i,
            Err(e) if blocked_by(&e).is_some() => return Err(e),
            Err(_) => {
                primary_func_depend = false;
                break;
            }
        };
        if p_idx < 0 {
            primary_func_depend = false;
            break;
        }
        let p_col = field_name_ptr(&output_names[p_idx as usize]);
        if gby_or_single_value_col_names.contains(&p_col) {
            continue;
        }
        if let Some(w_col) = where_depend_names.get(&p_col)
            && gby_or_single_value_col_names.contains(w_col)
        {
            continue;
        }
        if let Some(j_col) = join_depend_names.get(&p_col)
            && gby_or_single_value_col_names.contains(j_col)
        {
            continue;
        }
        primary_func_depend = false;
        break;
    }
    Ok(primary_func_depend && has_primary_field)
}

// go: checkExprInGroupByOrIsSingleValue L1 [M-MAPORDER]
#[allow(clippy::too_many_arguments)]
fn check_expr_in_group_by_or_is_single_value<'a>(
    arena: &PlanArena,
    p: PlanId,
    expr: &'a Expr,
    offset: usize,
    loc: &'static str,
    gby_or_single_value_col_names: &HashSet<FieldNamePtr>,
    gby_exprs: &[&'a Expr],
    fields: &'a [SelectFieldW<'_>],
    gby_col_replacements: &HashMap<ByAddr<'_, ast::ColumnName>, usize>,
    not_in_gby_or_single_value_col_names: &mut OrderedErrExprLocMap,
) -> Result<(), Error> {
    if matches!(expr, Expr::Aggregate(_)) {
        return Ok(());
    }
    if let Expr::FuncCall(f) = expr
        && f.fn_name.l() == ast::funcs::Grouping
    {
        // just skip grouping function check here, because later in building plan phase, we
        // will do the grouping function valid check.
        return Ok(());
    }
    if !matches!(expr, Expr::Column(_)) {
        let resolve_gby = |e: &'a Expr| -> &'a Expr {
            if let Expr::Column(c) = e
                && let Some(&idx) = gby_col_replacements.get(&node_addr(&**c))
            {
                return fields[idx].expr().expect("resolved field has expr");
            }
            e
        };
        for &gby_expr in gby_exprs {
            if expression_deep_equal_with(gby_expr, expr, &resolve_gby) {
                return Ok(());
            }
        }
    }
    // Function `any_value` can be used in aggregation, even `ONLY_FULL_GROUP_BY` is set.
    if let Expr::FuncCall(f) = expr
        && f.fn_name.l() == ast::funcs::AnyValue
    {
        return Ok(());
    }
    let mut col_map = OrderedFieldNameSet::default();
    all_col_from_expr_node(arena, p, expr, &mut col_map)?;
    for (key, col) in col_map.entries {
        if !gby_or_single_value_col_names.contains(&key) {
            not_in_gby_or_single_value_col_names.insert(&col, ErrExprLoc { offset, loc });
        }
    }
    Ok(())
}

/// An entry extracted by [`extract_table_list`]: the table's effective schema and name (or
/// alias when `as_name` is true) plus the resolved `TableInfo` if found in `resolve_ctx`.
#[derive(Clone, Debug)]
pub struct ExtractedTable {
    pub schema: CIStr,
    pub name: CIStr,
    pub table_info: Option<Arc<TableInfo>>,
}

/// tableListExtractor extracts all the TableNames from a read-only AST node without mutating
/// the AST or `resolveCtx` (D09.4 spec §2.2, `M-MAPORDER`).
struct TableListExtractor<'a> {
    as_name: bool,
    table_names: Vec<ExtractedTable>,
    resolve_ctx: Option<&'a ContextRef>,
}

impl<'a> TableListExtractor<'a> {
    // glue: recursive extraction helper for TableListExtractor (immutable AST)
    fn inner_extract(&self, node: ResultSetRef<'_>) -> Vec<ExtractedTable> {
        let mut inner = TableListExtractor {
            as_name: self.as_name,
            table_names: Vec::new(),
            resolve_ctx: self.resolve_ctx,
        };
        inner.walk_result_set(node);
        inner.table_names
    }

    // glue: resolveCtx lookup helper for TableListExtractor
    fn lookup_table_info(&self, tn: &ast::TableName) -> Option<Arc<TableInfo>> {
        let rc = self.resolve_ctx?;
        let guard = rc.lock();
        guard.get_table_name(tn).map(|w| Arc::clone(&w.table_info))
    }

    // glue: ast.Node.Accept traversal for TableListExtractor (the Rust AST has no visitor, D02.2)
    fn walk_result_set(&mut self, node: ResultSetRef<'_>) {
        if !self.enter(node) {
            match node {
                ResultSetRef::Join(j) => {
                    if let Some(l) = &j.left {
                        self.walk_result_set(ResultSetRef::Node(l));
                    }
                    if let Some(r) = &j.right {
                        self.walk_result_set(ResultSetRef::Node(r));
                    }
                }
                ResultSetRef::Node(ast::ResultSetNode::Join(j)) => {
                    if let Some(l) = &j.left {
                        self.walk_result_set(ResultSetRef::Node(l));
                    }
                    if let Some(r) = &j.right {
                        self.walk_result_set(ResultSetRef::Node(r));
                    }
                }
                ResultSetRef::Node(ast::ResultSetNode::Select(s)) => {
                    if let Some(from) = &s.from {
                        self.walk_result_set(ResultSetRef::Join(&from.table_refs));
                    }
                }
                _ => {}
            }
        }
        let _ = self.leave(node);
    }

    // go: tableListExtractor.Enter L2 (read-only traversal over ResultSetRef for SELECT/FROM path; avoids Go's temporary resolveCtx mutation)
    fn enter(&mut self, n: ResultSetRef<'_>) -> bool {
        match n {
            ResultSetRef::Node(ast::ResultSetNode::TableName(x)) => {
                self.table_names.push(ExtractedTable {
                    schema: ci(&x.schema),
                    name: ci(&x.name),
                    table_info: self.lookup_table_info(x),
                });
                false
            }
            ResultSetRef::Node(ast::ResultSetNode::TableSource(x)) => {
                match &x.source {
                    ast::ResultSetNode::TableName(s) => {
                        let table_info = self.lookup_table_info(s);
                        if !x.as_name.l().is_empty() && self.as_name {
                            self.table_names.push(ExtractedTable {
                                schema: CIStr::new(""),
                                name: ci(&x.as_name),
                                table_info,
                            });
                        } else {
                            self.table_names.push(ExtractedTable {
                                schema: ci(&s.schema),
                                name: ci(&s.name),
                                table_info,
                            });
                        }
                    }
                    ast::ResultSetNode::Select(s) => {
                        if let Some(from) = &s.from {
                            let inner_list =
                                self.inner_extract(ResultSetRef::Join(&from.table_refs));
                            if let Some(first) = inner_list.into_iter().next() {
                                let inner_table = if !x.as_name.l().is_empty() && self.as_name {
                                    ExtractedTable {
                                        schema: CIStr::new(""),
                                        name: ci(&x.as_name),
                                        table_info: first.table_info,
                                    }
                                } else {
                                    first
                                };
                                self.table_names.push(inner_table);
                            }
                        }
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    // go: tableListExtractor.Leave L1
    fn leave(&mut self, _n: ResultSetRef<'_>) -> bool {
        true
    }
}

/// ExtractTableList is a wrapper for tableListExtractor and removes duplicate TableName.
/// If `as_name` is true, extract AsName prior to OrigName.
// go: ExtractTableList L2 (read-only ResultSetRef traversal and deterministic deduplication order, M-MAPORDER)
pub fn extract_table_list(
    node: ResultSetRef<'_>,
    resolve_ctx: Option<&ContextRef>,
    as_name: bool,
) -> Vec<ExtractedTable> {
    let mut e = TableListExtractor {
        as_name,
        table_names: Vec::new(),
        resolve_ctx,
    };
    e.walk_result_set(node);
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut deduped: Vec<ExtractedTable> = Vec::new();
    for x in e.table_names {
        let k1 = x.schema.l.clone();
        let k2 = x.name.l.clone();
        // allow empty schema name OR empty table name
        if !k1.is_empty() || !k2.is_empty() {
            if let Some(&pos) = index.get(&(k1.clone(), k2.clone())) {
                deduped[pos] = x;
            } else {
                let pos = deduped.len();
                index.insert((k1, k2), pos);
                deduped.push(x);
            }
        }
    }
    deduped
}

/// gbyResolver resolves GROUP BY expressions (aliases and 1-based positions) without
/// mutating the AST (D09.4 spec §4.3).
struct GbyResolver<'r, 'ast> {
    ctx: &'r dyn BuildContext,
    in_execute: bool,
    fields: &'r [SelectFieldW<'ast>],
    names: &'r [Arc<FieldName>],
    err: Option<Error>,
    in_expr: bool,
    is_param: bool,
    expr_depth: usize,
    use_as_value_in_gby: HashSet<ByAddr<'ast, ast::ParamMarkerExpr>>,
    col_replacements: &'r mut HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
}

impl<'ast> GbyResolver<'_, 'ast> {
    /// Walk a top-level GROUP BY item expression and return its resolved form.
    // glue: ast.Node.Accept for gbyResolver (immutable AST)
    fn resolve_item(&mut self, expr: &'ast Expr) -> Result<ResolvedExpr<'ast>, Error> {
        self.in_expr = false;
        self.expr_depth = 0;
        self.is_param = false;
        self.err = None;
        let (resolved, _ok) = self.accept(expr, true)?;
        if let Some(err) = self.err.take() {
            return Err(err);
        }
        Ok(resolved)
    }

    // glue: recursive AST walk helper for gbyResolver (immutable AST)
    fn accept(
        &mut self,
        in_node: &'ast Expr,
        is_root: bool,
    ) -> Result<(ResolvedExpr<'ast>, bool), Error> {
        let skip = self.enter(in_node)?;
        if !skip {
            for c in expr_children(in_node) {
                let (_, ok) = self.accept(c, false)?;
                if !ok {
                    return Ok((ResolvedExpr::Ast(in_node), false));
                }
            }
        }
        self.leave(in_node, is_root)
    }

    // go: gbyResolver.Enter L2 (GROUP BY ? is blocked-by:D02.2: Rust ast::Expr::Position has no parameter marker form)
    fn enter(&mut self, in_node: &'ast Expr) -> Result<bool, Error> {
        self.expr_depth += 1;
        match in_node {
            Expr::Subquery(_) | Expr::CompareSubquery(_) | Expr::Exists(_) => return Ok(true),
            Expr::ParamMarker(n) => {
                self.is_param = true;
                if self.expr_depth == 1 && !self.use_as_value_in_gby.contains(&ByAddr::new(&**n)) {
                    let (_, is_null, is_expected_type) =
                        get_uint_from_node(self.ctx, in_node, false, self.in_execute);
                    // For constant uint expression in top level, it should be treated as position expression.
                    if !is_null && is_expected_type {
                        // glue: blocked-by:D02.2 (GROUP BY ?: Rust ast::Expr::Position has no parameter marker)
                        return Err(blocked(
                            "D02.2",
                            "GROUP BY ? (ConstructPositionExpr over a parameter marker)",
                        ));
                    }
                }
                return Ok(true);
            }
            Expr::Value(_) | Expr::Column(_) | Expr::Paren(_) => {}
            _ => {
                self.in_expr = true;
            }
        }
        Ok(false)
    }

    // go: gbyResolver.Leave L2 (immutable AST records alias/position replacements via ResolvedExpr and gby_col_replacements)
    fn leave(
        &mut self,
        in_node: &'ast Expr,
        is_root: bool,
    ) -> Result<(ResolvedExpr<'ast>, bool), Error> {
        let mut extractor = AggregateFuncExtractor::default();
        match in_node {
            Expr::Column(v) => {
                let (idx, mut err) = match find_field_name(self.names, v) {
                    Ok(i) => (i, None),
                    Err(e) if blocked_by(&e).is_some() => return Err(e),
                    Err(e) => (-1, Some(e)),
                };
                if idx < 0 || !self.in_expr {
                    let index = match resolve_from_select_fields(v, self.fields, false) {
                        Ok(i) => i,
                        Err(e) if blocked_by(&e).is_some() => return Err(e),
                        Err(_) => {
                            self.err = Some(ErrAmbiguous.gen_with_stack_by_args(go_args![
                                v.name.l(),
                                clause_msg(ClauseCode::GroupByClause)
                            ]));
                            return Ok((ResolvedExpr::Ast(in_node), false));
                        }
                    };
                    if idx >= 0 {
                        return Ok((ResolvedExpr::Ast(in_node), true));
                    }
                    if index != -1 {
                        let field_idx = index as usize;
                        let ret = self.fields[field_idx]
                            .expr()
                            .expect("resolved select field has expr");
                        accept_agg_extractor(&mut extractor, ret);
                        if !extractor.agg_funcs.is_empty() {
                            err = Some(ErrIllegalReference.gen_with_stack_by_args(go_args![
                                column_name_orig_col_name(v),
                                "reference to group function"
                            ]));
                        } else if has_window_flag(ret) {
                            err = Some(ErrIllegalReference.gen_with_stack_by_args(go_args![
                                column_name_orig_col_name(v),
                                "reference to window function"
                            ]));
                        } else {
                            if let Some(FieldExpr::Ast(Expr::ParamMarker(pm))) =
                                &self.fields[field_idx].expr
                            {
                                self.use_as_value_in_gby.insert(ByAddr::new(&**pm));
                            }
                            if !is_root {
                                self.col_replacements.insert(ByAddr::new(&**v), field_idx);
                            }
                            return Ok((ResolvedExpr::Field(field_idx), true));
                        }
                    }
                    self.err = err;
                    return Ok((ResolvedExpr::Ast(in_node), false));
                }
            }
            Expr::Position(n) => {
                let pos_expr = PositionExpr::from_num(*n);
                let (pos, is_null) = match pos_from_position_expr(self.ctx, &pos_expr) {
                    Ok(v) => v,
                    Err(e) if blocked_by(&e).is_some() => return Err(e),
                    Err(_) => {
                        self.err = Some(ErrUnknown.gen_with_stack_by_args(go_args![]));
                        return Ok((ResolvedExpr::Ast(in_node), false));
                    }
                };
                if is_null {
                    return Ok((ResolvedExpr::Ast(in_node), false));
                }
                if pos < 1 || pos as usize > self.fields.len() {
                    self.err = Some(Error::plain(format!(
                        "Unknown column '{pos}' in 'group statement'"
                    )));
                    return Ok((ResolvedExpr::Ast(in_node), false));
                }
                let field_idx = pos as usize - 1;
                let ret = self.fields[field_idx]
                    .expr()
                    .expect("resolved select field has expr");
                accept_agg_extractor(&mut extractor, ret);
                if !extractor.agg_funcs.is_empty() || has_window_flag(ret) {
                    let field_name = if !self.fields[field_idx].as_name.o.is_empty() {
                        self.fields[field_idx].as_name.o.clone()
                    } else {
                        self.fields[field_idx].text.to_string()
                    };
                    self.err =
                        Some(ErrWrongGroupField.gen_with_stack_by_args(go_args![field_name]));
                    return Ok((ResolvedExpr::Ast(in_node), false));
                }
                return Ok((ResolvedExpr::Field(field_idx), true));
            }
            Expr::Values(v)
                if v.name.o.is_empty() && v.table.o.is_empty() && v.schema.o.is_empty() =>
            {
                self.err = Some(
                    ErrUnknownColumn.gen_with_stack_by_args(go_args!["", "VALUES() function"]),
                );
            }
            _ => {}
        }
        Ok((ResolvedExpr::Ast(in_node), true))
    }
}

impl<'ast> PlanBuilder<'ast> {
    /// resolveHavingAndOrderBy will process aggregate functions and resolve the columns that don't exist in select fields.
    /// If we found some columns that are not in select fields, we will append it to select fields and update the colMapper.
    /// When we rewrite the order by / having expression, we will find column in map at first.
    ///
    /// Returns the aggregate mappers of HAVING and ORDER BY, the select fields with auxiliary
    /// fields appended, and the immutable-AST column replacement map for columns inside
    /// aggregates.
    // go: PlanBuilder.resolveHavingAndOrderBy L2 (an ORDER BY subquery is blocked-by:D09.6)
    #[allow(clippy::type_complexity)]
    pub fn resolve_having_and_order_by(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
        p: PlanId,
        fields: Vec<SelectFieldW<'ast>>,
        resolved_gby_items: &[ResolvedExpr<'ast>],
        gby_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<
        (
            AggMapper<'ast>,
            AggMapper<'ast>,
            Vec<SelectFieldW<'ast>>,
            HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
        ),
        Error,
    > {
        let mut agg_col_replacements = HashMap::new();
        let mut extractor = HavingWindowAndOrderbyExprResolver {
            in_agg_func: false,
            in_window_func: false,
            in_window_spec: false,
            in_expr: false,
            p,
            arena: &ctx.arena,
            select_fields: fields,
            agg_mapper: AggMapper::new(),
            col_mapper: &mut self.col_mapper,
            agg_col_replacements: &mut agg_col_replacements,
            gby_items: resolved_gby_items,
            gby_col_replacements,
            cur_clause: ClauseCode::UnknowClause,
            prev_clause: Vec::new(),
        };
        // Extract agg funcs from having clause.
        if let Some(h) = &sel.having {
            extractor.cur_clause = ClauseCode::HavingClause;
            extractor.accept(&h.expr)?;
        }
        let having_agg_mapper = std::mem::take(&mut extractor.agg_mapper);
        // Extract agg funcs from order by clause.
        if let Some(o) = &sel.order_by {
            extractor.cur_clause = ClauseCode::OrderByClause;
            for item in &o.items {
                extractor.in_expr = false;
                if has_window_flag(&item.expr) {
                    continue;
                }
                extractor.accept(&item.expr)?;
            }
        }
        let order_agg_mapper = std::mem::take(&mut extractor.agg_mapper);
        let fields = extractor.select_fields;
        // this part is used to fetch correlated column from sub-query item in order-by clause, and append the origin
        // auxiliary select filed in select list, otherwise, sub-query itself won't get the name resolved in outer schema.
        if let Some(o) = &sel.order_by {
            let origin_col_priv = self.check_col_priv;
            self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
            let res: Result<(), Error> = (|| {
                for by_item in &o.items {
                    if let Expr::Subquery(_) = &by_item.expr {
                        // correlated agg will be extracted completely latter.
                        self.rewrite(
                            &*ctx.expr_ctx,
                            &by_item.expr,
                            rewrite_plan(&ctx.arena, p),
                            None,
                            true,
                        )?;
                        // The rewriter stops at the subquery first (blocked-by:D09.6), so the
                        // correlated columns of its plan are never collected here.
                        // glue: blocked-by:D09.6 (the correlated columns of an ORDER BY subquery)
                        return Err(blocked(
                            "D09.6",
                            "the correlated columns of an ORDER BY subquery",
                        ));
                    }
                }
                Ok(())
            })();
            self.check_col_priv = origin_col_priv;
            res?;
        }
        Ok((
            having_agg_mapper,
            order_agg_mapper,
            fields,
            agg_col_replacements,
        ))
    }

    // go: PlanBuilder.tblInfoFromCol L2 (read-only ExtractTableList; b.isCreateView is D09.15)
    fn tbl_info_from_col(
        &self,
        from: ResultSetRef<'_>,
        name: &FieldName,
    ) -> Result<Option<Arc<TableInfo>>, Error> {
        let table_list = extract_table_list(from, self.resolve_ctx.as_ref(), true);
        for field in table_list {
            if field.name.l == name.tbl_name.l {
                if let Some(tbl_info) = field.table_info {
                    return Ok(Some(tbl_info));
                }
                // when the Select is inside a view, it's not pre-processed, tnW is nil.
                // (b.isCreateView is CREATE VIEW's, D09.15.)
                if let Some(is) = &self.is {
                    match is.0.table_by_name(&name.db_name, &name.tbl_name) {
                        Ok(tbl_info) => return Ok(Some(tbl_info)),
                        Err(e) if blocked_by(&e).is_some() => return Err(e),
                        Err(_) => return Ok(None),
                    }
                }
                return Ok(None);
            }
        }
        Ok(None)
    }

    // go: PlanBuilder.checkOnlyFullGroupBy L1
    fn check_only_full_group_by(
        &self,
        arena: &PlanArena,
        p: PlanId,
        sel: &'ast ast::SelectStmt,
        fields: &[SelectFieldW<'ast>],
        resolved_gby_items: &[ResolvedExpr<'ast>],
        gby_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<(), Error> {
        if sel.group_by.is_some() {
            self.check_only_full_group_by_with_group_clause(
                arena,
                p,
                sel,
                fields,
                resolved_gby_items,
                gby_col_replacements,
            )
        } else {
            self.check_only_full_group_by_with_out_group_clause(arena, p, sel, fields)
        }
    }

    // go: PlanBuilder.checkOnlyFullGroupByWithGroupClause L2 (deterministic iteration order, M-MAPORDER)
    fn check_only_full_group_by_with_group_clause(
        &self,
        arena: &PlanArena,
        p: PlanId,
        sel: &'ast ast::SelectStmt,
        fields: &[SelectFieldW<'ast>],
        resolved_gby_items: &[ResolvedExpr<'ast>],
        gby_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<(), Error> {
        let mut gby_or_single_value_col_names: HashSet<FieldNamePtr> =
            HashSet::with_capacity(fields.len());
        let mut gby_exprs: Vec<&Expr> = Vec::with_capacity(fields.len());
        for by_item in resolved_gby_items {
            let mut expr = get_inner_from_parentheses_and_unary_plus(by_item.get(fields));
            if let Expr::Column(c) = expr
                && let Some(&idx) = gby_col_replacements.get(&node_addr(&**c))
            {
                expr = get_inner_from_parentheses_and_unary_plus(
                    fields[idx].expr().expect("resolved field has expr"),
                );
            }
            if let Expr::Column(col_expr) = expr {
                add_gby_or_single_value_col_name(
                    arena,
                    p,
                    col_expr,
                    &mut gby_or_single_value_col_names,
                )?;
            } else {
                gby_exprs.push(expr);
            }
        }
        // MySQL permits a nonaggregate column not named in a GROUP BY clause when ONLY_FULL_GROUP_BY SQL mode is enabled,
        // provided that this column is limited to a single value.
        extract_singe_value_col_names_from_where(
            arena,
            p,
            sel.where_.as_ref(),
            &mut gby_or_single_value_col_names,
        )?;

        let mut not_in_gby_or_single_value_col_names = OrderedErrExprLocMap::default();
        for (offset, field) in fields.iter().enumerate() {
            if field.auxiliary {
                continue;
            }
            if let Some(fexpr) = field.expr() {
                check_expr_in_group_by_or_is_single_value(
                    arena,
                    p,
                    get_inner_from_parentheses_and_unary_plus(fexpr),
                    offset,
                    ErrExprInSelect,
                    &gby_or_single_value_col_names,
                    &gby_exprs,
                    fields,
                    gby_col_replacements,
                    &mut not_in_gby_or_single_value_col_names,
                )?;
            }
        }

        if let Some(o) = &sel.order_by {
            for (offset, item) in o.items.iter().enumerate() {
                if let Expr::Column(col_name) = &item.expr {
                    let index = resolve_from_select_fields(col_name, fields, false)?;
                    // If the ByItem is in fields list, it has been checked already in above.
                    if index >= 0 {
                        continue;
                    }
                }
                check_expr_in_group_by_or_is_single_value(
                    arena,
                    p,
                    get_inner_from_parentheses_and_unary_plus(&item.expr),
                    offset,
                    ErrExprInOrderBy,
                    &gby_or_single_value_col_names,
                    &gby_exprs,
                    fields,
                    gby_col_replacements,
                    &mut not_in_gby_or_single_value_col_names,
                )?;
            }
        }
        if not_in_gby_or_single_value_col_names.is_empty() {
            return Ok(());
        }

        let where_depends = build_where_func_depend(arena, p, sel.where_.as_ref())?;
        let from_ref = ResultSetRef::Join(&sel.from.as_ref().expect("sel.from").table_refs);
        let join_depends = self.build_join_func_depend(arena, p, from_ref)?;
        let rollup = sel.group_by.as_ref().is_some_and(|g| g.rollup);
        let mut tbl_map: HashSet<usize> =
            HashSet::with_capacity(not_in_gby_or_single_value_col_names.entries.len());
        for (_, name, err_expr_loc) in not_in_gby_or_single_value_col_names.entries {
            let Some(tbl_info) = self.tbl_info_from_col(from_ref, &name)? else {
                continue;
            };
            let tbl_key = Arc::as_ptr(&tbl_info) as usize;
            if tbl_map.contains(&tbl_key) {
                continue;
            }
            if check_col_func_depend(
                arena,
                p,
                &name,
                &tbl_info,
                &gby_or_single_value_col_names,
                &where_depends,
                &join_depends,
            )? {
                tbl_map.insert(tbl_key);
                continue;
            }
            match err_expr_loc.loc {
                ErrExprInSelect => {
                    if rollup {
                        return Err(
                            ErrFieldInGroupingNotGroupBy.gen_with_stack_by_args(go_args![
                                (err_expr_loc.offset + 1).to_string()
                            ]),
                        );
                    }
                    // OrigColName is empty for derived-table columns (e.g. aliased aggregates
                    // like `MAX(...) AS col2`). Fall back to ColName so the error message
                    // includes the full identifier rather than the truncated "tbl." form.
                    let col_part = if name.orig_col_name.o.is_empty() {
                        name.col_name.o.as_str()
                    } else {
                        name.orig_col_name.o.as_str()
                    };
                    let mut col_ref = format!("{}.{}", name.tbl_name.o, col_part);
                    if !name.db_name.o.is_empty() {
                        col_ref = format!("{}.{}", name.db_name.o, col_ref);
                    }
                    return Err(ErrFieldNotInGroupBy.gen_with_stack_by_args(go_args![
                        err_expr_loc.offset as i64 + 1,
                        err_expr_loc.loc,
                        col_ref
                    ]));
                }
                ErrExprInOrderBy => {
                    // Go passes `sel.OrderBy.Items[errExprLoc.Offset].Expr.Text()`, which is
                    // always empty in v8.5.8 because parser does not call SetText on ORDER BY
                    // item expressions (go-bug-kept, #429, D09.4 spec §8 item 4).
                    return Err(ErrFieldNotInGroupBy.gen_with_stack_by_args(go_args![
                        err_expr_loc.offset as i64 + 1,
                        err_expr_loc.loc,
                        ""
                    ]));
                }
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    // go: PlanBuilder.checkOnlyFullGroupByWithOutGroupClause L2 (a blocked error of FindFieldName is returned, not turned into ErrMixOfGroupFuncAndFields)
    fn check_only_full_group_by_with_out_group_clause(
        &self,
        arena: &PlanArena,
        p: PlanId,
        sel: &ast::SelectStmt,
        fields: &[SelectFieldW<'_>],
    ) -> Result<(), Error> {
        let mut resolver = ColResolverForOnlyFullGroupBy {
            non_agg_cols: Vec::new(),
            expr_idx: 0,
            non_agg_col_idxs: Vec::new(),
            has_agg_func_or_any_value: false,
            first_order_by_agg_col_idx: -1,
            cur_clause: ClauseCode::FieldList,
        };
        for (idx, field) in fields.iter().enumerate() {
            resolver.expr_idx = idx;
            if let Some(e) = field.expr() {
                resolver.accept(e);
            }
        }
        if !resolver.non_agg_cols.is_empty() {
            if let Some(h) = &sel.having {
                // Go leaves curClause=FieldList and exprIdx at the last SELECT field index
                // when walking HAVING (go-bug-kept, #428, D09.4 spec §8 item 3).
                resolver.accept(&h.expr);
            }
            if let Some(o) = &sel.order_by {
                resolver.cur_clause = ClauseCode::OrderByClause;
                for (idx, by_item) in o.items.iter().enumerate() {
                    resolver.expr_idx = idx;
                    resolver.accept(&by_item.expr);
                }
            }
        }
        if resolver.first_order_by_agg_col_idx != -1 && !resolver.non_agg_cols.is_empty() {
            // SQL like `select a from t where a = 1 order by count(b)` is illegal.
            return Err(ErrAggregateOrderNonAggQuery
                .gen_with_stack_by_args(go_args![resolver.first_order_by_agg_col_idx + 1]));
        }
        if !resolver.has_agg_func_or_any_value || resolver.non_agg_cols.is_empty() {
            return Ok(());
        }
        let mut single_value_col_names: HashSet<FieldNamePtr> =
            HashSet::with_capacity(fields.len());
        extract_singe_value_col_names_from_where(
            arena,
            p,
            sel.where_.as_ref(),
            &mut single_value_col_names,
        )?;
        let where_depends = build_where_func_depend(arena, p, sel.where_.as_ref())?;
        let from_ref = ResultSetRef::Join(&sel.from.as_ref().expect("sel.from").table_refs);
        let join_depends = self.build_join_func_depend(arena, p, from_ref)?;
        let output_names = names_of(arena, p);
        let mut tbl_map: HashSet<usize> = HashSet::with_capacity(resolver.non_agg_cols.len());
        for (i, col_name) in resolver.non_agg_cols.iter().enumerate() {
            let idx = match find_field_name(output_names, col_name) {
                Ok(i) => i,
                Err(e) if blocked_by(&e).is_some() => return Err(e),
                Err(_) => -1,
            };
            if idx < 0 {
                return Err(ErrMixOfGroupFuncAndFields.gen_with_stack_by_args(go_args![
                    resolver.non_agg_col_idxs[i] as i64 + 1,
                    col_name.name.o.clone()
                ]));
            }
            let field_name = &output_names[idx as usize];
            if single_value_col_names.contains(&field_name_ptr(field_name)) {
                continue;
            }
            let Some(tbl_info) = self.tbl_info_from_col(from_ref, field_name)? else {
                continue;
            };
            let tbl_key = Arc::as_ptr(&tbl_info) as usize;
            if tbl_map.contains(&tbl_key) {
                continue;
            }
            if check_col_func_depend(
                arena,
                p,
                field_name,
                &tbl_info,
                &single_value_col_names,
                &where_depends,
                &join_depends,
            )? {
                tbl_map.insert(tbl_key);
                continue;
            }
            return Err(ErrMixOfGroupFuncAndFields.gen_with_stack_by_args(go_args![
                resolver.non_agg_col_idxs[i] as i64 + 1,
                col_name.name.o.clone()
            ]));
        }
        Ok(())
    }

    /// resolveGbyExprs resolves group by expressions from the group by clause of a select statement.
    /// Returns `(gby_exprs, resolved_gby_items, gby_col_replacements)`:
    /// - `gby_exprs` is passed to `rewrite_gby_exprs`.
    /// - `resolved_gby_items` mirrors Go's in-place update of `gby.Items[i].Expr` when `!isParam`.
    /// - `gby_col_replacements` stores inner column-alias replacements for an immutable AST.
    // go: PlanBuilder.resolveGbyExprs L2 (immutable AST returns gby_exprs, resolved_gby_items and gby_col_replacements)
    #[allow(clippy::type_complexity)]
    pub fn resolve_gby_exprs(
        &mut self,
        ctx: &PlanCtx<'_>,
        p: PlanId,
        gby: &'ast ast::GroupByClause,
        fields: &[SelectFieldW<'ast>],
    ) -> Result<
        (
            Vec<ResolvedExpr<'ast>>,
            Vec<ResolvedExpr<'ast>>,
            HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
        ),
        Error,
    > {
        self.cur_clause = ClauseCode::GroupByClause;
        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let res = self.resolve_gby_exprs_inner(ctx, p, gby, fields);
        self.check_col_priv = origin_col_priv;
        res
    }

    // go-part: PlanBuilder.resolveGbyExprs
    #[allow(clippy::type_complexity)]
    fn resolve_gby_exprs_inner(
        &mut self,
        ctx: &PlanCtx<'_>,
        p: PlanId,
        gby: &'ast ast::GroupByClause,
        fields: &[SelectFieldW<'ast>],
    ) -> Result<
        (
            Vec<ResolvedExpr<'ast>>,
            Vec<ResolvedExpr<'ast>>,
            HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
        ),
        Error,
    > {
        let mut exprs = Vec::with_capacity(gby.items.len());
        let mut resolved_items = Vec::with_capacity(gby.items.len());
        let mut col_replacements = HashMap::new();
        let mut names = names_of(&ctx.arena, p);
        // findJoinFullSchema walks through transparent wrappers (LogicalSelection)
        // and handles both LogicalJoin and LogicalApply, so GROUP BY resolution sees
        // redundant USING/NATURAL JOIN columns even when a Selection sits above the join.
        // (The resolver reads no schema, only the names.)
        if let Some((_, full_names)) = find_join_full_schema(&ctx.arena, p) {
            names = full_names;
        }
        let in_execute = ctx.vars.stmt_ctx().in_prepared_plan_building;
        let mut resolver = GbyResolver {
            ctx: &*ctx.expr_ctx,
            in_execute,
            fields,
            names,
            err: None,
            in_expr: false,
            is_param: false,
            expr_depth: 0,
            use_as_value_in_gby: HashSet::new(),
            col_replacements: &mut col_replacements,
        };
        for item in &gby.items {
            let ret_expr = resolver.resolve_item(&item.expr)?;
            if !resolver.is_param {
                resolved_items.push(ret_expr);
            } else {
                resolved_items.push(ResolvedExpr::Ast(&item.expr));
            }
            exprs.push(ret_expr);
        }
        Ok((exprs, resolved_items, col_replacements))
    }

    // go: PlanBuilder.rewriteGbyExprs L2 (R29: sets check_col_priv = ReportColumnErrOption during GROUP BY rewrites; Go v8.5.8 guarded resolveGbyExprs instead, where no expression rewriting runs)
    pub fn rewrite_gby_exprs(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        gby: &ast::GroupByClause,
        items: &[ResolvedExpr<'ast>],
        fields: &[SelectFieldW<'ast>],
        gby_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<(PlanId, Vec<ExprRef>, bool), Error> {
        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let res = self.rewrite_gby_exprs_inner(ctx, p, gby, items, fields, gby_col_replacements);
        self.check_col_priv = origin_col_priv;
        res
    }

    // go-part: PlanBuilder.rewriteGbyExprs
    fn rewrite_gby_exprs_inner(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        gby: &ast::GroupByClause,
        items: &[ResolvedExpr<'ast>],
        fields: &[SelectFieldW<'ast>],
        gby_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<(PlanId, Vec<ExprRef>, bool), Error> {
        let mut exprs = Vec::with_capacity(gby.items.len());
        let col_repl_map: ColReplacements<'_> = gby_col_replacements
            .iter()
            .filter_map(|(k, &idx)| fields.get(idx)?.expr().map(|e| (*k, e)))
            .collect();
        let col_repl = (!col_repl_map.is_empty()).then_some(&col_repl_map);

        for item in items {
            let expr_node = item.get(fields);
            let expr = self
                .rewrite_with_col_replacements(
                    &*ctx.expr_ctx,
                    expr_node,
                    rewrite_plan(&ctx.arena, p),
                    None,
                    col_repl,
                    true,
                )?
                .expect("asScalar rewrite returns an expression");
            exprs.push(expr);
        }
        Ok((p, exprs, gby.rollup))
    }

    /// resolveCorrelatedAggregates extracts the aggregates of the outer query from the
    /// subqueries of the select fields, HAVING and ORDER BY. Without a subquery there is
    /// none; with one it is `blocked-by:D09.6`.
    // glue: stub of PlanBuilder.resolveCorrelatedAggregates, blocked-by:D09.6 (correlated aggregates inside subqueries)
    fn resolve_correlated_aggregates(
        &self,
        sel: &ast::SelectStmt,
        fields: &[SelectFieldW<'_>],
    ) -> Result<(), Error> {
        let any = fields
            .iter()
            .filter_map(SelectFieldW::expr)
            .any(has_subquery)
            || sel.having.as_ref().is_some_and(|h| has_subquery(&h.expr))
            || sel
                .order_by
                .as_ref()
                .is_some_and(|o| o.items.iter().any(|i| has_subquery(&i.expr)));
        if any {
            return Err(blocked(
                "D09.6",
                "resolveCorrelatedAggregates (aggregates of the outer query inside subqueries)",
            ));
        }
        Ok(())
    }

    // go: PlanBuilder.unfoldWildStar L2 (no LogicalApply arm yet: the apply part)
    fn unfold_wild_star(
        &self,
        arena: &PlanArena,
        p: PlanId,
        select_fields: Vec<SelectFieldW<'ast>>,
    ) -> Result<Vec<SelectFieldW<'ast>>, Error> {
        // Extract FullSchema/FullNames from LogicalJoin or LogicalApply (which embeds LogicalJoin).
        // (There is no LogicalApply yet: the apply part of the join block.)
        let (full_schema, full_names) = match arena.downcast_ref::<LogicalJoin>(p) {
            Some(x) => (x.full_schema.as_ref(), Some(&x.full_names)),
            None => (None, None),
        };
        let mut result_list = Vec::with_capacity(select_fields.len().max(2));
        for (i, field) in select_fields.into_iter().enumerate() {
            let Some(wild_card) = field.wild_card else {
                result_list.push(field);
                continue;
            };
            if wild_card.table.l().is_empty() && i > 0 {
                return Err(ErrInvalidWildCard.clone());
            }
            let mut list =
                unfold_wild_star(wild_card, names_of(arena, p), &schema_of(arena, p).columns);
            // For sql like `select t1.*, t2.* from t1 join t2 using(a)` or `select t1.*, t2.* from t1 natual join t2`,
            // the schema of the Join doesn't contain enough columns because the join keys are coalesced in this schema.
            // We should collect the columns from the FullSchema.
            if let (Some(full_schema), Some(full_names)) = (full_schema, full_names)
                && !wild_card.table.l().is_empty()
            {
                list = unfold_wild_star(wild_card, full_names, &full_schema.columns);
            }
            if list.is_empty() {
                return Err(ErrBadTable.gen_with_stack_by_args(go_args![wild_card.table.o.clone()]));
            }
            result_list.extend(list);
        }
        Ok(result_list)
    }

    // go: PlanBuilder.popVisitInfo L1
    pub fn pop_visit_info(&mut self) {
        if self.visit_info.is_empty() {
            return;
        }
        self.visit_info.pop();
    }
}

// go: unfoldWildStar L1
fn unfold_wild_star<'a>(
    wild_card: &ast::WildCardField,
    output_name: &[Arc<FieldName>],
    column: &[Arc<Column>],
) -> Vec<SelectFieldW<'a>> {
    let mut result_list = Vec::new();
    let (db_name, tbl_name) = (wild_card.schema.l(), wild_card.table.l());
    for (i, name) in output_name.iter().enumerate() {
        let col = &column[i];
        if col.is_hidden {
            continue;
        }
        if (db_name.is_empty() || db_name == name.db_name.l)
            && (tbl_name.is_empty() || tbl_name == name.tbl_name.l)
            && col.id != ExtraHandleID as i64
            && col.id != ExtraPhysTblID as i64
        {
            let col_name = column_name_of(name);
            // colName.SetType(col.GetStaticType()): the AST type of a column name is not read.
            let mut field = SelectFieldW::auxiliary(FieldExpr::Built(Box::new(Expr::Column(
                Box::new(col_name),
            ))));
            field.auxiliary = false;
            field.is_unfold_from_wild_card = true;
            field.text = Cow::Owned(name.col_name.o.clone());
            result_list.push(field);
        }
    }
    result_list
}

/// addAliasName adds an alias to each select field when a view is created (`capFlag &
/// canExpandAST`, set only by CREATE VIEW).
// glue: stub of PlanBuilder.addAliasName, blocked-by:D09.15 (CREATE VIEW)
pub fn add_alias_name() -> Result<(), Error> {
    Err(blocked("D09.15", "addAliasName (CREATE VIEW)"))
}

/// findColumnNameByUniqueID finds the name of an outer column referred by a subquery.
// glue: stub of findColumnNameByUniqueID, blocked-by:D09.6 (the correlated columns of subqueries)
pub fn find_column_name_by_unique_id() -> Result<(), Error> {
    Err(blocked("D09.6", "findColumnNameByUniqueID (subqueries)"))
}

// glue: stub of allColFromAggExprNode, blocked-by:D09.4 (correlated aggregates, D09.4 part D)
pub fn all_col_from_agg_expr_node() -> Result<(), Error> {
    Err(blocked(
        "D09.4",
        "allColFromAggExprNode (correlated aggregates)",
    ))
}

// ---------------------------------------------------------------- window plan building (D09.4 part B)

/// Frame clause held by [`WindowSpecW`]: either borrowed from the statement AST or owned
/// (synthesized by `handle_default_frame` without mutating the AST, D09.4 spec §4.3).
#[derive(Debug)]
pub enum CowFrame<'ast> {
    /// Borrowed from the parsed statement AST.
    Borrowed(&'ast ast::FrameClause),
    /// Synthesized default frame clause (`RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`
    /// or `ROWS BETWEEN CURRENT ROW AND CURRENT ROW`).
    Owned(ast::FrameClause),
}

impl CowFrame<'_> {
    /// Borrow the underlying [`ast::FrameClause`].
    // glue: accessor for CowFrame
    pub fn as_frame(&self) -> &ast::FrameClause {
        match self {
            CowFrame::Borrowed(f) => f,
            CowFrame::Owned(f) => f,
        }
    }
}

/// Builder-local view of `*ast.WindowSpec` so `merge_window_spec` and `handle_default_frame`
/// never mutate the statement AST (D09.4 spec §4.3 item 4).
#[derive(Debug, Default)]
pub struct WindowSpecW<'ast> {
    /// Window name (`spec.Name`).
    pub name: ast::CIStr,
    /// Referenced window name (`spec.Ref`); cleared after merging.
    pub ref_: ast::CIStr,
    /// Partition-by clause borrowed from the AST.
    pub partition_by: Option<&'ast ast::PartitionByClause>,
    /// Order-by clause borrowed from the AST.
    pub order_by: Option<&'ast ast::OrderByClause>,
    /// Frame clause (borrowed from AST or owned default frame).
    pub frame: Option<CowFrame<'ast>>,
    /// Whether `OVER w` (alias only) vs `OVER (...)`.
    pub only_alias: bool,
}

impl<'ast> WindowSpecW<'ast> {
    /// Construct a builder-local view of an AST `WindowSpec`.
    // glue: constructor for WindowSpecW
    pub fn of_ast(spec: &'ast ast::WindowSpec) -> Self {
        WindowSpecW {
            name: spec.name.clone(),
            ref_: spec.ref_.clone(),
            partition_by: spec.partition_by.as_deref(),
            order_by: spec.order_by.as_deref(),
            frame: spec.frame.as_deref().map(CowFrame::Borrowed),
            only_alias: spec.only_alias,
        }
    }

    /// Shallow-copy metadata + clause pointers with a replacement frame (never clones AST exprs).
    // glue: copy helper for handleDefaultFrame (immutable AST)
    fn with_frame(&self, frame: Option<CowFrame<'ast>>) -> Self {
        WindowSpecW {
            name: self.name.clone(),
            ref_: self.ref_.clone(),
            partition_by: self.partition_by,
            order_by: self.order_by,
            frame,
            only_alias: self.only_alias,
        }
    }

    /// Restore using Go's `format.NewRestoreCtx(0, ...)` (flags = 0).
    // glue: flags=0 restore of WindowSpecW for specEqual
    fn restore_raw(&self, out: &mut String) -> Result<(), String> {
        restore_window_spec_raw(
            &self.name.o,
            self.only_alias,
            &self.ref_.o,
            self.partition_by,
            self.order_by,
            self.frame.as_ref().map(CowFrame::as_frame),
            out,
        )
    }
}

/// Identity handle for a [`WindowSpecW`] in `group_window_funcs` / `build_window_functions`
/// (replaces Go's `*ast.WindowSpec` pointer keys under an immutable AST).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WindowSpecId {
    /// Index into `PlanBuilder.window_specs` (named window specs of the SELECT).
    Named(usize),
    /// Index into the local `local_specs` pool created by `group_window_funcs`.
    Local(usize),
}

// glue: resolve WindowSpecId against PlanBuilder.window_specs and local_specs
fn resolve_spec_id<'a, 'ast>(
    id: WindowSpecId,
    named_specs: &'a [WindowSpecW<'ast>],
    local_specs: &'a [WindowSpecW<'ast>],
) -> &'a WindowSpecW<'ast> {
    match id {
        WindowSpecId::Named(i) => &named_specs[i],
        WindowSpecId::Local(i) => &local_specs[i],
    }
}

/// Walk `expr` with a [`WindowFuncExtractor`].
// glue: ast.Node.Accept for WindowFuncExtractor (the Rust AST has no visitor, D02.2)
fn accept_window_extractor<'a>(extractor: &mut WindowFuncExtractor<'a>, expr: &'a Expr) {
    if !extractor.enter(expr) {
        for c in expr_children(expr) {
            accept_window_extractor(extractor, c);
        }
        match expr {
            Expr::Aggregate(a) => {
                if let Some(o) = &a.order {
                    for item in &o.items {
                        accept_window_extractor(extractor, &item.expr);
                    }
                }
            }
            Expr::Window(w) => {
                if let Some(pb) = &w.spec.partition_by {
                    for item in &pb.items {
                        accept_window_extractor(extractor, &item.expr);
                    }
                }
                if let Some(ob) = &w.spec.order_by {
                    for item in &ob.items {
                        accept_window_extractor(extractor, &item.expr);
                    }
                }
                if let Some(fr) = &w.spec.frame {
                    if let Some(e) = &fr.extent.start.expr {
                        accept_window_extractor(extractor, e);
                    }
                    if let Some(e) = &fr.extent.end.expr {
                        accept_window_extractor(extractor, e);
                    }
                }
            }
            _ => {}
        }
    }
    let _ = extractor.leave(expr);
}

// go: getWindowName L1
fn get_window_name(name: &str) -> &str {
    if name.is_empty() {
        return "<unnamed window>";
    }
    name
}

// go: getAllByItems L1
pub fn get_all_by_items<'a, 'ast>(
    items_buf: &'a mut Vec<&'ast ast::ByItem>,
    spec: &WindowSpecW<'ast>,
) -> &'a [&'ast ast::ByItem] {
    items_buf.clear();
    if let Some(pb) = spec.partition_by {
        items_buf.extend(pb.items.iter());
    }
    if let Some(ob) = spec.order_by {
        items_buf.extend(ob.items.iter());
    }
    items_buf.as_slice()
}

// go: restoreByItemText L1
pub fn restore_by_item_text(item: &ast::ByItem) -> String {
    let mut sb = String::new();
    if item.expr.restore_raw(&mut sb).is_err() {
        return String::new();
    }
    sb
}

// go: compareItems L1
pub fn compare_items(l_items: &[&ast::ByItem], r_items: &[&ast::ByItem]) -> bool {
    let min_len = l_items.len().min(r_items.len());
    for i in 0..min_len {
        let cmp = restore_by_item_text(l_items[i]).cmp(&restore_by_item_text(r_items[i]));
        if cmp != std::cmp::Ordering::Equal {
            return cmp == std::cmp::Ordering::Less;
        }
        let res = compare_bool(l_items[i].desc, r_items[i].desc);
        if res != 0 {
            return res < 0;
        }
    }
    l_items.len() < r_items.len()
}

/// Grouped window specification and its associated window functions.
struct WindowFuncsGroup<'ast> {
    spec: WindowSpecId,
    funcs: Vec<&'ast ast::WindowFuncExpr>,
}

/// sortWindowSpecs sorts the window specifications by reversed alphabetical order, then we could add less `Sort` operator
/// in physical plan because the window functions with the same partition by and order by clause will be at near places.
// go: sortWindowSpecs L1 [M-PLAN]
fn sort_window_specs<'ast>(
    grouped_funcs: &HashMap<WindowSpecId, Vec<&'ast ast::WindowFuncExpr>>,
    ordered_spec: &[WindowSpecId],
    named_specs: &[WindowSpecW<'ast>],
    local_specs: &[WindowSpecW<'ast>],
) -> Vec<WindowFuncsGroup<'ast>> {
    let mut windows = Vec::with_capacity(grouped_funcs.len());
    for &spec in ordered_spec {
        windows.push(WindowFuncsGroup {
            spec,
            funcs: grouped_funcs.get(&spec).cloned().unwrap_or_default(),
        });
    }
    let mut l_items_buf = Vec::with_capacity(4);
    let mut r_items_buf = Vec::with_capacity(4);
    // Note: Go uses sort.SliceStable with non-strict `!compareItems`; up to 20 window groups both
    // Go's sort.SliceStable and Rust's slice::sort_by use insertion sort and match, whereas >20
    // groups can differ in order because Go uses symMerge while Rust uses a different merge sort.
    windows.sort_by(|a, b| {
        let sa = resolve_spec_id(a.spec, named_specs, local_specs);
        let sb = resolve_spec_id(b.spec, named_specs, local_specs);
        let l_less = {
            let l_items = get_all_by_items(&mut l_items_buf, sa);
            let r_items = get_all_by_items(&mut r_items_buf, sb);
            !compare_items(l_items, r_items)
        };
        if l_less {
            return std::cmp::Ordering::Less;
        }
        let r_less = {
            let l_items = get_all_by_items(&mut l_items_buf, sb);
            let r_items = get_all_by_items(&mut r_items_buf, sa);
            !compare_items(l_items, r_items)
        };
        if r_less {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    windows
}

// go: extractWindowFuncs L1
fn extract_window_funcs<'ast>(fields: &[SelectFieldW<'ast>]) -> Vec<&'ast ast::WindowFuncExpr> {
    let mut extractor = WindowFuncExtractor::default();
    for f in fields {
        // Window functions only come from parsed AST expressions (built fields are bare ColumnName).
        if let Some(FieldExpr::Ast(e)) = &f.expr {
            accept_window_extractor(&mut extractor, e);
        }
    }
    extractor.window_funcs
}

/// append WindowSpecId to Vec<WindowSpecId> if absent.
// go: appendIfAbsentWindowSpec L1
fn append_if_absent_window_spec(specs: &mut Vec<WindowSpecId>, ns: WindowSpecId) {
    if !specs.contains(&ns) {
        specs.push(ns);
    }
}

// go: specEqual L1
fn spec_equal(s1: Option<&WindowSpecW<'_>>, s2: Option<&WindowSpecW<'_>>) -> Result<bool, Error> {
    let (Some(s1), Some(s2)) = (s1, s2) else {
        return Ok(s1.is_none() && s2.is_none());
    };
    let mut sb1 = String::new();
    let mut sb2 = String::new();
    s1.restore_raw(&mut sb1).map_err(Error::plain)?;
    s2.restore_raw(&mut sb2).map_err(Error::plain)?;
    Ok(sb1 == sb2)
}

/// resolveWindowSpec resolve window specifications for sql like `select ... from t window w1 as (w2), w2 as (partition by a)`.
/// We need to resolve the referenced window to get the definition of current window spec.
// go: resolveWindowSpec L1
fn resolve_window_spec<'ast>(
    spec_idx: usize,
    specs: &mut [WindowSpecW<'ast>],
    index: &BTreeMap<String, usize>,
    in_stack: &mut HashMap<String, bool>,
) -> Result<(), Error> {
    let spec_name_l = specs[spec_idx].name.l();
    if in_stack.get(&spec_name_l).copied().unwrap_or(false) {
        return Err(ErrWindowCircularityInWindowGraph.clone());
    }
    let ref_l = specs[spec_idx].ref_.l();
    if ref_l.is_empty() {
        return Ok(());
    }
    let Some(&ref_idx) = index.get(&ref_l) else {
        let ref_o = specs[spec_idx].ref_.o.clone();
        return Err(ErrWindowNoSuchWindow.gen_with_stack_by_args(go_args![ref_o]));
    };
    in_stack.insert(spec_name_l.clone(), true);
    resolve_window_spec(ref_idx, specs, index, in_stack)?;
    in_stack.insert(spec_name_l, false);
    // Copy the read-only fields from `specs[ref_idx]` before mutating `specs[spec_idx]`.
    let ref_snapshot = WindowSpecW {
        name: specs[ref_idx].name.clone(),
        ref_: specs[ref_idx].ref_.clone(),
        partition_by: specs[ref_idx].partition_by,
        order_by: specs[ref_idx].order_by,
        frame: specs[ref_idx].frame.as_ref().map(|f| match f {
            CowFrame::Borrowed(b) => CowFrame::Borrowed(b),
            CowFrame::Owned(_) => unreachable!("buildWindowSpecs only holds borrowed frames"),
        }),
        only_alias: specs[ref_idx].only_alias,
    };
    merge_window_spec(&mut specs[spec_idx], &ref_snapshot)
}

// go: mergeWindowSpec L1
fn merge_window_spec<'ast>(
    spec: &mut WindowSpecW<'ast>,
    ref_spec: &WindowSpecW<'ast>,
) -> Result<(), Error> {
    if ref_spec.frame.is_some() {
        return Err(
            ErrWindowNoInherentFrame.gen_with_stack_by_args(go_args![ref_spec.name.o.clone()])
        );
    }
    if spec.partition_by.is_some() {
        return Err(ErrWindowNoChildPartitioning.clone());
    }
    if ref_spec.order_by.is_some() {
        if spec.order_by.is_some() {
            return Err(ErrWindowNoRedefineOrderBy.gen_with_stack_by_args(go_args![
                get_window_name(&spec.name.o).to_owned(),
                ref_spec.name.o.clone()
            ]));
        }
        spec.order_by = ref_spec.order_by;
    }
    spec.partition_by = ref_spec.partition_by;
    spec.ref_ = ast::CIStr::default();
    Ok(())
}

// go: buildWindowSpecs L2 (deterministic declaration-order iteration, M-MAPORDER; immutable AST via WindowSpecW)
fn build_window_specs<'ast>(
    specs: &'ast [ast::WindowSpec],
) -> Result<(Vec<WindowSpecW<'ast>>, BTreeMap<String, usize>), Error> {
    let mut specs_vec = Vec::with_capacity(specs.len());
    let mut index = BTreeMap::new();
    for spec in specs {
        let key = spec.name.l();
        if index.contains_key(&key) {
            return Err(
                ErrWindowDuplicateName.gen_with_stack_by_args(go_args![spec.name.o.clone()])
            );
        }
        let idx = specs_vec.len();
        specs_vec.push(WindowSpecW::of_ast(spec));
        index.insert(key, idx);
    }
    let mut in_stack = HashMap::with_capacity(specs.len());
    for i in 0..specs_vec.len() {
        resolve_window_spec(i, &mut specs_vec, &index, &mut in_stack)?;
    }
    Ok((specs_vec, index))
}

/// Evaluate a RANGE window frame bound expression into a [`Constant`].
///
/// Strictly for compound decimal `INTERVAL` expressions (e.g. `INTERVAL 1.0+0.5 DAY`),
/// derives `RetType` from the evaluated `Datum` (`KindMysqlDecimal -> TypeNewDecimal`)
/// instead of Go's zero-value `FieldType` (`TypeUnspecified -> ETString`, which selects
/// `SubDateDatetimeString`/`AddDateDatetimeString` and truncates `"1.5"` to 1 day;
/// Divergence ruling R28 / #413, owner decision 2026-11-26 option A). All other non-literal
/// bounds (numeric RANGE bounds, non-decimal INTERVALs, parenthesized literals such as
/// `INTERVAL (1.5) DAY`, and parameter markers such as `INTERVAL ? DAY`) keep Go's
/// zero-value `FieldType`.
// go-part: PlanBuilder.buildWindowFunctionFrameBound
pub(crate) fn eval_window_range_bound_constant(
    ctx: &dyn BuildContext,
    bound_expr: &Expr,
    spec_name: &str,
    in_prepare_stmt: bool,
    is_interval: bool,
) -> Result<Constant, Error> {
    if let Expr::Value(v) = bound_expr {
        let go_val = value_expr_go_value(v);
        let mut ret_type = FieldType::default();
        default_type_for_value(&go_val, &mut ret_type, v.charset, v.collate);
        let mut val = value_expr_datum(v);
        let gv = val.get_value();
        val.set_value(gv, &ret_type);
        return Ok(Constant::new(val, ret_type));
    }
    let val = match eval_ast_expr_with_plan_ctx(ctx, bound_expr) {
        Ok(v) => v,
        Err(e) if blocked_by(&e).is_some() => return Err(e),
        Err(_) => {
            return Err(ErrWindowRangeBoundNotConstant
                .gen_with_stack_by_args(go_args![get_window_name(spec_name).to_owned()]));
        }
    };
    let mut ret_type = FieldType::default();
    // Divergence ruling R28 (#413, owner decision 2026-11-26 option A): strictly for a
    // compound decimal INTERVAL bound (not numeric RANGE, not non-decimal INTERVAL, not a
    // parenthesized literal like `INTERVAL (1.5) DAY`, and not a ParamMarker like
    // `INTERVAL ? DAY`), infer Constant.RetType from the evaluated Decimal Datum so
    // DateSub/DateAdd selects SubDateDatetimeDecimal/AddDateDatetimeDecimal (2 days)
    // instead of Go's SubDateDatetimeString/AddDateDatetimeString (1 day). Neither String
    // nor Decimal signatures are pushed down to TiFlash.
    if is_interval
        && !in_prepare_stmt
        && val.kind() == tidb_types::datum::KindMysqlDecimal
        && !matches!(bound_expr, Expr::ParamMarker(_) | Expr::Paren(_))
    {
        default_type_for_value(
            &val.get_value(),
            &mut ret_type,
            DefaultCharset,
            DefaultCollationName,
        );
    }
    Ok(Constant::new(val, ret_type))
}

/// Clear `NotNullFlag` on any column matching `target_uid` inside an expression tree.
// glue: Go aliasing (M-FT) helper for window_func.go:76 & logical_plan_builder.go:7251 (#402)
fn clear_not_null_in_expr(expr: &mut ExprRef, target_uid: i64) {
    match Arc::make_mut(expr) {
        PExpr::Column(c) if c.unique_id == target_uid => {
            Arc::make_mut(&mut c.ret_type).del_flag(NotNullFlag);
        }
        PExpr::ScalarFunction(sf) => {
            for arg in &mut sf.function.args {
                clear_not_null_in_expr(arg, target_uid);
            }
        }
        _ => {}
    }
}

/// Clear `NotNullFlag` on any column matching `target_uid` inside a `WindowFrame`.
// glue: Go aliasing (M-FT) helper for window_func.go:76 & logical_plan_builder.go:7251 (#402)
fn clear_not_null_in_frame(frame: &mut Option<WindowFrame>, target_uid: i64) {
    if let Some(fr) = frame {
        for bound in [&mut fr.start, &mut fr.end].into_iter().flatten() {
            for e in bound
                .calc_funcs
                .iter_mut()
                .chain(bound.compare_cols.iter_mut())
            {
                clear_not_null_in_expr(e, target_uid);
            }
        }
    }
}

/// Propagate `NotNullFlag` removal on `target_uid` through the subtree below a `LogicalWindow`
/// (`np` down to `DataSource`), emulating Go's shared `*FieldType` pointer mutation in
/// `NewWindowFuncDesc` for `first_value` / `last_value` / `nth_value` (`M-FT`, confirmed V1 #402).
/// Does not modify `TableInfo` or `ColumnInfo`.
// glue: Go aliasing (M-FT): window_func.go:76 & logical_plan_builder.go:7251 (#402)
fn clear_not_null_in_subtree(arena: &mut PlanArena, root: PlanId, target_uid: i64) {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let children = arena.get(id).children().to_vec();
        stack.extend(children);

        if arena.get(id).schema_producer().is_some() {
            let schema = arena.schema_mut(id);
            for col in &mut schema.columns {
                if col.unique_id == target_uid && has_not_null_flag(col.ret_type.get_flag()) {
                    Arc::make_mut(&mut Arc::make_mut(col).ret_type).del_flag(NotNullFlag);
                }
            }
        }

        let node = arena.get_mut(id);
        match node.kind() {
            LogicalKind::DataSource => {
                let ds = node.as_any_mut().downcast_mut::<DataSource>().unwrap();
                for col in &mut ds.tbl_cols {
                    if col.unique_id == target_uid && has_not_null_flag(col.ret_type.get_flag()) {
                        Arc::make_mut(&mut Arc::make_mut(col).ret_type).del_flag(NotNullFlag);
                    }
                }
                for col in &mut ds.common_handle_cols {
                    if col.unique_id == target_uid && has_not_null_flag(col.ret_type.get_flag()) {
                        Arc::make_mut(&mut Arc::make_mut(col).ret_type).del_flag(NotNullFlag);
                    }
                }
                for path in ds
                    .all_possible_access_paths
                    .iter_mut()
                    .chain(ds.possible_access_paths.iter_mut())
                {
                    for col_opt in &mut path.full_idx_cols {
                        if let Some(col) = col_opt
                            && col.unique_id == target_uid
                            && has_not_null_flag(col.ret_type.get_flag())
                        {
                            Arc::make_mut(&mut Arc::make_mut(col).ret_type).del_flag(NotNullFlag);
                        }
                    }
                }
                for hc in [&mut ds.handle_cols, &mut ds.un_mutable_handle_cols]
                    .into_iter()
                    .flatten()
                {
                    hc.clear_col_not_null_flag(target_uid);
                }
                for cond in ds
                    .pushed_down_conds
                    .iter_mut()
                    .chain(ds.all_conds.iter_mut())
                {
                    clear_not_null_in_expr(cond, target_uid);
                }
            }
            LogicalKind::Projection => {
                let proj = node
                    .as_any_mut()
                    .downcast_mut::<LogicalProjection>()
                    .unwrap();
                let schema_uids: Vec<i64> = proj
                    .sp
                    .schema_opt()
                    .map(|s| s.columns.iter().map(|c| c.unique_id).collect())
                    .unwrap_or_default();
                for (idx, expr) in proj.exprs.iter_mut().enumerate() {
                    clear_not_null_in_expr(expr, target_uid);
                    if schema_uids.get(idx).copied() == Some(target_uid)
                        && let PExpr::ScalarFunction(sf) = Arc::make_mut(expr)
                        && has_not_null_flag(sf.function.tp.get_flag())
                    {
                        Arc::make_mut(&mut sf.function.tp).del_flag(NotNullFlag);
                    }
                }
            }
            LogicalKind::Selection => {
                let sel = node
                    .as_any_mut()
                    .downcast_mut::<LogicalSelection>()
                    .unwrap();
                for cond in &mut sel.conditions {
                    clear_not_null_in_expr(cond, target_uid);
                }
            }
            LogicalKind::Aggregation => {
                let agg = node
                    .as_any_mut()
                    .downcast_mut::<LogicalAggregation>()
                    .unwrap();
                for item in &mut agg.group_by_items {
                    clear_not_null_in_expr(item, target_uid);
                }
                for desc in &mut agg.agg_funcs {
                    for arg in &mut desc.base.args {
                        clear_not_null_in_expr(arg, target_uid);
                    }
                    for item in &mut desc.order_by_items {
                        clear_not_null_in_expr(&mut item.expr, target_uid);
                    }
                }
            }
            LogicalKind::Window => {
                let win = node.as_any_mut().downcast_mut::<LogicalWindow>().unwrap();
                for item in win.partition_by.iter_mut().chain(win.order_by.iter_mut()) {
                    if item.col.unique_id == target_uid
                        && has_not_null_flag(item.col.ret_type.get_flag())
                    {
                        Arc::make_mut(&mut Arc::make_mut(&mut item.col).ret_type)
                            .del_flag(NotNullFlag);
                    }
                }
                for desc in &mut win.window_func_descs {
                    for arg in &mut desc.base.args {
                        clear_not_null_in_expr(arg, target_uid);
                    }
                }
                clear_not_null_in_frame(&mut win.frame, target_uid);
            }
            LogicalKind::Join => {
                // The join's conditions hold the children's columns, and `FullSchema` is a
                // `MergeSchema` of the children's (full) schemas: Go's column copies share the
                // `*FieldType` (`Column.Clone` is shallow), except the null-supplying side of an
                // outer join, which `ResetNotNullFlag` copied and which has no NOT NULL flag.
                let join = node.as_any_mut().downcast_mut::<LogicalJoin>().unwrap();
                for cond in join
                    .equal_conditions
                    .iter_mut()
                    .chain(join.na_eq_conditions.iter_mut())
                    .chain(join.left_conditions.iter_mut())
                    .chain(join.right_conditions.iter_mut())
                    .chain(join.other_conditions.iter_mut())
                {
                    clear_not_null_in_expr(cond, target_uid);
                }
                if let Some(full_schema) = &mut join.full_schema {
                    for col in &mut full_schema.columns {
                        if col.unique_id == target_uid && has_not_null_flag(col.ret_type.get_flag())
                        {
                            Arc::make_mut(&mut Arc::make_mut(col).ret_type).del_flag(NotNullFlag);
                        }
                    }
                }
            }
            LogicalKind::Sort => {
                let sort = node.as_any_mut().downcast_mut::<LogicalSort>().unwrap();
                for item in &mut sort.by_items {
                    clear_not_null_in_expr(&mut item.expr, target_uid);
                }
            }
            LogicalKind::TopN => {
                let topn = node.as_any_mut().downcast_mut::<LogicalTopN>().unwrap();
                for item in &mut topn.by_items {
                    clear_not_null_in_expr(&mut item.expr, target_uid);
                }
            }
            LogicalKind::Limit | LogicalKind::TableDual => {}
        }
    }
}

impl<'ast> PlanBuilder<'ast> {
    /// resolveWindowFunction will process window functions and resolve the columns that don't exist in select fields.
    // go: PlanBuilder.resolveWindowFunction L2 (immutable AST returns updated fields and agg_col_replacements)
    #[allow(clippy::type_complexity)]
    pub fn resolve_window_function(
        &mut self,
        ctx: &PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
        p: PlanId,
        fields: Vec<SelectFieldW<'ast>>,
    ) -> Result<
        (
            AggMapper<'ast>,
            Vec<SelectFieldW<'ast>>,
            HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
        ),
        Error,
    > {
        let orig_len = fields.len();
        let mut agg_col_replacements = HashMap::new();
        let empty_gby_repl = HashMap::new();
        let mut extractor = HavingWindowAndOrderbyExprResolver {
            in_agg_func: false,
            in_window_func: false,
            in_window_spec: false,
            in_expr: false,
            p,
            arena: &ctx.arena,
            select_fields: fields,
            agg_mapper: AggMapper::new(),
            col_mapper: &mut self.col_mapper,
            agg_col_replacements: &mut agg_col_replacements,
            gby_items: &[],
            gby_col_replacements: &empty_gby_repl,
            cur_clause: ClauseCode::FieldList,
            prev_clause: Vec::new(),
        };
        for i in 0..orig_len {
            let Some(FieldExpr::Ast(e)) = extractor.select_fields[i].expr else {
                continue;
            };
            if !has_window_flag(e) {
                continue;
            }
            extractor.accept(e)?;
        }
        for spec in &sel.window_specs {
            extractor.accept_window_spec(spec)?;
        }
        if let Some(o) = &sel.order_by {
            extractor.cur_clause = ClauseCode::OrderByClause;
            for item in &o.items {
                if !has_window_flag(&item.expr) {
                    continue;
                }
                extractor.accept(&item.expr)?;
            }
        }
        Ok((
            extractor.agg_mapper,
            extractor.select_fields,
            agg_col_replacements,
        ))
    }

    /// buildProjectionForWindow builds the projection for expressions in the window specification that is not an column,
    /// so after the projection, window functions only needs to deal with columns.
    // go: PlanBuilder.buildProjectionForWindow L1 [M-PLAN, M-CTX]
    #[allow(clippy::type_complexity)]
    fn build_projection_for_window(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        mut p: PlanId,
        spec: &WindowSpecW<'ast>,
        args: &[&'ast Expr],
        agg_map: Option<&AggMapper<'_>>,
    ) -> Result<(PlanId, Vec<SortItem>, Vec<SortItem>, Vec<ExprRef>), Error> {
        self.opt_flag |= FlagEliminateProjection;

        let partition_items: &[ast::ByItem] = spec.partition_by.map_or(&[], |pb| &pb.items);
        let order_items: &[ast::ByItem] = spec.order_by.map_or(&[], |ob| &ob.items);

        let p_schema_cols = schema_of(&ctx.arena, p).columns.clone();
        let p_names = names_of(&ctx.arena, p).clone();
        let proj_len = p_schema_cols.len() + partition_items.len() + order_items.len() + args.len();
        let mut proj =
            LogicalProjection::init(Vec::with_capacity(proj_len), ctx, self.get_select_offset());
        let mut proj_schema = new_schema(Vec::with_capacity(proj_len));
        let mut proj_names = Vec::with_capacity(proj_len);
        for col in &p_schema_cols {
            proj.exprs
                .push(PExpr::Column(Column::clone(col)).into_ref());
            proj_schema.append([Arc::clone(col)]);
        }
        proj_names.extend(p_names);

        let mut property_items = Vec::with_capacity(partition_items.len() + order_items.len());
        p = self.build_by_items_for_window(
            ctx,
            p,
            &mut proj,
            &mut proj_schema,
            &mut proj_names,
            partition_items,
            &mut property_items,
            agg_map,
        )?;
        let len_partition = property_items.len();
        p = self.build_by_items_for_window(
            ctx,
            p,
            &mut proj,
            &mut proj_schema,
            &mut proj_names,
            order_items,
            &mut property_items,
            agg_map,
        )?;

        let mut new_arg_list = Vec::with_capacity(args.len());
        for &arg in args {
            let new_arg = self
                .rewrite(
                    &*ctx.expr_ctx,
                    arg,
                    rewrite_plan(&ctx.arena, p),
                    agg_map,
                    true,
                )?
                .expect("asScalar rewrite returns an expression");
            if matches!(&*new_arg, PExpr::Column(_) | PExpr::Constant(_)) {
                new_arg_list.push(Arc::new((*new_arg).clone()));
                continue;
            }
            let ret_type = Arc::new(new_arg.get_type(&*ctx.eval_ctx()).into_owned());
            proj.exprs.push(new_arg);
            proj_names.push(EmptyName.clone());
            let col = Arc::new(Column {
                unique_id: ctx.vars.alloc_plan_column_id(),
                ret_type,
                ..Column::default()
            });
            proj_schema.append([Arc::clone(&col)]);
            new_arg_list.push(PExpr::Column(Column::clone(&col)).into_ref());
        }

        proj.sp.set_schema(Some(proj_schema));
        proj.sp.set_output_names(proj_names);
        proj.set_children(vec![p]);
        let order_by = property_items.split_off(len_partition);
        let partition_by = property_items;
        Ok((
            ctx.arena.alloc(Box::new(proj)),
            partition_by,
            order_by,
            new_arg_list,
        ))
    }

    // go: PlanBuilder.buildArgs4WindowFunc L1 [M-CTX]
    fn build_args4_window_func(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        args: &[Expr],
        agg_map: Option<&AggMapper<'_>>,
    ) -> Result<Vec<ExprRef>, Error> {
        self.opt_flag |= FlagEliminateProjection;

        let mut new_arg_list = Vec::with_capacity(args.len());
        for arg in args {
            let new_arg = self
                .rewrite(
                    &*ctx.expr_ctx,
                    arg,
                    rewrite_plan(&ctx.arena, p),
                    agg_map,
                    true,
                )?
                .expect("asScalar rewrite returns an expression");
            if matches!(&*new_arg, PExpr::Column(_) | PExpr::Constant(_)) {
                new_arg_list.push(Arc::new((*new_arg).clone()));
                continue;
            }
            let ret_type = Arc::new(new_arg.get_type(&*ctx.eval_ctx()).into_owned());
            let col = Column {
                unique_id: ctx.vars.alloc_plan_column_id(),
                ret_type,
                ..Column::default()
            };
            new_arg_list.push(PExpr::Column(col).into_ref());
        }
        Ok(new_arg_list)
    }

    // go: PlanBuilder.buildByItemsForWindow L1 [M-CTX]
    #[allow(clippy::too_many_arguments)]
    fn build_by_items_for_window(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        proj: &mut LogicalProjection,
        proj_schema: &mut Schema,
        proj_names: &mut NameSlice,
        items: &[ast::ByItem],
        ret_items: &mut Vec<SortItem>,
        agg_map: Option<&AggMapper<'_>>,
    ) -> Result<PlanId, Error> {
        for item in items {
            ItemTransformer.accept(&item.expr)?;
            let it = self
                .rewrite(
                    &*ctx.expr_ctx,
                    &item.expr,
                    rewrite_plan(&ctx.arena, p),
                    agg_map,
                    true,
                )?
                .expect("asScalar rewrite returns an expression");
            if it.get_type(&*ctx.eval_ctx()).get_type() == TypeNull {
                continue;
            }
            if let PExpr::Column(col) = &*it {
                let col_arc = Arc::new(col.clone());
                ret_items.push(SortItem {
                    col: Arc::clone(&col_arc),
                    desc: item.desc,
                });
                // We need to attempt to add this column because a subquery may be created during the expression rewrite process.
                // Therefore, we need to ensure that the column from the newly created query plan is added.
                // If the column is already in the schema, we don't need to add it again.
                if !proj_schema.contains(&col_arc) {
                    proj.exprs.push(it);
                    proj_names.push(EmptyName.clone());
                    proj_schema.append([col_arc]);
                }
                continue;
            }
            let ret_type = Arc::new(it.get_type(&*ctx.eval_ctx()).into_owned());
            proj.exprs.push(it);
            proj_names.push(EmptyName.clone());
            let col = Arc::new(Column {
                unique_id: ctx.vars.alloc_plan_column_id(),
                ret_type,
                ..Column::default()
            });
            proj_schema.append([Arc::clone(&col)]);
            ret_items.push(SortItem {
                col,
                desc: item.desc,
            });
        }
        Ok(p)
    }

    /// buildWindowFunctionFrameBound builds the bounds of window function frames.
    /// For type `Rows`, the bound expr must be an unsigned integer.
    /// For type `Range`, the bound expr must be temporal or numeric types.
    // go: PlanBuilder.buildWindowFunctionFrameBound L1 [M-CTX]
    fn build_window_function_frame_bound(
        &self,
        ctx: &PlanCtx<'_>,
        spec: &WindowSpecW<'ast>,
        order_by_items: &[SortItem],
        bound_clause: &ast::FrameBound,
    ) -> Result<FrameBound, Error> {
        let frame_type = spec.frame.as_ref().expect("frame").as_frame().tp;
        let mut bound = FrameBound {
            tp: bound_clause.tp,
            unbounded: bound_clause.unbounded,
            is_explicit_range: false,
            ..FrameBound::default()
        };
        if bound.unbounded {
            return Ok(bound);
        }

        let in_execute = ctx.vars.stmt_ctx().in_prepared_plan_building;
        if frame_type == ast::FrameType::Rows {
            if bound.tp == ast::BoundType::CurrentRow {
                return Ok(bound);
            }
            let (num_rows, _, _) = get_uint_from_node(
                &*ctx.expr_ctx,
                bound_clause.expr.as_ref().expect("rows bound expr"),
                false,
                in_execute,
            );
            bound.num = num_rows;
            return Ok(bound);
        }

        bound.calc_funcs = Vec::with_capacity(order_by_items.len());
        bound.cmp_funcs = vec![None; order_by_items.len()];
        if bound.tp == ast::BoundType::CurrentRow {
            for (i, item) in order_by_items.iter().enumerate() {
                let col_expr = PExpr::Column(Column::clone(&item.col)).into_ref();
                bound.cmp_funcs[i] = Some(get_cmp_function(&*ctx.expr_ctx, &col_expr, &col_expr));
                bound.calc_funcs.push(col_expr);
            }
            return Ok(bound);
        }

        let col = &order_by_items[0].col;
        let bound_expr = bound_clause.expr.as_ref().expect("range bound expr");

        let mut checker = ParamMarkerInPrepareChecker::new(in_execute);
        checker.accept(bound_expr);

        let expr = eval_window_range_bound_constant(
            &*ctx.expr_ctx,
            bound_expr,
            &spec.name.o,
            checker.in_prepare_stmt,
            bound_clause.unit != ast::TimeUnitType::Invalid,
        )?;

        // If it has paramMarker and is in prepare stmt. We don't need to eval it since its value is not decided yet.
        if !checker.in_prepare_stmt {
            // Do not raise warnings for truncate.
            let expr_ctx = ctx_with_handle_truncate_err_level(
                Arc::clone(&ctx.expr_ctx) as tidb_sessionctx::exprctx::BuildContextRef,
                errctx::LevelIgnore,
            );
            match expr.eval_int(&*expr_ctx.get_eval_ctx(), &EvalState::new(), Row::default()) {
                Ok(Some(u_val)) if u_val >= 0 => {}
                Err(e) if blocked_by(&e).is_some() => return Err(e),
                _ => {
                    return Err(ErrWindowFrameIllegal.gen_with_stack_by_args(go_args![
                        get_window_name(&spec.name.o).to_owned()
                    ]));
                }
            }
        }

        bound.is_explicit_range = true;
        let desc = order_by_items[0].desc;
        let col_expr = PExpr::Column(Column::clone(col)).into_ref();
        let const_expr = PExpr::Constant(expr).into_ref();
        let calc_func = if bound_clause.unit != ast::TimeUnitType::Invalid {
            let unit_val = bound_clause.unit.as_str();
            let unit = PExpr::Constant(Constant::new(
                new_string_datum(unit_val),
                new_field_type(TypeVarchar),
            ))
            .into_ref();

            // When the order is asc:
            //   `+` for following, and `-` for the preceding
            // When the order is desc, `+` becomes `-` and vice-versa.
            let func_name = if (!desc && bound.tp == ast::BoundType::Preceding)
                || (desc && bound.tp == ast::BoundType::Following)
            {
                ast::funcs::DateSub
            } else {
                ast::funcs::DateAdd
            };

            new_function_base(
                &*ctx.expr_ctx,
                func_name,
                (*col.ret_type).clone(),
                vec![Arc::clone(&col_expr), const_expr, unit],
            )?
        } else {
            // When the order is asc:
            //   `+` for following, and `-` for the preceding
            // When the order is desc, `+` becomes `-` and vice-versa.
            let func_name = if (!desc && bound.tp == ast::BoundType::Preceding)
                || (desc && bound.tp == ast::BoundType::Following)
            {
                ast::funcs::Minus
            } else {
                ast::funcs::Plus
            };

            new_function_base(
                &*ctx.expr_ctx,
                func_name,
                (*col.ret_type).clone(),
                vec![Arc::clone(&col_expr), const_expr],
            )?
        };
        bound.calc_funcs.push(calc_func);

        let cmp_data_type =
            get_accurate_cmp_type(&*ctx.eval_ctx(), &col_expr, &bound.calc_funcs[0]);
        bound.update_cmp_funcs_and_cmp_data_type(cmp_data_type);
        Ok(bound)
    }

    /// buildWindowFunctionFrame builds the window function frames.
    // go: PlanBuilder.buildWindowFunctionFrame L1 [M-CTX]
    fn build_window_function_frame(
        &self,
        ctx: &PlanCtx<'_>,
        spec: &WindowSpecW<'ast>,
        order_by_items: &[SortItem],
    ) -> Result<Option<WindowFrame>, Error> {
        let Some(frame_clause) = spec.frame.as_ref().map(CowFrame::as_frame) else {
            return Ok(None);
        };
        let start = self.build_window_function_frame_bound(
            ctx,
            spec,
            order_by_items,
            &frame_clause.extent.start,
        )?;
        let end = self.build_window_function_frame_bound(
            ctx,
            spec,
            order_by_items,
            &frame_clause.extent.end,
        )?;
        Ok(Some(WindowFrame {
            tp: frame_clause.tp,
            start: Some(start),
            end: Some(end),
        }))
    }

    // go: PlanBuilder.checkWindowFuncArgs L1 [M-DEFER, M-CTX]
    fn check_window_func_args(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        window_func_exprs: &[&'ast ast::WindowFuncExpr],
        window_agg_map: Option<&AggMapper<'_>>,
    ) -> Result<(), Error> {
        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let res = self.check_window_func_args_inner(ctx, p, window_func_exprs, window_agg_map);
        self.check_col_priv = origin_col_priv;
        res
    }

    // go-part: PlanBuilder.checkWindowFuncArgs
    fn check_window_func_args_inner(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        window_func_exprs: &[&'ast ast::WindowFuncExpr],
        window_agg_map: Option<&AggMapper<'_>>,
    ) -> Result<(), Error> {
        let in_execute = ctx.vars.stmt_ctx().in_prepared_plan_building;
        let mut checker = ParamMarkerInPrepareChecker::new(in_execute);
        for window_func_expr in window_func_exprs {
            if window_func_expr.name.to_lowercase() == ast::funcs::AggFuncGroupConcat {
                return Err(ErrNotSupportedYet
                    .gen_with_stack_by_args(go_args!["group_concat as window function"]));
            }
            let args =
                self.build_args4_window_func(ctx, p, &window_func_expr.args, window_agg_map)?;
            let arg0_not_null_uid = args.first().and_then(|e| match &**e {
                PExpr::Column(c) if has_not_null_flag(c.ret_type.get_flag()) => Some(c.unique_id),
                _ => None,
            });
            checker.in_prepare_stmt = false;
            for expr in &window_func_expr.args {
                checker.accept(expr);
            }
            let Some(desc) = new_window_func_desc(
                &*ctx.expr_ctx,
                &window_func_expr.name,
                args,
                checker.in_prepare_stmt,
            )?
            else {
                return Err(ErrWrongArguments
                    .gen_with_stack_by_args(go_args![window_func_expr.name.to_lowercase()]));
            };
            // Go aliasing (M-FT): window_func.go:76 & logical_plan_builder.go:7136 (#402).
            // Go strips NotNullFlag via NewWindowFuncDesc inside checkWindowFuncArgs — before ANY
            // window group is built — mutating the shared *FieldType pointer of args[0]. Clearing
            // NotNullFlag on `p` right here ensures every window group (even one built before the
            // first_value/last_value/nth_value group, e.g.
            // `SELECT LEAD(k,1,0) OVER (ORDER BY k), FIRST_VALUE(k) OVER (ORDER BY id) FROM sbtest1`)
            // sees the nullable column type when building its own projection and descriptors.
            // Note: in Go, shared *FieldType aliasing can also reach a DataSource column or an
            // aggregation's `firstrow` column with a different unique_id (e.g.
            // `SELECT FIRST_VALUE(a) OVER () FROM t GROUP BY a`, currently blocked at D09.13);
            // verify that path when D09.13 opens.
            if let Some(uid) = arg0_not_null_uid
                && desc
                    .base
                    .args
                    .first()
                    .is_some_and(|e| !has_not_null_flag(e.get_type(&*ctx.eval_ctx()).get_flag()))
            {
                clear_not_null_in_subtree(&mut ctx.arena, p, uid);
            }
        }
        Ok(())
    }

    // go: PlanBuilder.buildWindowFunctions L1 [M-DEFER, M-FT, M-PLAN, M-CTX]
    #[allow(clippy::too_many_arguments)]
    fn build_window_functions(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        grouped_funcs: &HashMap<WindowSpecId, Vec<&'ast ast::WindowFuncExpr>>,
        ordered_spec: &[WindowSpecId],
        local_specs: &[WindowSpecW<'ast>],
        agg_map: Option<&AggMapper<'_>>,
    ) -> Result<(PlanId, WindowMapper<'ast>), Error> {
        // b.buildingCTE: no CTE is built yet (D09.5).
        let origin_col_priv = self.check_col_priv;
        self.check_col_priv = CheckSelectColumnPrivOption::ReportColumnErrOption;
        let res = self.build_window_functions_inner(
            ctx,
            p,
            grouped_funcs,
            ordered_spec,
            local_specs,
            agg_map,
        );
        self.check_col_priv = origin_col_priv;
        res
    }

    // go-part: PlanBuilder.buildWindowFunctions
    #[allow(clippy::too_many_arguments)]
    fn build_window_functions_inner(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        mut p: PlanId,
        grouped_funcs: &HashMap<WindowSpecId, Vec<&'ast ast::WindowFuncExpr>>,
        ordered_spec: &[WindowSpecId],
        local_specs: &[WindowSpecW<'ast>],
        agg_map: Option<&AggMapper<'_>>,
    ) -> Result<(PlanId, WindowMapper<'ast>), Error> {
        let mut args: Vec<&'ast Expr> = Vec::with_capacity(4);
        let mut window_map: WindowMapper<'ast> = WindowMapper::new();
        let in_execute = ctx.vars.stmt_ctx().in_prepared_plan_building;
        let sorted =
            sort_window_specs(grouped_funcs, ordered_spec, &self.window_specs, local_specs);
        for window_group in sorted {
            args.clear();
            let funcs = window_group.funcs;
            for window_func in &funcs {
                args.extend(window_func.args.iter());
            }
            // Borrow spec by copying its lightweight handles before calling `&mut self` methods.
            let spec_view = {
                let s = resolve_spec_id(window_group.spec, &self.window_specs, local_specs);
                WindowSpecW {
                    name: s.name.clone(),
                    ref_: s.ref_.clone(),
                    partition_by: s.partition_by,
                    order_by: s.order_by,
                    frame: None,
                    only_alias: s.only_alias,
                }
            };
            let (np, partition_by, order_by, mut built_args) =
                self.build_projection_for_window(ctx, p, &spec_view, &args, agg_map)?;
            if funcs.is_empty() {
                // len(funcs) == 0 indicates this an unused named window spec,
                // so we just check for its validity and don't have to build plan for it.
                let spec = resolve_spec_id(window_group.spec, &self.window_specs, local_specs);
                self.check_origin_window_spec(ctx, spec, &order_by)?;
                continue;
            }
            self.check_origin_window_funcs(ctx, &funcs, &order_by)?;
            let frame = {
                let spec = resolve_spec_id(window_group.spec, &self.window_specs, local_specs);
                self.build_window_function_frame(ctx, spec, &order_by)?
            };

            let mut window = LogicalWindow::init(
                Vec::new(),
                partition_by,
                order_by,
                frame,
                ctx,
                self.get_select_offset(),
            );
            let mut window_names = names_of(&ctx.arena, np).clone();
            let mut schema = schema_of(&ctx.arena, np).clone_schema();
            let mut descs: Vec<tidb_expression::aggregation::WindowFuncDesc> =
                Vec::with_capacity(funcs.len());
            let mut pre_args = 0usize;
            let mut checker = ParamMarkerInPrepareChecker::new(in_execute);
            for window_func in funcs {
                checker.in_prepare_stmt = false;
                for expr in &window_func.args {
                    checker.accept(expr);
                }
                let next_pre = pre_args + window_func.args.len();
                let wf_args: Vec<ExprRef> = built_args[pre_args..next_pre].to_vec();
                let arg0_not_null_uid = wf_args.first().and_then(|e| match &**e {
                    PExpr::Column(c) if has_not_null_flag(c.ret_type.get_flag()) => {
                        Some(c.unique_id)
                    }
                    _ => None,
                });
                let Some(mut desc) = new_window_func_desc(
                    &*ctx.expr_ctx,
                    &window_func.name,
                    wf_args,
                    checker.in_prepare_stmt,
                )?
                else {
                    return Err(ErrWrongArguments
                        .gen_with_stack_by_args(go_args![window_func.name.to_lowercase()]));
                };
                // Go aliasing (M-FT): window_func.go:76 & logical_plan_builder.go:7251 (#402).
                // For first_value/last_value/nth_value (non-enum/set), Go's DelFlag(NotNullFlag)
                // mutates the shared *FieldType pointer of args[0], clearing NotNullFlag on the
                // source column across the LogicalWindow schema and all descendants down to
                // DataSource (without touching TableInfo). Propagate it so ColumnPruner and
                // ProjectionEliminator cannot restore NotNullFlag from child schemas.
                if let Some(uid) = arg0_not_null_uid
                    && desc.base.args.first().is_some_and(|e| {
                        !has_not_null_flag(e.get_type(&*ctx.eval_ctx()).get_flag())
                    })
                {
                    for col in &mut schema.columns {
                        if col.unique_id == uid && has_not_null_flag(col.ret_type.get_flag()) {
                            Arc::make_mut(&mut Arc::make_mut(col).ret_type).del_flag(NotNullFlag);
                        }
                    }
                    for item in window
                        .partition_by
                        .iter_mut()
                        .chain(window.order_by.iter_mut())
                    {
                        if item.col.unique_id == uid
                            && has_not_null_flag(item.col.ret_type.get_flag())
                        {
                            Arc::make_mut(&mut Arc::make_mut(&mut item.col).ret_type)
                                .del_flag(NotNullFlag);
                        }
                    }
                    for prev_desc in &mut descs {
                        for arg in &mut prev_desc.base.args {
                            clear_not_null_in_expr(arg, uid);
                        }
                    }
                    for rem_arg in &mut built_args[next_pre..] {
                        clear_not_null_in_expr(rem_arg, uid);
                    }
                    clear_not_null_in_frame(&mut window.frame, uid);
                    clear_not_null_in_subtree(&mut ctx.arena, np, uid);
                }
                pre_args = next_pre;
                desc.wrap_cast_for_agg_args(&*ctx.expr_ctx)?;
                let ret_tp = Arc::clone(&desc.base.ret_tp);
                descs.push(desc);
                window_map.insert(ByAddr::new(window_func), schema.len() as isize);
                schema.append([Arc::new(Column {
                    unique_id: ctx.vars.alloc_plan_column_id(),
                    ret_type: ret_tp,
                    ..Column::default()
                })]);
                window_names.push(EmptyName.clone());
            }
            window.window_func_descs = descs;
            window.set_children(vec![np]);
            window.sp.set_schema(Some(schema));
            window.sp.set_output_names(window_names);
            p = ctx.arena.alloc(Box::new(window));
        }
        Ok((p, window_map))
    }

    /// checkOriginWindowFuncs checks the validity for original window specifications for a group of functions.
    /// Because the grouped specification is different from them, we should especially check them before build window frame.
    // go: PlanBuilder.checkOriginWindowFuncs L1
    fn check_origin_window_funcs(
        &self,
        ctx: &PlanCtx<'_>,
        funcs: &[&'ast ast::WindowFuncExpr],
        order_by_items: &[SortItem],
    ) -> Result<(), Error> {
        for f in funcs {
            if f.ignore_null {
                return Err(ErrNotSupportedYet.gen_with_stack_by_args(go_args!["IGNORE NULLS"]));
            }
            if f.distinct {
                return Err(ErrNotSupportedYet
                    .gen_with_stack_by_args(go_args!["<window function>(DISTINCT ..)"]));
            }
            if f.from_last {
                return Err(ErrNotSupportedYet.gen_with_stack_by_args(go_args!["FROM LAST"]));
            }
            let name_l = f.spec.name.l();
            if !name_l.is_empty() {
                let spec = &self.window_specs[self.window_spec_index[&name_l]];
                self.check_origin_window_spec(ctx, spec, order_by_items)?;
            } else {
                let spec = WindowSpecW::of_ast(&f.spec);
                self.check_origin_window_spec(ctx, &spec, order_by_items)?;
            }
        }
        Ok(())
    }

    /// checkOriginWindowSpec checks the validity for given window specification.
    // go: PlanBuilder.checkOriginWindowSpec L1
    fn check_origin_window_spec(
        &self,
        ctx: &PlanCtx<'_>,
        spec: &WindowSpecW<'ast>,
        order_by_items: &[SortItem],
    ) -> Result<(), Error> {
        let Some(frame) = spec.frame.as_ref().map(CowFrame::as_frame) else {
            return Ok(());
        };
        if frame.tp == ast::FrameType::Groups {
            return Err(ErrNotSupportedYet.gen_with_stack_by_args(go_args!["GROUPS"]));
        }
        let (start, end) = (&frame.extent.start, &frame.extent.end);
        if start.tp == ast::BoundType::Following && start.unbounded {
            return Err(ErrWindowFrameStartIllegal
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }
        if end.tp == ast::BoundType::Preceding && end.unbounded {
            return Err(ErrWindowFrameEndIllegal
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }
        if start.tp == ast::BoundType::Following
            && (end.tp == ast::BoundType::Preceding || end.tp == ast::BoundType::CurrentRow)
        {
            return Err(ErrWindowFrameIllegal
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }
        if (start.tp == ast::BoundType::Following || start.tp == ast::BoundType::CurrentRow)
            && end.tp == ast::BoundType::Preceding
        {
            return Err(ErrWindowFrameIllegal
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }

        self.check_origin_window_frame_bound(ctx, start, spec, order_by_items)?;
        self.check_origin_window_frame_bound(ctx, end, spec, order_by_items)?;
        Ok(())
    }

    // go: PlanBuilder.checkOriginWindowFrameBound L1 [M-CTX]
    fn check_origin_window_frame_bound(
        &self,
        ctx: &PlanCtx<'_>,
        bound: &ast::FrameBound,
        spec: &WindowSpecW<'ast>,
        order_by_items: &[SortItem],
    ) -> Result<(), Error> {
        if bound.tp == ast::BoundType::CurrentRow || bound.unbounded {
            return Ok(());
        }

        let frame_type = spec.frame.as_ref().expect("frame").as_frame().tp;
        if frame_type == ast::FrameType::Rows {
            if bound.unit != ast::TimeUnitType::Invalid {
                return Err(ErrWindowRowsIntervalUse
                    .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
            }
            let in_execute = ctx.vars.stmt_ctx().in_prepared_plan_building;
            let (_, is_null, is_expected_type) = get_uint_from_node(
                &*ctx.expr_ctx,
                bound.expr.as_ref().expect("rows bound expr"),
                false,
                in_execute,
            );
            if is_null || !is_expected_type {
                return Err(ErrWindowFrameIllegal
                    .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
            }
            return Ok(());
        }

        if order_by_items.len() != 1 {
            return Err(ErrWindowRangeFrameOrderType
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }
        let order_item_type = order_by_items[0].col.ret_type.get_type();
        let (is_numeric, is_temporal) = (
            is_type_numeric(order_item_type),
            is_type_temporal(order_item_type),
        );
        if !is_numeric && !is_temporal {
            return Err(ErrWindowRangeFrameOrderType
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }
        if bound.unit != ast::TimeUnitType::Invalid && !is_temporal {
            return Err(ErrWindowRangeFrameNumericType
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }
        if bound.unit == ast::TimeUnitType::Invalid && !is_numeric {
            return Err(ErrWindowRangeFrameTemporalType
                .gen_with_stack_by_args(go_args![get_window_name(&spec.name.o).to_owned()]));
        }
        Ok(())
    }

    // go: PlanBuilder.handleDefaultFrame L1 [M-CTX]
    fn handle_default_frame(
        &self,
        ctx: &PlanCtx<'_>,
        spec: &WindowSpecW<'ast>,
        window_func_name: &str,
    ) -> Option<WindowSpecW<'ast>> {
        let need = need_frame(window_func_name);
        // According to MySQL, In the absence of a frame clause, the default frame depends on whether an ORDER BY clause is present:
        //   (1) With order by, the default frame is equivalent to "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW";
        //   (2) Without order by, the default frame includes all partition rows, equivalent to "RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING",
        //       or "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING", which is the same as an empty frame.
        if need && spec.frame.is_none() && spec.order_by.is_some() {
            return Some(spec.with_frame(Some(CowFrame::Owned(ast::FrameClause {
                tp: ast::FrameType::Ranges,
                extent: ast::FrameExtent {
                    start: ast::FrameBound {
                        tp: ast::BoundType::Preceding,
                        unbounded: true,
                        ..Default::default()
                    },
                    end: ast::FrameBound {
                        tp: ast::BoundType::CurrentRow,
                        ..Default::default()
                    },
                },
            }))));
        }
        // "RANGE/ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING" is equivalent to empty frame.
        if need
            && let Some(f) = spec.frame.as_ref().map(CowFrame::as_frame)
            && f.extent.start.unbounded
            && f.extent.end.unbounded
        {
            return Some(spec.with_frame(None));
        }
        if !need {
            let mut updated = false;
            let mut new_frame: Option<CowFrame<'ast>> = None;

            // For functions that operate on the entire partition, the frame clause will be ignored.
            if spec.frame.is_some() {
                let spec_name = &spec.name.o;
                ctx.vars
                    .stmt_ctx()
                    .append_note(ErrWindowFunctionIgnoresFrame.fast_gen_by_args(go_args![
                        window_func_name.to_owned(),
                        get_window_name(spec_name).to_owned()
                    ]));
                updated = true;
            }
            if ctx.vars.enable_pipelined_window_exec {
                let (use_def, default_frame) = use_default_frame(window_func_name);
                if use_def {
                    new_frame = Some(CowFrame::Owned(default_frame));
                    updated = true;
                }
            }
            if updated {
                return Some(spec.with_frame(new_frame));
            }
        }
        None
    }

    /// groupWindowFuncs groups the window functions according to the window specification name.
    // go: PlanBuilder.groupWindowFuncs L2 (immutable AST uses WindowSpecId and local_specs; deterministic unused-window order, M-MAPORDER)
    #[allow(clippy::type_complexity)]
    fn group_window_funcs(
        &self,
        ctx: &PlanCtx<'_>,
        window_funcs: &[&'ast ast::WindowFuncExpr],
    ) -> Result<
        (
            HashMap<WindowSpecId, Vec<&'ast ast::WindowFuncExpr>>,
            Vec<WindowSpecId>,
            Vec<WindowSpecW<'ast>>,
        ),
        Error,
    > {
        // updatedSpecMap is used to handle the specifications that have frame clause changed.
        let mut updated_spec_map: HashMap<String, Vec<WindowSpecId>> = HashMap::new();
        let mut grouped_window: HashMap<WindowSpecId, Vec<&'ast ast::WindowFuncExpr>> =
            HashMap::new();
        let mut ordered_spec: Vec<WindowSpecId> = Vec::with_capacity(window_funcs.len());
        let mut local_specs: Vec<WindowSpecW<'ast>> = Vec::new();

        for &window_func in window_funcs {
            if window_func.spec.name.l().is_empty() {
                let mut spec = WindowSpecW::of_ast(&window_func.spec);
                let ref_l = spec.ref_.l();
                if !ref_l.is_empty() {
                    let Some(&ref_idx) = self.window_spec_index.get(&ref_l) else {
                        return Err(ErrWindowNoSuchWindow.gen_with_stack_by_args(go_args![
                            get_window_name(&spec.ref_.o).to_owned()
                        ]));
                    };
                    merge_window_spec(&mut spec, &self.window_specs[ref_idx])?;
                }
                if let Some(new_spec) = self.handle_default_frame(ctx, &spec, &window_func.name) {
                    spec = new_spec;
                }
                let id = WindowSpecId::Local(local_specs.len());
                local_specs.push(spec);
                grouped_window.entry(id).or_default().push(window_func);
                append_if_absent_window_spec(&mut ordered_spec, id);
                continue;
            }

            let name = window_func.spec.name.l();
            let Some(&named_idx) = self.window_spec_index.get(&name) else {
                return Err(ErrWindowNoSuchWindow
                    .gen_with_stack_by_args(go_args![window_func.spec.name.o.clone()]));
            };
            let named_id = WindowSpecId::Named(named_idx);
            let spec = &self.window_specs[named_idx];
            match self.handle_default_frame(ctx, spec, &window_func.name) {
                None => {
                    grouped_window
                        .entry(named_id)
                        .or_default()
                        .push(window_func);
                    append_if_absent_window_spec(&mut ordered_spec, named_id);
                }
                Some(new_spec) => {
                    let updated_id = if let Some(existing_ids) = updated_spec_map.get_mut(&name) {
                        let mut found = None;
                        for &cand_id in existing_ids.iter() {
                            let cand = resolve_spec_id(cand_id, &self.window_specs, &local_specs);
                            if spec_equal(Some(cand), Some(&new_spec))? {
                                found = Some(cand_id);
                                break;
                            }
                        }
                        if let Some(id) = found {
                            id
                        } else {
                            let id = WindowSpecId::Local(local_specs.len());
                            local_specs.push(new_spec);
                            existing_ids.push(id);
                            id
                        }
                    } else {
                        let id = WindowSpecId::Local(local_specs.len());
                        local_specs.push(new_spec);
                        updated_spec_map.insert(name, vec![id]);
                        id
                    };
                    grouped_window
                        .entry(updated_id)
                        .or_default()
                        .push(window_func);
                    append_if_absent_window_spec(&mut ordered_spec, updated_id);
                }
            }
        }
        // Unused window specs should also be checked in b.buildWindowFunctions,
        // so we add them to `groupedWindow` with empty window functions (declaration order, M-MAPORDER).
        for (i, spec) in self.window_specs.iter().enumerate() {
            let id = WindowSpecId::Named(i);
            if !grouped_window.contains_key(&id) && !updated_spec_map.contains_key(&spec.name.l()) {
                grouped_window.insert(id, Vec::new());
                append_if_absent_window_spec(&mut ordered_spec, id);
            }
        }
        Ok((grouped_window, ordered_spec, local_specs))
    }
}

/// addExtraPhysTblIDColumn4DS for partition table (SELECT FOR UPDATE and union scans of
/// partitioned tables).
// glue: stub of addExtraPhysTblIDColumn4DS, blocked-by:D09.12 (partitioned tables)
pub fn add_extra_phys_tbl_id_column4_ds() -> Result<(), Error> {
    Err(blocked(
        "D09.12",
        "addExtraPhysTblIDColumn4DS (partitioned tables)",
    ))
}

/// UPDATE / DELETE helpers.
// glue: stub of getTableOffset, blocked-by:D09.15 (UPDATE / DELETE)
// glue: stub of collectTableName, blocked-by:D09.15 (UPDATE / DELETE)
pub fn get_table_offset() -> Result<(), Error> {
    Err(blocked("D09.15", "getTableOffset (UPDATE / DELETE)"))
}

// go: appendDynamicVisitInfo L1
pub fn append_dynamic_visit_info(
    vi: &mut Vec<VisitInfo>,
    privs: Vec<String>,
    with_grant: bool,
    err: Option<Error>,
) {
    vi.push(VisitInfo {
        privilege: ExtendedPriv.0,
        dynamic_privs: privs,
        dynamic_with_grant: with_grant,
        err,
        ..Default::default()
    });
}

// go: appendMultiColumns2VisitInfo L1
pub fn append_multi_columns2_visit_info(
    vi: &mut Vec<VisitInfo>,
    privilege: u64,
    db: &str,
    tbl: &str,
    cols: &[Arc<ColumnInfo>],
    err: Option<Error>,
) {
    for col in cols {
        vi.push(VisitInfo {
            privilege,
            db: db.to_owned(),
            table: tbl.to_owned(),
            column: col.name.l.clone(),
            err: err.clone(),
            ..Default::default()
        });
    }
}

// go: getInnerFromParenthesesAndUnaryPlus L1
pub fn get_inner_from_parentheses_and_unary_plus(expr: &Expr) -> &Expr {
    if let Expr::Paren(pexpr) = expr {
        return get_inner_from_parentheses_and_unary_plus(pexpr);
    }
    if let Expr::Unary(uexpr) = expr
        && uexpr.op == ast::Op::Plus
    {
        return get_inner_from_parentheses_and_unary_plus(&uexpr.v);
    }
    expr
}

// ---------------------------------------------------------------- SELECT and data source

impl<'ast> PlanBuilder<'ast> {
    /// buildSelect. The branches the slice does not need stop at the place Go takes them
    /// (D09.1 spec §5): table hints (D13.3), WITH (D09.5), GROUP BY, window functions,
    /// aggregation and DISTINCT (D09.4), subqueries (D09.6), the lock clause (the lock
    /// operator, D09.14). The recursive-CTE checks and `tryToBuildSequence` only act inside
    /// CTEs, which are not built yet.
    ///
    /// `sel.QueryBlockOffset` is the hint handler's ([`QBHintHandler::query_block_offset`],
    /// D13.3): 0 without a handler or for a SELECT it did not walk, as with Go's
    /// `BuildLogicalPlanForTest`. `pushTableHints` goes through the handler's
    /// `GetCurrentStmtHints` and `ParsePlanHints`, which stop at a non-empty hint list
    /// (`blocked-by:D02.2`, the parser's hint grammar); with no hints the pushed `PlanHints`
    /// is empty, so `popTableHints` has no unmatched-hint warning and the builder keeps no
    /// hint stack (`TableHints()` is always the empty hints, see [`NoTableHints`]).
    // go: PlanBuilder.buildSelect L2 (the branches of hints, CTEs, windows, ROLLUP, subqueries and locks are boundary stubs)
    pub fn build_select(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
    ) -> Result<PlanId, Error> {
        let qb_offset = self
            .hint_processor
            .as_ref()
            .map_or(0, |hp| hp.query_block_offset(sel));
        self.push_select_offset(qb_offset);
        if let Err(e) = self.push_table_hints(ctx, sel, qb_offset) {
            self.pop_select_offset();
            return Err(e);
        }
        let origin = self.in_straight_join;
        self.in_straight_join = sel.select_stmt_opts.straight_join;
        let res = self.build_select_inner(ctx, sel);
        self.in_straight_join = origin;
        self.pop_select_offset();
        // table hints are only visible in the current SELECT statement.
        // (b.popTableHints: the statement has none.)
        res
    }

    /// Go ignores the error of `ParsePlanHints` (a hint whose restore fails); the boundary
    /// of a non-empty list (`blocked-by:D02.2`) is returned instead, never dropped. The
    /// `SelectStmtOpts` hints are the same list in Go (the parser copies them into
    /// `TableHints`); the Rust parser keeps both fields, so both are read.
    // go: PlanBuilder.pushTableHints L2 (no hint stack is kept: the hints of a statement without hints are empty, see buildSelect)
    fn push_table_hints(
        &mut self,
        ctx: &PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
        current_level: isize,
    ) -> Result<(), Error> {
        let hints: Vec<&tidb_parser::gen_types::TableOptimizerHint> = sel
            .table_hints
            .iter()
            .chain(sel.select_stmt_opts.table_hints.iter())
            .collect();
        let sc = ctx.vars.stmt_ctx();
        let hints = match &self.hint_processor {
            Some(hp) => hp.get_current_stmt_hints(
                &hints,
                current_level,
                self.hint_state.as_mut(),
                Some(sc),
            )?,
            // Go's nil processor panics here; the builder is always given one by optimize and
            // BuildLogicalPlanForTest. Without one the list is read as is.
            None => hints,
        };
        let current_db = &ctx.vars.current_db;
        // The sub-query context: buildSelect is not reached inside a subquery yet (D09.6).
        let (_plan_hints, _sub_query_hint_flags) = parse_plan_hints(
            &hints,
            current_level,
            current_db,
            self.hint_processor.as_deref(),
            sc.stmt_hints.straight_join_order,
            false,
            false,
            true,
            sc,
        )?;
        // b.tableHintInfo = append(b.tableHintInfo, planHints): the hints are empty here.
        Ok(())
    }

    // go-part: PlanBuilder.buildSelect
    fn build_select_inner(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
    ) -> Result<PlanId, Error> {
        // b.buildingRecursivePartForCTE: no CTE is built yet (D09.5).

        // set for update read to true before building result set node
        if is_for_update_read_select_lock(sel.lock_info.as_deref()) {
            self.is_for_update_read = true;
        }

        // hints.CTEMerge: the statement has no table hints (D13.3).

        if sel.with.is_some() {
            // glue: blocked-by:D09.5 (common table expressions, buildWith)
            return Err(blocked("D09.5", "WITH (common table expressions)"));
        }

        let mut p = self.build_table_refs(ctx, sel.from.as_deref())?;

        let fields: Vec<SelectFieldW<'ast>> = sel
            .fields
            .fields
            .iter()
            .map(|f| SelectFieldW::of_ast(f, self.sql_text))
            .collect();
        let fields = self.unfold_wild_star(&ctx.arena, p, fields)?;
        // b.capFlag&canExpandAST: set only by CREATE VIEW (addAliasName, D09.15).

        let (gby_exprs, resolved_gby_items, gby_col_replacements) = if let Some(gby) = &sel.group_by
        {
            self.resolve_gby_exprs(ctx, p, gby, &fields)?
        } else {
            (Vec::new(), Vec::new(), HashMap::new())
        };

        // `checkOnlyFullGroupBy` should be executed before rewrite gbyExprs, because the field type of the fields
        // may change. For example, the length of a string field may change in `adjustRetFtForCastString`
        if SQLMode(ctx.vars.sql_mode).has_only_full_group_by()
            && sel.from.is_some()
            && !ctx.vars.optimizer_enable_new_only_full_group_by_check
        {
            self.check_only_full_group_by(
                &ctx.arena,
                p,
                sel,
                &fields,
                &resolved_gby_items,
                &gby_col_replacements,
            )?;
        }

        let (gby_cols, rollup) = if let Some(gby) = &sel.group_by {
            let (np, cols, r) =
                self.rewrite_gby_exprs(ctx, p, gby, &gby_exprs, &fields, &gby_col_replacements)?;
            p = np;
            (cols, r)
        } else {
            (Vec::new(), false)
        };

        let has_window_func_field = self.detect_select_window(sel);
        // Some SQL statements define WINDOW but do not use them. But we also need to check the window specification list.
        // For example: select id from t group by id WINDOW w AS (ORDER BY uids DESC) ORDER BY id;
        // We don't use the WINDOW w, but if the 'uids' column is not in the table t, we still need to report an error.
        let (window_agg_map, fields, window_agg_col_replacements) =
            if has_window_func_field || !sel.window_specs.is_empty() {
                // b.buildingRecursivePartForCTE: no CTE is built yet (D09.5).
                self.resolve_window_function(ctx, sel, p, fields)?
            } else {
                (AggMapper::new(), fields, HashMap::new())
            };
        // We must resolve having and order by clause before build projection,
        // because when the query is "select a+1 as b from t having sum(b) < 0", we must replace sum(b) to sum(a+1),
        // which only can be done before building projection and extracting Agg functions.
        let (having_map, order_map, fields, mut agg_col_replacements) = self
            .resolve_having_and_order_by(
                ctx,
                sel,
                p,
                fields,
                &resolved_gby_items,
                &gby_col_replacements,
            )?;
        agg_col_replacements.extend(window_agg_col_replacements);

        // We have to resolve correlated aggregate inside sub-queries before building aggregation and building projection,
        // for instance, count(a) inside the sub-query of "select (select count(a)) from t" should be evaluated within
        // the context of the outer query. So we have to extract such aggregates from sub-queries and put them into
        // SELECT field list.
        self.resolve_correlated_aggregates(sel, &fields)?;

        // b.allNames will be used in evalDefaultExpr(). Default function is special because it needs to find the
        // corresponding column name, but does not need the value in the column.
        // For example, select a from t order by default(b), the column b will not be in select fields. Also because
        // buildSort is after buildProjection, so we need get OutputNames before BuildProjection and store in allNames.
        // Otherwise, we will get select fields instead of all OutputNames, so that we can't find the column b in the
        // above example.
        self.all_names.push(names_of(&ctx.arena, p).clone());
        let res = self.build_select_rest(
            ctx,
            sel,
            p,
            &fields,
            gby_cols,
            rollup,
            has_window_func_field,
            &window_agg_map,
            &having_map,
            &order_map,
            &agg_col_replacements,
        );
        self.all_names.pop();
        res
    }

    // go-part: PlanBuilder.buildSelect
    #[allow(clippy::too_many_arguments)]
    fn build_select_rest(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
        mut p: PlanId,
        fields: &[SelectFieldW<'ast>],
        gby_cols: Vec<ExprRef>,
        rollup: bool,
        has_window_func_field: bool,
        window_agg_map: &AggMapper<'ast>,
        having_map: &AggMapper<'ast>,
        order_map: &AggMapper<'ast>,
        agg_col_replacements: &HashMap<ByAddr<'ast, ast::ColumnName>, usize>,
    ) -> Result<PlanId, Error> {
        if let Some(w) = &sel.where_ {
            p = self.build_selection(ctx, p, w, None)?;
        }
        if let Some(l) = &sel.lock_info
            && l.lock_type != ast::SelectLockType::None
        {
            // The lock tables' visit infos and StmtCtx.LockTableIDs, then b.buildSelectLock.
            // glue: blocked-by:D09.14 (SELECT ... FOR UPDATE / FOR SHARE: the lock operator LogicalLock and buildSelectLock)
            return Err(blocked(
                "D09.14",
                "a locking read (LogicalLock, buildSelectLock)",
            ));
        }
        self.handle_helper.pop_map();
        self.handle_helper.push_map(None);

        // Include auxiliary fields appended by resolveWindowFunction / resolveHavingAndOrderBy
        // (Go's detectSelectAgg sees them because Go appends into sel.Fields.Fields in place).
        let has_agg = self.detect_select_agg(sel)
            || fields
                .iter()
                .any(|f| f.wild_card.is_none() && f.expr().is_some_and(has_agg_flag));
        let mut need_build_agg = has_agg;
        let mut agg_funcs = Vec::new();
        let mut total_map = AggMapper::new();
        if has_agg {
            // b.buildingRecursivePartForCTE: no CTE is built yet (D09.5).
            (agg_funcs, total_map) = self.extract_agg_funcs_in_select_fields(fields);
            // len(aggFuncs) == 0 and sel.GroupBy == nil indicates that all the aggregate functions inside the SELECT fields
            // are actually correlated aggregates from the outer query, which have already been built in the outer query.
            if agg_funcs.is_empty() && sel.group_by.is_none() {
                need_build_agg = false;
            }
        }
        if need_build_agg {
            // if rollup syntax is specified, Expand OP is required to replicate the data to feed different grouping layout.
            if rollup {
                // glue: blocked-by:D09.4 (GROUP BY ... WITH ROLLUP: buildExpand, D09.4 part C)
                return Err(blocked("D09.4", "GROUP BY ... WITH ROLLUP (buildExpand)"));
            }
            let (np, agg_index_map) =
                self.build_aggregation(ctx, p, &agg_funcs, gby_cols, fields, agg_col_replacements)?;
            p = np;
            for idx in total_map.values_mut() {
                *idx = agg_index_map[&(*idx as usize)] as isize;
            }
        }

        // According to https://dev.mysql.com/doc/refman/8.0/en/window-functions-usage.html,
        // we can only process window functions after having clause, so `considerWindow` is false now.
        let (np, mut proj_exprs, mut old_len) = self.build_projection(
            ctx,
            p,
            fields,
            need_build_agg.then_some(&total_map),
            None,
            false,
            sel.order_by.is_some(),
        )?;
        p = np;

        if let Some(h) = &sel.having {
            self.cur_clause = ClauseCode::HavingClause;
            p = self.build_selection(ctx, p, &h.expr, Some(having_map))?;
        }

        let (window_specs, window_spec_index) = build_window_specs(&sel.window_specs)?;
        self.window_specs = window_specs;
        self.window_spec_index = window_spec_index;

        let mut window_mapper: Option<WindowMapper<'ast>> = None;
        if has_window_func_field || !sel.window_specs.is_empty() {
            let window_funcs = extract_window_funcs(fields);
            // we need to check the func args first before we check the window spec
            self.check_window_func_args(ctx, p, &window_funcs, Some(window_agg_map))?;
            let (grouped_funcs, ordered_spec, local_specs) =
                self.group_window_funcs(ctx, &window_funcs)?;
            let (np, mut wm) = self.build_window_functions(
                ctx,
                p,
                &grouped_funcs,
                &ordered_spec,
                &local_specs,
                Some(window_agg_map),
            )?;
            p = np;
            // `hasWindowFuncField == false` means there's only unused named window specs without window functions.
            // In such case plan `p` is not changed, so we don't have to build another projection.
            if has_window_func_field {
                // Now we build the window function fields.
                let (np, pe, ol) = self.build_projection(
                    ctx,
                    p,
                    fields,
                    Some(window_agg_map),
                    Some(&mut wm),
                    true,
                    false,
                )?;
                p = np;
                proj_exprs = pe;
                old_len = ol;
            }
            window_mapper = Some(wm);
        }

        if sel.distinct {
            p = self.build_distinct(ctx, p, old_len)?;
        }

        if let Some(o) = &sel.order_by {
            // flag it if cte contain order by (no CTE is being built, D09.5)

            // We need to keep the ORDER BY clause for the following cases:
            // 1. The select is top level query, order should be honored
            // 2. The query has LIMIT clause
            // 3. The control flag requires keeping ORDER BY explicitly
            if self.qb_offset.len() == 1
                || sel.limit.is_some()
                || !ctx.vars.remove_orderby_in_subquery
            {
                if SQLMode(ctx.vars.sql_mode).has_only_full_group_by() {
                    p = self.build_sort_with_check(
                        ctx,
                        p,
                        &o.items,
                        Some(order_map),
                        window_mapper.as_ref(),
                        Some(&proj_exprs),
                        old_len,
                        sel.distinct,
                    )?;
                } else {
                    p =
                        self.build_sort(ctx, p, &o.items, Some(order_map), window_mapper.as_ref())?;
                }
            }
        }

        if let Some(l) = &sel.limit {
            p = self.build_limit(ctx, p, l)?;
        }

        // sel.Fields.Fields = originalFields: the builder's field list is dropped.
        if old_len != schema_of(&ctx.arena, p).len() {
            let exprs = column2_exprs(&schema_of(&ctx.arena, p).columns[..old_len]);
            let mut proj = LogicalProjection::init(exprs, ctx, self.get_select_offset());
            proj.set_children(vec![p]);
            let mut schema =
                new_schema(schema_of(&ctx.arena, p).clone_schema().columns[..old_len].to_vec());
            for col in schema.columns.iter_mut() {
                Arc::make_mut(col).unique_id = ctx.vars.alloc_plan_column_id();
            }
            proj.sp
                .set_output_names(names_of(&ctx.arena, p)[..old_len].to_vec());
            proj.sp.set_schema(Some(schema));
            // b.tryToBuildSequence(currentLayerCTEs, proj): no CTE of this query block.
            return Ok(ctx.arena.alloc(Box::new(proj)));
        }

        // b.tryToBuildSequence(currentLayerCTEs, p): no CTE of this query block.
        Ok(p)
    }

    // go: PlanBuilder.buildTableDual L1
    pub fn build_table_dual(&mut self, ctx: &mut PlanCtx<'_>) -> PlanId {
        self.handle_helper.push_map(None);
        let mut dual = LogicalTableDual::init(ctx, self.get_select_offset());
        dual.row_count = 1;
        ctx.arena.alloc(Box::new(dual))
    }

    /// buildDataSource: the data source of a table (D09.1 spec §4). Boundary stubs (spec
    /// §5): CTE names (D09.5), memory tables (D10.11), partitioned tables (D09.12), views
    /// (this block, part 2), virtual generated columns (part 2), table samples (D02.2) and the
    /// union scan of a table the transaction wrote, of a local temporary table or of a cached
    /// table (`LogicalUnionScan`, D09.14). The statement's tables are read through the
    /// InfoSchema lookup ([`PlanBuilder::is`]); the hint-related fields of `DataSource` stay
    /// empty because the statement has no hints (D13.3).
    // go: PlanBuilder.buildDataSource L2 (CTEs, memory tables, partitions, views, virtual generated columns, samples and union scans are boundary stubs)
    pub fn build_data_source(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        tn: &'ast ast::TableName,
        as_name: &'ast ast::CIStr,
    ) -> Result<PlanId, Error> {
        self.opt_flag |= FlagPredicateSimplification;
        let mut db_name = ci(&tn.schema);

        if db_name.l.is_empty() {
            // Try CTE: b.tryBuildCTE finds nothing, b.outerCTEs is empty (WITH stops
            // before, D09.5).
            db_name = CIStr::new(&ctx.vars.current_db);
        }

        // is := b.is (the view stack is empty: views are not built yet).
        let Some(is) = self.is.clone() else {
            // glue: blocked-by:D05.4 (the statement's InfoSchema, PlanBuilder.Init)
            return Err(blocked("D05.4", "the InfoSchema of the plan builder"));
        };

        let tbl = is.0.table_by_name(&db_name, &ci(&tn.name))?;

        let table_info = try_lock_mdl_and_update_schema_if_necessary(
            ctx.vars,
            &db_name,
            tbl,
            &*is.0,
            self.domain_is.as_ref().map(|d| &*d.0),
        )?;

        // b.isCreateView && tableInfo.TempTableType == model.TempTableLocal: CREATE VIEW (D09.15).

        // tbl.Type().IsVirtualTable(): the tables of the memory databases.
        if is_mem_db(&db_name.l) {
            // glue: blocked-by:D10.11 (memory tables: buildMemTable)
            return Err(blocked("D10.11", "a memory table (buildMemTable)"));
        }

        let tbl_name = if as_name.l().is_empty() {
            ci(&tn.name)
        } else {
            ci(as_name)
        };

        if table_info.get_partition_info().is_some() {
            // The partition processor flags, the global stats check and the partition names.
            // glue: blocked-by:D09.12 (partitioned tables: the table layer only marks them, D06.4 part 1)
            return Err(blocked(
                "D09.12",
                "a partitioned table (partition pruning and the partitioned table)",
            ));
        } else if !tn.partition_names.is_empty() {
            return Err(ErrPartitionClauseOnNonpartitioned.clone());
        }

        let mut possible_paths = get_possible_access_paths(
            ctx.vars,
            &tn.index_hints,
            &table_info,
            &db_name,
            self.is_for_update_read,
            self.opt_flag & crate::core::rule::logical_rules::FlagPartitionProcessor > 0,
        )?;

        if table_info.is_view() {
            // Get the hints belong to the current view, then b.BuildDataSourceFromView.
            // Note (R29/R30, specs/D04.1-privilege/spec.md §5.2 item 3): BuildDataSourceFromView
            // must register and enforce caller SelectPriv on the view itself before inner expansion.
            // glue: blocked-by:D09.1 (views: BuildDataSourceFromView, part 2 of this block)
            return Err(blocked("D09.1", "a view (BuildDataSourceFromView)"));
        }

        // Single-point table-read SELECT privilege registration (R29, specs/D04.1-privilege/spec.md
        // §5.2 items 1–2): registered unconditionally for base tables and sequences (before
        // sequence -> TableDual conversion and before any expression rewrite / pruning).
        // Future D10.11 (buildMemTable) and D09.12 (partitioned tables) branches must reuse this.
        self.append_table_select_visit_info(&db_name.l, &table_info.name.l);

        if table_info.is_sequence() {
            if tn.table_sample.is_some() {
                // glue: blocked-by:D02.2 (TABLESAMPLE)
                return Err(blocked("D02.2", "TABLESAMPLE"));
            }
            // When the source is a Sequence, we convert it to a TableDual, as what most databases do.
            return Ok(self.build_table_dual(ctx));
        }

        // remain tikv access path to generate point get acceess path if existed
        // see detail in issue: https://github.com/pingcap/tidb/issues/39543
        if !(self.is_for_update_read && ctx.vars.state.txn_ctx.is_explicit) {
            // Skip storage engine check for CreateView (capFlag: CREATE VIEW only).
            possible_paths =
                filter_path_by_isolation_read(ctx.vars, possible_paths, &tbl_name, &db_name)?;
        }

        // Try to substitute generate column only if there is an index on generate column.
        let tbl_cols = table_info.cols();
        for index in table_info.indices.iter().flatten() {
            if index.state != StatePublic {
                continue;
            }
            for index_col in index.columns.iter().flatten() {
                let col_info =
                    tbl_cols[index_col.offset as usize].expect("Go reads a nil *table.Column");
                if col_info.is_generated() && !col_info.generated_stored {
                    self.opt_flag |= FlagGcSubstitute;
                    break;
                }
            }
        }

        // b.inUpdateStmt / b.inDeleteStmt: the write path (D09.15); tbl.Cols().
        let columns: Vec<Arc<ColumnInfo>> = table_info
            .columns
            .iter()
            .flatten()
            .filter(|c| c.state == StatePublic)
            .map(Arc::clone)
            .collect();
        // extract the IndexMergeHint: the statement has no table hints (D13.3).
        let all_paths: Vec<_> = possible_paths.clone();

        let count_cnt = columns.len() + 1; // +1 for an extra handle column
        let mut ds = DataSource::init(
            ctx,
            Arc::clone(&table_info),
            db_name.clone(),
            Vec::with_capacity(count_cnt),
            new_schema(Vec::new()),
            self.get_select_offset(),
        );
        ds.table_as_name = Some(ci(as_name));
        ds.possible_access_paths = possible_paths;
        ds.all_possible_access_paths = all_paths;
        ds.partition_names = tn.partition_names.iter().map(ci).collect();
        ds.is_for_update_read = self.is_for_update_read;
        let mut handle_cols: Option<HandleCols> = None;
        let mut schema = new_schema(Vec::with_capacity(columns.len()));
        let mut names: NameSlice = Vec::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            ds.columns.push(Arc::clone(col));
            names.push(Arc::new(FieldName {
                db_name: db_name.clone(),
                tbl_name: table_info.name.clone(),
                col_name: col.name.clone(),
                orig_tbl_name: table_info.name.clone(),
                orig_col_name: col.name.clone(),
                // For update statement and delete statement, internal version should see the special middle state column, while user doesn't.
                not_explicit_usable: col.state != StatePublic,
                ..FieldName::default()
            }));
            let new_col = Arc::new(Column {
                unique_id: ctx.vars.alloc_plan_column_id(),
                id: col.id,
                ret_type: Arc::new(col.field_type.clone()),
                orig_name: names[i].string(),
                is_hidden: col.hidden,
                ..Column::default()
            });
            // col.IsPKHandleColumn(tableInfo)
            if table_info.pk_is_handle && has_pri_key_flag(col.field_type.get_flag()) {
                handle_cols = Some(new_int_handle_cols(Arc::clone(&new_col)));
            }
            schema.append([Arc::clone(&new_col)]);
            ds.append_table_col(new_col);
        }
        // We append an extra handle column to the schema when the handle
        // column is not the primary key of "ds".
        if handle_cols.is_none() {
            if table_info.is_common_handle {
                let primary_idx =
                    find_primary_index(&table_info).expect("a clustered table has a primary index");
                handle_cols = Some(HandleCols::Common(new_common_handle_cols(
                    Arc::clone(&table_info),
                    Arc::clone(primary_idx),
                    &ds.tbl_cols,
                )));
            } else {
                let extra_col = Arc::new(ds.new_extra_handle_schema_col(ctx));
                // R32 (D5 option A): track physical table by unique_id so privilege collection
                // enforces full-table SELECT on `_tidb_rowid` while keeping `orig_tbl_name == ""`
                // on the wire (matching Go MySQL column metadata).
                self.rowid_orig_tbl
                    .insert(extra_col.unique_id, table_info.name.l.clone());
                handle_cols = Some(new_int_handle_cols(Arc::clone(&extra_col)));
                ds.columns.push(Arc::new(new_extra_handle_col_info()));
                schema.append([Arc::clone(&extra_col)]);
                names.push(Arc::new(FieldName {
                    db_name: db_name.clone(),
                    tbl_name: table_info.name.clone(),
                    col_name: ExtraHandleName.clone(),
                    orig_col_name: ExtraHandleName.clone(),
                    ..FieldName::default()
                }));
                ds.append_table_col(extra_col);
            }
        }
        let handle_cols = handle_cols.expect("set above");
        ds.handle_cols = Some(handle_cols.clone());
        ds.un_mutable_handle_cols = Some(handle_cols.clone());
        let mut handle_map: HashMap<i64, Vec<HandleCols>> = HashMap::new();
        handle_map.insert(table_info.id, vec![handle_cols]);
        self.handle_helper.push_map(Some(handle_map));
        let schema_cols = schema.columns.clone();
        ds.sp.set_schema(Some(schema));
        ds.sp.set_output_names(names);
        // setPreferredStoreType will mark user preferred path, which should be shared by all ds alternative. Here
        // we only mark it for the AllPossibleAccessPaths(since the element inside is shared by PossibleAccessPaths),
        // and the following ds alternative will clone/inherit this mark from DS copying.
        set_preferred_store_type(&mut ds, None);
        if tn.table_sample.is_some() {
            // ds.SampleInfo = tablesampler.NewTableSampleInfo(tn.TableSample, ...)
            // glue: blocked-by:D02.2 (TABLESAMPLE)
            return Err(blocked("D02.2", "TABLESAMPLE"));
        }
        self.is_sampling = false;

        let origin_col_priv = self.check_col_priv;
        for (i, _) in schema_cols.iter().enumerate() {
            if i < columns.len() && columns[i].is_generated() && !columns[i].generated_stored {
                // b.rewrite(ctx, columns[i].GeneratedExpr.Clone(), ds, nil, true) and colExpr.VirtualExpr
                // glue: blocked-by:D09.1 (virtual generated columns: the column's generation expression, part 2 of this block)
                return Err(blocked(
                    "D09.1",
                    "a virtual generated column (its generation expression)",
                ));
            }
        }
        self.check_col_priv = origin_col_priv;

        let col_infos: Vec<ColumnInfo> = ds.columns.iter().map(|c| ColumnInfo::clone(c)).collect();
        // Init CommonHandleCols and CommonHandleLens for data source.
        if table_info.is_common_handle {
            let primary_idx =
                find_primary_index(&table_info).expect("a clustered table has a primary index");
            let (cols, lens) = index_info2_full_cols(&col_infos, &schema_cols, primary_idx);
            if cols.iter().any(Option::is_none) {
                // glue: blocked-by:D09.1 (a clustered primary key over a column that is not public)
                return Err(blocked(
                    "D09.1",
                    "a clustered primary key over a column that is not public",
                ));
            }
            ds.common_handle_cols = cols.into_iter().flatten().collect();
            ds.common_handle_lens = lens;
        }
        // Init FullIdxCols, FullIdxColLens for accessPaths.
        let mut contain_expr_prefix_uk = false;
        for path in ds.all_possible_access_paths.iter_mut() {
            if !path.is_int_handle_path {
                let index = Arc::clone(path.index.as_ref().expect("an index path"));
                (path.full_idx_cols, path.full_idx_col_lens) =
                    index_info2_full_cols(&col_infos, &schema_cols, &index);

                // check whether the path's index has a tidb_shard() prefix and the index column count
                // more than 1. e.g. index(tidb_shard(a), a)
                // set UkShardIndexPath only for unique secondary index
                if !path.is_common_handle_path {
                    // tidb_shard expression must be first column of index
                    if let Some(Some(col)) = path.full_idx_cols.first()
                        && gc_column_expr_is_tidb_shard(col.virtual_expr.as_deref())
                        && index.columns.iter().flatten().count() > 1
                        && index.unique
                    {
                        path.is_uk_shard_index_path = true;
                        contain_expr_prefix_uk = true;
                    }
                }
            }
        }
        if contain_expr_prefix_uk {
            ds.contain_expr_prefix_uk = true;
        }
        // Go shares each *AccessPath between AllPossibleAccessPaths and PossibleAccessPaths
        // (they are copies of the same slice), so the writes above are seen through both.
        ds.possible_access_paths = ds.all_possible_access_paths.clone();

        let dirty = table_has_dirty_content(ctx, &table_info)?;
        if dirty
            || table_info.temp_table_type == TempTableLocal
            || table_info.table_cache_status_type == TableCacheStatusEnable
        {
            // us := logicalop.LogicalUnionScan{HandleCols: handleCols}.Init(...)
            // glue: blocked-by:D09.14 (LogicalUnionScan: the transaction's writes, local temporary tables and cached tables)
            return Err(blocked(
                "D09.14",
                "a union scan (a table the transaction wrote, a local temporary table or a cached table)",
            ));
        }

        // Adding ExtraPhysTblIDCol for SelectLock (SELECT FOR UPDATE) is done when building SelectLock

        // sessionVars.StmtCtx.TblInfo2UnionScan[tableInfo] = dirty: dirty is false here (a
        // dirty table stopped above), and the union scan that reads the map is not built
        // (D09.1 spec §5).
        Ok(ctx.arena.alloc(Box::new(ds)))
    }
}

/// The `defer` of `coalesceCommonColumns` that restores `checkColPriv` on every return,
/// errors included (M-DEFER: `defer` → drop guard).
// glue: M-DEFER drop guard for PlanBuilder.coalesceCommonColumns
struct RestoreCheckColPriv<'a, 'ast> {
    b: &'a mut PlanBuilder<'ast>,
    origin: CheckSelectColumnPrivOption,
}

impl Drop for RestoreCheckColPriv<'_, '_> {
    // glue: the deferred b.checkColPriv = originColPriv
    fn drop(&mut self) {
        self.b.check_col_priv = self.origin;
    }
}

impl<'ast> std::ops::Deref for RestoreCheckColPriv<'_, 'ast> {
    type Target = PlanBuilder<'ast>;
    // glue: access to the builder through the guard
    fn deref(&self) -> &PlanBuilder<'ast> {
        self.b
    }
}

impl std::ops::DerefMut for RestoreCheckColPriv<'_, '_> {
    // glue: access to the builder through the guard
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.b
    }
}

/// findJoinFullSchema walks through single-child wrapper operators (e.g., LogicalSelection
/// created by ON clauses on inner joins) to find an underlying LogicalJoin or LogicalApply
/// with FullSchema. This is needed because USING/NATURAL joins set FullSchema on the
/// LogicalJoin, but when that join has an ON condition with InnerJoin type, the result is
/// wrapped in a LogicalSelection, hiding the FullSchema from direct type assertions.
///
/// L2: there is no `LogicalApply` yet (the apply part of the join block, with correlated
/// subqueries), so its arm is not here.
// go: findJoinFullSchema L2 (no LogicalApply arm yet: the apply part)
fn find_join_full_schema(arena: &PlanArena, mut p: PlanId) -> Option<(&Schema, &NameSlice)> {
    loop {
        let node = arena.get(p);
        match node.kind() {
            LogicalKind::Join => {
                let x = arena
                    .downcast_ref::<LogicalJoin>(p)
                    .expect("a Join is a LogicalJoin");
                return x.full_schema.as_ref().map(|s| (s, &x.full_names));
            }
            LogicalKind::Selection => {
                // LogicalSelection is a transparent wrapper (e.g., from ON clauses); look through it.
                // Do NOT walk through LogicalProjection: it represents a derived table boundary,
                // and exposing its inner join's FullSchema would leak inner table aliases.
                let children = node.children();
                if children.len() != 1 {
                    return None;
                }
                p = children[0];
            }
            _ => return None,
        }
    }
}

/// containsLateralTableSource checks if a ResultSetNode contains a LATERAL table source
/// anywhere in its subtree. Used only to decide whether to push outerSchemas before
/// building the right side, so nested LATERAL sources can resolve outer columns.
// go: containsLateralTableSource L1
fn contains_lateral_table_source(node: &ast::ResultSetNode) -> bool {
    match node {
        ast::ResultSetNode::TableSource(n) => {
            if n.lateral {
                return true;
            }
            // Descend into the inner source (derived table / set-op) so nested
            // LATERAL inside a subquery or set-op used as a table source is detected.
            contains_lateral_table_source(&n.source)
        }
        ast::ResultSetNode::Join(n) => {
            let left = n.left.as_ref().is_some_and(contains_lateral_table_source);
            // For parenthesized single table refs, the parser creates Join{Left: TableSource, Right: nil}
            match &n.right {
                None => left,
                // Check both sides for nested LATERAL
                Some(right) => left || contains_lateral_table_source(right),
            }
        }
        _ => false,
    }
}

/// isImmediateLateralTableSource checks whether the top-level ResultSetNode is itself a
/// LATERAL TableSource, without recursing into both sides of a multi-table nested join.
/// A parenthesized single-table form (Join{Left: source, Right: nil}) is transparent and
/// is unwrapped, but a multi-table join on the right side is not itself a LATERAL source.
/// This is used after rightPlan is built to decide whether to produce a LogicalApply.
///
/// Its only caller is the LATERAL branch of `buildJoin`, which stops at
/// `blocked-by:D09.3` until the apply part is ported, so it is unused outside tests.
// go: isImmediateLateralTableSource L1
#[allow(dead_code)]
fn is_immediate_lateral_table_source(node: &ast::ResultSetNode) -> bool {
    match node {
        ast::ResultSetNode::TableSource(n) => n.lateral,
        ast::ResultSetNode::Join(n) => {
            // Parenthesized single-table ref: parser creates Join{Left: source, Right: nil}
            if n.right.is_none() {
                return n
                    .left
                    .as_ref()
                    .is_some_and(is_immediate_lateral_table_source);
            }
            // A multi-table join is not itself a single LATERAL source
            false
        }
        _ => false,
    }
}

/// findColFromNaturalUsingJoin is used to recursively find the column from the
/// underlying natural-using-join.
/// e.g. For SQL like `select t2.a from t1 join t2 using(a) where t2.a > 0`, the
/// plan will be `join->selection->projection`. The schema of the `selection`
/// will be `[t1.a]`, thus we need to recursively retrieve the `t2.a` from the
/// underlying join.
///
/// L2: there is no `LogicalMaxOneRow` yet (subqueries, D09.6). A column missing from the
/// full schema panics, as Go's index -1 does.
// go: findColFromNaturalUsingJoin L2 (no LogicalMaxOneRow arm yet: subqueries)
fn find_col_from_natural_using_join(
    arena: &PlanArena,
    p: PlanId,
    col: &Column,
) -> Option<Arc<FieldName>> {
    let node = arena.get(p);
    match node.kind() {
        LogicalKind::Limit | LogicalKind::Selection | LogicalKind::TopN | LogicalKind::Sort => {
            find_col_from_natural_using_join(arena, node.children()[0], col)
        }
        LogicalKind::Join => {
            let x = arena
                .downcast_ref::<LogicalJoin>(p)
                .expect("a Join is a LogicalJoin");
            let full_schema = x.full_schema.as_ref()?;
            let idx = full_schema.column_index(col);
            Some(Arc::clone(
                &x.full_names[usize::try_from(idx).expect("the column is in the full schema")],
            ))
        }
        _ => None,
    }
}

/// The table hints of a query block (Go's `*h.PlanHints`): there are none here, because
/// `buildSelect` stops at any table hint (`blocked-by:D13.3`).
#[derive(Debug)]
pub enum NoTableHints {}

/// setPreferredStoreType marks the storage the `READ_FROM_STORAGE` hint prefers. Without
/// table hints (`hintInfo` is never set, see [`NoTableHints`]) there is nothing to mark.
// go: setPreferredStoreType L2 (the statement has no table hints: they are blocked-by:D13.3)
fn set_preferred_store_type(ds: &mut DataSource, hint_info: Option<&NoTableHints>) {
    let Some(hint_info) = hint_info else {
        return;
    };
    let _ = ds;
    match *hint_info {}
}

/// getStatsTable gets statistics information for a table specified by "tableID".
/// A pseudo statistics table is returned in any of the following scenario:
/// 1. tidb-server started and statistics handle has not been initialized.
/// 2. table row count from statistics is zero.
/// 3. statistics is outdated.
///
/// The new stack has no statistics handle (VS1 plan §3.2, the statistics cache is D11.2):
/// Go's first case, the pseudo table. Returns a `Result` for the statistics cache to come.
// go: getStatsTable L2 (the statistics handle is D11.2; the new stack has none)
pub fn get_stats_table(
    ctx: &PlanCtx<'_>,
    tbl_info: &tidb_model::TableInfo,
    pid: i64,
) -> Result<tidb_statistics::Table, Error> {
    let _ = (ctx, pid);
    // EnableOptimizerDebugTrace (debugTraceGetStatsTbl): only PLAN REPLAYER switches the
    // debug trace on, which the Rust AST does not have; physicalOptimize returns its boundary
    // (D02.2) first.
    // 1. tidb-server started and statistics handle has not been initialized.
    // statsHandle := domain.GetDomain(ctx).StatsHandle(): nil in the new stack.
    Ok(tidb_statistics::table::pseudo_table(tbl_info, false, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::expression_rewriter::node_addr;
    use crate::core::planbuilder::{HandleColHelper, new_plan_builder};

    /// Two nodes with the same content are different keys (Go's pointer identity), and a
    /// lookup by the address of a live node finds its key.
    #[test]
    fn by_addr_keys_by_identity() {
        let a = Box::new(ast::ColumnName {
            name: ast::CIStr::new("a"),
            ..Default::default()
        });
        let b = Box::new(ast::ColumnName::clone(&a));
        let mut m: HashMap<ByAddr<'_, ast::ColumnName>, usize> = HashMap::new();
        m.insert(ByAddr::new(&a), 1);
        m.insert(ByAddr::new(&b), 2);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&node_addr(&*a)), Some(&1));
        assert_eq!(m.get(&node_addr(&*b)), Some(&2));
    }

    #[test]
    fn spec_field_comments() {
        for (text, want) in [
            ("/*!40001 a */", " a "),
            ("/*!40001a*/", "a"),
            ("/*!M500001a*/", "a"),
            ("/*!1234a*/", "1234a"),
            ("a /* c */ + 1", "a /* c  + 1"),
            ("b'101'", "b'101'"),
        ] {
            assert_eq!(trim_spec_field_comments(text), want, "{text}");
        }
    }

    #[test]
    fn visit_info_helpers() {
        let mut vi = Vec::new();
        append_dynamic_visit_info(&mut vi, vec!["BACKUP_ADMIN".to_owned()], true, None);
        assert_eq!(vi[0].privilege, ExtendedPriv.0);
        assert_eq!(vi[0].dynamic_privs, vec!["BACKUP_ADMIN".to_owned()]);
        assert!(vi[0].dynamic_with_grant);
        let cols = vec![
            Arc::new(ColumnInfo {
                name: CIStr::new("A"),
                ..ColumnInfo::default()
            }),
            Arc::new(ColumnInfo {
                name: CIStr::new("b"),
                ..ColumnInfo::default()
            }),
        ];
        append_multi_columns2_visit_info(&mut vi, 1, "db", "t", &cols, None);
        assert_eq!(vi.len(), 3);
        assert_eq!((vi[1].column.as_str(), vi[2].column.as_str()), ("a", "b"));
        let mut b = new_plan_builder(&[]);
        b.visit_info = vi;
        b.pop_visit_info();
        assert_eq!(b.visit_info.len(), 2);
        b.visit_info.clear();
        b.pop_visit_info();
        assert!(b.visit_info.is_empty());
    }

    /// containsLateralTableSource looks into every subtree; isImmediateLateralTableSource
    /// only through the parenthesized single-table form (Go `:683-724`).
    #[test]
    fn lateral_table_source_detection() {
        let ts = |lateral: bool| {
            ast::ResultSetNode::TableSource(Box::new(ast::TableSource {
                source: ast::ResultSetNode::TableName(Box::default()),
                as_name: ast::CIStr::default(),
                lateral,
            }))
        };
        let join = |left: ast::ResultSetNode, right: Option<ast::ResultSetNode>| {
            ast::ResultSetNode::Join(Box::new(ast::Join {
                left: Some(left),
                right,
                ..ast::Join::default()
            }))
        };
        assert!(!contains_lateral_table_source(&ts(false)));
        assert!(contains_lateral_table_source(&ts(true)));
        assert!(is_immediate_lateral_table_source(&ts(true)));
        // Parenthesized single table: transparent for both.
        let paren = join(ts(true), None);
        assert!(contains_lateral_table_source(&paren));
        assert!(is_immediate_lateral_table_source(&paren));
        // A LATERAL nested in a multi-table join: contained, not immediate.
        let nested = join(ts(false), Some(ts(true)));
        assert!(contains_lateral_table_source(&nested));
        assert!(!is_immediate_lateral_table_source(&nested));
        assert!(!contains_lateral_table_source(&join(
            ts(false),
            Some(ts(false))
        )));
        // A LATERAL inside a derived table's source.
        let derived = ast::ResultSetNode::TableSource(Box::new(ast::TableSource {
            source: join(ts(false), Some(ts(true))),
            as_name: ast::CIStr::default(),
            lateral: false,
        }));
        assert!(contains_lateral_table_source(&derived));
        assert!(!is_immediate_lateral_table_source(&derived));
    }

    #[test]
    fn handle_col_helper_stack() {
        let mut h = HandleColHelper::default();
        h.push_map(None);
        h.push_map(Some(HashMap::new()));
        assert!(h.pop_map().is_some());
        assert!(h.pop_map().is_none());
        assert_eq!((h.stack_tail, h.id2_handle_map_stack.len()), (0, 0));
    }

    // ---- D09.4 part A unit tests (spec §6.3 & ruling R27 / #425) ----

    struct TestAcc;
    impl tidb_sessionctx::variable::session::GlobalVarAccessor for TestAcc {
        fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
            Ok(())
        }
        fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    struct SingleTableIS(Arc<tidb_model::TableInfo>);
    impl crate::core::plan_cache_utils::InfoSchemaLookup for SingleTableIS {
        fn table_by_name(
            &self,
            schema: &CIStr,
            table: &CIStr,
        ) -> Result<Arc<tidb_model::TableInfo>, Error> {
            if schema.l == "test" && table.l == self.0.name.l {
                return Ok(Arc::clone(&self.0));
            }
            Err(
                tidb_infoschema::error::ErrTableNotExists.fast_gen_by_args(vec![
                    tidb_base::gofmt::Arg::from(schema.o.clone()),
                    tidb_base::gofmt::Arg::from(table.o.clone()),
                ]),
            )
        }
        fn table_by_id(&self, id: i64) -> Result<Option<Arc<tidb_model::TableInfo>>, Error> {
            Ok((self.0.id == id).then(|| Arc::clone(&self.0)))
        }
        fn schema_name_by_id(&self, id: i64) -> Option<CIStr> {
            (id == 1).then(|| CIStr::new("test"))
        }
        fn schema_meta_version(&self) -> i64 {
            0
        }
        fn snapshot(&self) -> Arc<dyn crate::core::plan_cache_utils::InfoSchemaLookup> {
            Arc::new(SingleTableIS(Arc::clone(&self.0)))
        }
    }

    fn make_test_table_t() -> Arc<tidb_model::TableInfo> {
        let col = |id: i64, offset: isize, name: &str| {
            let mut ft = FieldType::default();
            ft.set_type(tidb_base::mysql::TypeLong);
            ft.set_flen(11);
            ft.set_charset("binary");
            ft.set_collate("binary");
            Arc::new(ColumnInfo {
                id,
                name: CIStr::new(name),
                offset,
                field_type: ft,
                state: tidb_model::StatePublic,
                ..ColumnInfo::default()
            })
        };
        Arc::new(tidb_model::TableInfo {
            id: 100,
            name: CIStr::new("t"),
            columns: Some(vec![col(1, 0, "a"), col(2, 1, "b")]),
            state: tidb_model::StatePublic,
            ..tidb_model::TableInfo::default()
        })
    }

    fn parse_and_preprocess_t_opts(
        sql: &str,
        is: &crate::core::preprocess::InfoSchemaHandle,
        prepare: bool,
    ) -> (ast::Stmt, crate::core::resolve::ContextRef) {
        let mut stmt = tidb_parser::parse(sql, &tidb_parser::ParseOptions::default())
            .unwrap()
            .stmts
            .remove(0);
        let mut vars = tidb_sessionctx::variable::session::SessionVars::new(Box::new(TestAcc));
        vars.current_db = "test".to_owned();
        let mut node = crate::core::resolve::new_node_w(&mut stmt);
        let mut opts: Vec<crate::core::preprocess::PreprocessOpt> =
            vec![crate::core::preprocess::with_preprocessor_return(
                crate::core::preprocess::PreprocessorReturn {
                    info_schema: Some(is.clone()),
                    ..Default::default()
                },
            )];
        if prepare {
            opts.push(Box::new(crate::core::preprocess::in_prepare));
        }
        let (_, res) = crate::core::preprocess::preprocess(&mut vars, &mut node, opts);
        res.unwrap();
        let rctx = node.get_resolve_context();
        (stmt, rctx)
    }

    fn parse_and_preprocess_t(
        sql: &str,
        is: &crate::core::preprocess::InfoSchemaHandle,
    ) -> (ast::Stmt, crate::core::resolve::ContextRef) {
        parse_and_preprocess_t_opts(sql, is, false)
    }

    fn build_stmt_once(
        stmt: &ast::Stmt,
        sql: &str,
        rctx: &crate::core::resolve::ContextRef,
        is: &crate::core::preprocess::InfoSchemaHandle,
        sql_mode: i64,
        params: Vec<tidb_types::datum::Datum>,
        in_execute: bool,
    ) -> Result<String, Error> {
        use std::sync::atomic::Ordering;
        use tidb_base::errors::RedactLogDisable;
        use tidb_sessionctx::exprstatic as es;

        let mut vars = tidb_sessionctx::variable::session::SessionVars::new(Box::new(TestAcc));
        vars.current_db = "test".to_owned();
        vars.sql_mode = sql_mode;
        vars.state.plan_id.0.store(0, Ordering::SeqCst);
        vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
        vars.stmt_ctx_mut().in_prepared_plan_building = in_execute;

        let mut builder = new_plan_builder(&[]);
        builder.init(&mut vars, Some(is.clone()), None);
        builder.sql_text = sql;
        builder.resolve_ctx = Some(Arc::clone(rctx));

        let ev = es::new_eval_context(vec![
            es::with_type_flags(tidb_types::context::DefaultStmtFlags),
            es::with_sql_mode(SQLMode(sql_mode)),
            es::with_location(Some(tidb_base::gostd::time::utc())),
            es::with_current_db("test".to_owned()),
            es::with_param_list(&params),
        ]);
        let expr_ctx = es::new_expr_context(vec![
            es::with_eval_ctx(ev),
            es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
            es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
        ]);
        let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
        ctx.has_dirty_content = Some(Arc::new(|_| false));
        let root = builder.build(&mut ctx, stmt)?;
        // Summarize LogicalAggregation agg_funcs and group_by_items for deterministic assertions.
        fn find_agg_summary(ctx: &PlanCtx<'_>, p: PlanId) -> Option<String> {
            let node = ctx.arena.get(p);
            if node.kind() == crate::LogicalKind::Aggregation {
                let agg = node.as_any().downcast_ref::<LogicalAggregation>().unwrap();
                let ectx = ctx.eval_ctx();
                let aggs: Vec<String> = agg
                    .agg_funcs
                    .iter()
                    .map(|a| {
                        let args: Vec<String> = a
                            .base
                            .args
                            .iter()
                            .map(|e| {
                                String::from_utf8_lossy(
                                    &e.string_with_ctx(&*ectx, RedactLogDisable),
                                )
                                .into_owned()
                            })
                            .collect();
                        format!("{}({})", a.base.name, args.join(","))
                    })
                    .collect();
                let gby: Vec<String> = agg
                    .group_by_items
                    .iter()
                    .map(|e| {
                        String::from_utf8_lossy(&e.string_with_ctx(&*ectx, RedactLogDisable))
                            .into_owned()
                    })
                    .collect();
                return Some(format!("aggs=[{}] gby=[{}]", aggs.join(";"), gby.join(",")));
            }
            node.children()
                .iter()
                .find_map(|&c| find_agg_summary(ctx, c))
        }
        Ok(find_agg_summary(&ctx, root).unwrap_or_default())
    }

    #[test]
    fn agg_col_replacements_having_alias_and_nested() {
        let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(
            make_test_table_t(),
        )) as _);
        let sql1 = "SELECT a + 1 AS x FROM t HAVING SUM(x) < 0";
        let (stmt1, rctx1) = parse_and_preprocess_t(sql1, &is);
        let s1 = build_stmt_once(&stmt1, sql1, &rctx1, &is, 0, vec![], false).unwrap();
        assert!(
            s1.contains("sum(plus(test.t.a, 1))"),
            "expected alias x rewritten to a + 1, got {s1}"
        );

        // Nested expression inside SUM(x + 2): x is rewritten once, inner column a is not re-replaced.
        let sql2 = "SELECT a + 1 AS x FROM t HAVING SUM(x + 2) < 0";
        let (stmt2, rctx2) = parse_and_preprocess_t(sql2, &is);
        let s2 = build_stmt_once(&stmt2, sql2, &rctx2, &is, 0, vec![], false).unwrap();
        assert!(
            s2.contains("sum(plus(plus(test.t.a, 1), 2))"),
            "expected nested alias x rewritten cleanly, got {s2}"
        );
    }

    #[test]
    fn prepared_repeated_execute_group_by_alias_and_position_idempotent() {
        let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(
            make_test_table_t(),
        )) as _);
        for sql in [
            "SELECT a + 1 AS x FROM t GROUP BY x",
            "SELECT a + 1 AS x FROM t GROUP BY 1",
        ] {
            let (stmt, rctx) = parse_and_preprocess_t(sql, &is);
            let run1 = build_stmt_once(
                &stmt,
                sql,
                &rctx,
                &is,
                tidb_base::mysql::ModeOnlyFullGroupBy.0,
                vec![],
                true,
            )
            .unwrap();
            let run2 = build_stmt_once(
                &stmt,
                sql,
                &rctx,
                &is,
                tidb_base::mysql::ModeOnlyFullGroupBy.0,
                vec![],
                true,
            )
            .unwrap();
            assert_eq!(run1, run2, "repeated EXECUTE must be idempotent for {sql}");
            assert!(
                run1.contains("gby=[plus(test.t.a, 1)]"),
                "unexpected GROUP BY plan: {run1}"
            );
        }
    }

    #[test]
    fn prepared_repeated_execute_having_agg_alias_with_shadowed_col_idempotent() {
        let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(
            make_test_table_t(),
        )) as _);
        // sql_mode = '' with shadowed table column `a`: `x` expands to `a + 1`, where `a`
        // resolves to table column `t.a` (not alias `a + 100`) on every EXECUTE.
        let sql1 = "SELECT a + 1 AS x, a + 100 AS a FROM t HAVING SUM(x) = 5";
        let (stmt1, rctx1) = parse_and_preprocess_t(sql1, &is);
        let r1a = build_stmt_once(&stmt1, sql1, &rctx1, &is, 0, vec![], true).unwrap();
        let r1b = build_stmt_once(&stmt1, sql1, &rctx1, &is, 0, vec![], true).unwrap();
        assert_eq!(r1a, r1b);
        assert!(r1a.contains("sum(plus(test.t.a, 1))"), "got {r1a}");

        // Default ONLY_FULL_GROUP_BY with WHERE a = 1 (single-value column).
        let sql2 = "SELECT a + 1 AS x, a + 100 AS a FROM t WHERE a = 1 HAVING SUM(x) = 5";
        let (stmt2, rctx2) = parse_and_preprocess_t(sql2, &is);
        let r2a = build_stmt_once(
            &stmt2,
            sql2,
            &rctx2,
            &is,
            tidb_base::mysql::ModeOnlyFullGroupBy.0,
            vec![],
            true,
        )
        .unwrap();
        let r2b = build_stmt_once(
            &stmt2,
            sql2,
            &rctx2,
            &is,
            tidb_base::mysql::ModeOnlyFullGroupBy.0,
            vec![],
            true,
        )
        .unwrap();
        assert_eq!(r2a, r2b);
        assert!(r2a.contains("sum(plus(test.t.a, 1))"), "got {r2a}");

        // R27 (#437): Go TiDB v8.5.8 fails with ERROR 1055 from the second EXECUTE onward
        // because resolving `x` inside `SUM(x)` shares the SELECT field's AST node and mutates
        // it in place. tidb-rs keeps the AST immutable so every build succeeds identically.
        let sql3 = "SELECT a+1 AS x FROM t GROUP BY a+1 HAVING SUM(x) > 0";
        let (stmt3, rctx3) = parse_and_preprocess_t(sql3, &is);
        let r3a = build_stmt_once(
            &stmt3,
            sql3,
            &rctx3,
            &is,
            tidb_base::mysql::ModeOnlyFullGroupBy.0,
            vec![],
            true,
        )
        .unwrap();
        let r3b = build_stmt_once(
            &stmt3,
            sql3,
            &rctx3,
            &is,
            tidb_base::mysql::ModeOnlyFullGroupBy.0,
            vec![],
            true,
        )
        .unwrap();
        assert_eq!(r3a, r3b);
        assert!(r3a.contains("sum(plus(test.t.a, 1))"), "got {r3a}");
        assert!(r3a.contains("gby=[plus(test.t.a, 1)]"), "got {r3a}");
    }

    /// Ruling R27 (#425, D09.4 spec §8 item 7 & §9.1 item 3, owner decision 2026-11-26 option A):
    /// `PREPARE s FROM 'SELECT ? AS z FROM t GROUP BY 1'` with plan cache OFF succeeds on
    /// repeated EXECUTE for both `@p = 1` and `@p = 99` (Go v8.5.8 fails with error 1105 on 99).
    #[test]
    fn prepared_group_by_position_pointing_to_param_marker() {
        let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(
            make_test_table_t(),
        )) as _);
        let sql = "SELECT ? AS z FROM t GROUP BY 1";
        let (stmt, rctx) = parse_and_preprocess_t_opts(sql, &is, true);
        let mode = tidb_base::mysql::ModeOnlyFullGroupBy.0;
        let run1 = build_stmt_once(
            &stmt,
            sql,
            &rctx,
            &is,
            mode,
            vec![tidb_types::datum::new_int_datum(1)],
            true,
        )
        .expect("EXECUTE with @p = 1 must succeed");
        let run2 = build_stmt_once(
            &stmt,
            sql,
            &rctx,
            &is,
            mode,
            vec![tidb_types::datum::new_int_datum(99)],
            true,
        )
        .expect("EXECUTE with @p = 99 must succeed under R27 (#425)");
        assert!(run1.contains("gby=[1]"), "expected gby=[1], got {run1}");
        assert!(run2.contains("gby=[99]"), "expected gby=[99], got {run2}");
    }

    #[test]
    fn defer_guard_restores_check_col_priv_on_error() {
        use tidb_sessionctx::exprstatic as es;

        let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(
            make_test_table_t(),
        )) as _);
        for sql in [
            // Fails inside build_aggregation when rewriting unknown column `missing`.
            "SELECT SUM(missing) FROM t",
            // Fails inside resolve_gby_exprs on out-of-range position 9.
            "SELECT a FROM t GROUP BY 9",
            // Fails inside rewrite_gby_exprs on unknown column `missing`.
            "SELECT COUNT(*) FROM t GROUP BY missing",
            // Fails inside check_window_func_args (NTILE requires positive argument).
            "SELECT NTILE(0) OVER () FROM t",
            // Fails inside build_window_functions (unknown column in ORDER BY).
            "SELECT ROW_NUMBER() OVER (ORDER BY missing) FROM t",
        ] {
            let (stmt, rctx) = parse_and_preprocess_t(sql, &is);
            let mut vars = tidb_sessionctx::variable::session::SessionVars::new(Box::new(TestAcc));
            vars.current_db = "test".to_owned();
            vars.sql_mode = tidb_base::mysql::ModeOnlyFullGroupBy.0;
            let mut builder = new_plan_builder(&[]);
            builder.init(&mut vars, Some(is.clone()), None);
            builder.sql_text = sql;
            builder.resolve_ctx = Some(rctx);
            builder.check_col_priv = CheckSelectColumnPrivOption::ReportTableErrOption;

            let ev = es::new_eval_context(vec![
                es::with_type_flags(tidb_types::context::DefaultStmtFlags),
                es::with_sql_mode(SQLMode(tidb_base::mysql::ModeOnlyFullGroupBy.0)),
                es::with_location(Some(tidb_base::gostd::time::utc())),
                es::with_current_db("test".to_owned()),
            ]);
            let expr_ctx = es::new_expr_context(vec![
                es::with_eval_ctx(ev),
                es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
                es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
            ]);
            let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
            ctx.has_dirty_content = Some(Arc::new(|_| false));
            assert!(
                builder.build(&mut ctx, &stmt).is_err(),
                "expected error for {sql}"
            );
            assert_eq!(
                builder.check_col_priv,
                CheckSelectColumnPrivOption::ReportTableErrOption,
                "check_col_priv must be restored on error for {sql}"
            );
        }
    }

    #[test]
    fn extract_agg_funcs_in_exprs_and_by_items() {
        let stmt = tidb_parser::parse(
            "SELECT SUM(a) + COUNT(b) FROM t ORDER BY MAX(a), MIN(b) DESC",
            &tidb_parser::ParseOptions::default(),
        )
        .unwrap()
        .stmts
        .remove(0);
        let ast::Stmt::Select(sel) = stmt else {
            panic!("expected SELECT");
        };
        let b = new_plan_builder(&[]);
        let exprs: Vec<Expr> = sel
            .fields
            .fields
            .into_iter()
            .filter_map(|f| f.expr)
            .collect();
        let (aggs, mapper) = b.extract_agg_funcs_in_exprs(&exprs);
        assert_eq!(aggs.len(), 2);
        assert_eq!(mapper.len(), 2);
        let by_items = &sel.order_by.as_ref().unwrap().items;
        let by_aggs = b.extract_agg_funcs_in_by_items(by_items);
        assert_eq!(by_aggs.len(), 2);
    }

    #[test]
    fn logical_aggregation_extract_correlated_cols_unit() {
        use tidb_expression::{
            CorrelatedColumn,
            aggregation::{AggFuncDesc, BaseFuncDesc},
        };
        use tidb_sessionctx::exprstatic as es;

        let mut vars = tidb_sessionctx::variable::session::SessionVars::new(Box::new(TestAcc));
        let expr_ctx = es::new_expr_context(vec![
            es::with_eval_ctx(es::new_eval_context(vec![])),
            es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
            es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
        ]);
        let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
        let mk_cor = |uid: i64| {
            PExpr::CorrelatedColumn(CorrelatedColumn::new(Column {
                unique_id: uid,
                ..Column::default()
            }))
            .into_ref()
        };
        let mut agg_desc = AggFuncDesc::from_base(
            BaseFuncDesc::from_parts(
                "sum".to_owned(),
                vec![mk_cor(20)],
                Some(Arc::new(FieldType::default())),
            ),
            false,
        );
        agg_desc.order_by_items.push(ByItems {
            expr: mk_cor(30),
            desc: false,
        });
        let agg = LogicalAggregation::init(vec![agg_desc], vec![mk_cor(10)], &mut ctx, 0);
        let uids: Vec<i64> = agg
            .extract_correlated_cols()
            .iter()
            .map(|c| c.column.unique_id)
            .collect();
        assert_eq!(uids, vec![10, 20, 30]);
    }

    /// Ruling R28 (#413, D09.4 spec §7 item 7 & §9.1 item 2, owner decision 2026-11-26):
    /// A compound decimal expression in a window RANGE INTERVAL bound (e.g. `INTERVAL 1.0+0.5 DAY`)
    /// R28 (#413): For compound decimal `INTERVAL` bounds (e.g. `INTERVAL 1.0+0.5 DAY`), tidb-rs
    /// derives `Constant.ret_type` from the evaluated `Datum` (`TypeNewDecimal` / `ETDecimal`),
    /// matching literal `INTERVAL 1.5 DAY` and `DATE_SUB` (2 days) rather than Go's `TypeUnspecified`
    /// (`ETString` -> `SubDateDatetimeString`, which truncates `"1.5"` to 1 day). Neither
    /// `SubDateDatetimeString` nor `SubDateDatetimeDecimal` is in the TiFlash allowlist, so Go does
    /// not push these windows down either. All non-ruled shapes (`1+1 PRECEDING`, `INTERVAL 1+1 DAY`,
    /// `INTERVAL (1.5) DAY`, prepared `INTERVAL ? DAY` with `1.5`) keep Go's zero `FieldType`.
    #[test]
    fn window_range_compound_decimal_interval_uses_decimal_date_func() {
        use tidb_expression::eval::{ArgsFallback, BuiltinFunc, SigDesc, SigDrivers};
        use tidb_expression::scalar_function::ScalarFunction;
        use tidb_sessionctx::exprstatic as es;

        let make_expr_ctx = |params: Vec<tidb_types::datum::Datum>| {
            let mut opts = vec![
                es::with_type_flags(tidb_types::context::DefaultStmtFlags),
                es::with_location(Some(tidb_base::gostd::time::utc())),
                es::with_current_db("test".to_owned()),
            ];
            if !params.is_empty() {
                opts.push(es::with_param_list(&params));
            }
            let ev = es::new_eval_context(opts);
            es::new_expr_context(vec![
                es::with_eval_ctx(ev),
                es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
                es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
            ])
        };
        let expr_ctx = make_expr_ctx(vec![]);

        let extract_start_bound = |sql: &str| -> (ast::Expr, bool) {
            let stmt = tidb_parser::parse(sql, &tidb_parser::ParseOptions::default())
                .unwrap()
                .stmts
                .remove(0);
            let ast::Stmt::Select(mut sel) = stmt else {
                panic!("expected SELECT");
            };
            let Some(ast::Expr::Window(wf)) = sel.fields.fields.remove(0).expr else {
                panic!("expected window function");
            };
            let start = wf.spec.frame.unwrap().extent.start;
            (
                start.expr.unwrap(),
                start.unit != ast::TimeUnitType::Invalid,
            )
        };

        // Verify Go's zero-value FieldType evaluates as ETString (field_type.go:429-453), so Go
        // selects SubDateDatetimeString rather than SubDateDatetimeInt.
        let go_zero_ft = FieldType::default();
        assert_eq!(go_zero_ft.get_type(), tidb_base::mysql::TypeUnspecified);
        assert_eq!(go_zero_ft.eval_type(), tidb_parser::types::ETString);

        let (compound_expr, compound_is_iv) = extract_start_bound(
            "SELECT SUM(a) OVER (ORDER BY d RANGE BETWEEN INTERVAL 1.0+0.5 DAY PRECEDING AND CURRENT ROW) FROM t",
        );
        let (literal_expr, literal_is_iv) = extract_start_bound(
            "SELECT SUM(a) OVER (ORDER BY d RANGE BETWEEN INTERVAL 1.5 DAY PRECEDING AND CURRENT ROW) FROM t",
        );
        assert!(compound_is_iv && literal_is_iv);

        let compound_const =
            eval_window_range_bound_constant(&*expr_ctx, &compound_expr, "", false, true).unwrap();
        let literal_const =
            eval_window_range_bound_constant(&*expr_ctx, &literal_expr, "", false, true).unwrap();

        assert_eq!(
            compound_const.ret_type.get_type(),
            tidb_base::mysql::TypeNewDecimal
        );
        assert_eq!(
            compound_const.ret_type.eval_type(),
            tidb_parser::types::ETDecimal
        );
        assert_eq!(
            compound_const.ret_type.get_type(),
            literal_const.ret_type.get_type()
        );
        assert_eq!(
            compound_const.value.to_string().unwrap(),
            literal_const.value.to_string().unwrap()
        );

        // Non-ruled shapes MUST keep Go's zero-value FieldType (TypeUnspecified / ETString):
        // (a) compound integer INTERVAL (`INTERVAL 1+1 DAY`), (b) parenthesized literal
        // (`INTERVAL (1.5) DAY`), (c) non-INTERVAL numeric RANGE (`1+1` or `?` with is_interval=false),
        // and (d) prepared `INTERVAL ? DAY` with Decimal(1.5).
        for (label, sql) in [
            (
                "compound integer INTERVAL 1+1 DAY",
                "SELECT SUM(a) OVER (ORDER BY d RANGE BETWEEN INTERVAL 1+1 DAY PRECEDING AND CURRENT ROW) FROM t",
            ),
            (
                "parenthesized literal INTERVAL (1.5) DAY",
                "SELECT SUM(a) OVER (ORDER BY d RANGE BETWEEN INTERVAL (1.5) DAY PRECEDING AND CURRENT ROW) FROM t",
            ),
        ] {
            let (expr, is_iv) = extract_start_bound(sql);
            assert!(is_iv);
            let c = eval_window_range_bound_constant(&*expr_ctx, &expr, "", false, is_iv).unwrap();
            assert_eq!(
                c.ret_type.get_type(),
                tidb_base::mysql::TypeUnspecified,
                "{label} must keep Go zero FieldType"
            );
            assert_eq!(
                c.ret_type.eval_type(),
                tidb_parser::types::ETString,
                "{label} must keep ETString"
            );
        }

        // Non-INTERVAL numeric RANGE bound (e.g. `1+1` or `1.0+0.5` with is_interval = false,
        // and SQL `RANGE BETWEEN ? PRECEDING`) keeps Go's zero FieldType.
        let non_interval_const =
            eval_window_range_bound_constant(&*expr_ctx, &compound_expr, "", false, false).unwrap();
        assert_eq!(
            non_interval_const.ret_type.get_type(),
            tidb_base::mysql::TypeUnspecified,
            "non-INTERVAL numeric RANGE bound must keep Go zero FieldType"
        );
        let (range_param_expr, range_param_is_iv) = extract_start_bound(
            "SELECT SUM(a) OVER (ORDER BY a RANGE BETWEEN ? PRECEDING AND CURRENT ROW) FROM t",
        );
        assert!(!range_param_is_iv);
        let mut dec_0_3 = tidb_types::MyDecimal::default();
        dec_0_3.from_string(b"0.3").unwrap();
        let range_param_ctx = make_expr_ctx(vec![tidb_types::datum::new_decimal_datum(dec_0_3)]);
        let range_param_const = eval_window_range_bound_constant(
            &*range_param_ctx,
            &range_param_expr,
            "",
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            range_param_const.ret_type.get_type(),
            tidb_base::mysql::TypeUnspecified,
            "RANGE BETWEEN ? PRECEDING must keep Go zero FieldType"
        );

        // Prepared `INTERVAL ? DAY` with bound Decimal(1.5) also stays outside R28 (zero FieldType).
        let (param_expr, param_is_iv) = extract_start_bound(
            "SELECT SUM(a) OVER (ORDER BY d RANGE BETWEEN INTERVAL ? DAY PRECEDING AND CURRENT ROW) FROM t",
        );
        assert!(param_is_iv);
        let mut dec_1_5 = tidb_types::MyDecimal::default();
        dec_1_5.from_string(b"1.5").unwrap();
        let param_ctx = make_expr_ctx(vec![tidb_types::datum::new_decimal_datum(dec_1_5)]);
        let param_const =
            eval_window_range_bound_constant(&*param_ctx, &param_expr, "", false, true).unwrap();
        assert_eq!(
            param_const.ret_type.get_type(),
            tidb_base::mysql::TypeUnspecified,
            "INTERVAL ? DAY must keep Go zero FieldType"
        );
        assert_eq!(
            param_const.ret_type.eval_type(),
            tidb_parser::types::ETString
        );

        // Verify neither Go's SubDateDatetimeString nor tidb-rs's SubDateDatetimeDecimal is in the
        // TiFlash pushdown allowlist (infer_pushdown.go:356-364), so R28 has no TiFlash pushdown cost.
        static SIG_SUB_STR: SigDesc = SigDesc {
            name: "builtinSubDateDatetimeStringSig",
            pb_code: tipb::ScalarFuncSig::SubDateDatetimeString as i32,
            vectorized: false,
            drivers: SigDrivers::None,
            args_fallback: ArgsFallback::None,
        };
        static SIG_SUB_DEC: SigDesc = SigDesc {
            name: "builtinSubDateDatetimeDecimalSig",
            pb_code: tipb::ScalarFuncSig::SubDateDatetimeDecimal as i32,
            vectorized: false,
            drivers: SigDrivers::None,
            args_fallback: ArgsFallback::None,
        };
        let dt_ft = new_field_type(tidb_base::mysql::TypeDatetime);
        let sf_str = ScalarFunction::new(
            tidb_parser::ast::funcs::DateSub,
            dt_ft.clone(),
            BuiltinFunc::new(&SIG_SUB_STR, vec![], dt_ft.clone(), "binary", "binary"),
        );
        let sf_dec = ScalarFunction::new(
            tidb_parser::ast::funcs::DateSub,
            dt_ft.clone(),
            BuiltinFunc::new(&SIG_SUB_DEC, vec![], dt_ft, "binary", "binary"),
        );
        let ectx = expr_ctx.get_eval_ctx();
        assert!(!tidb_expr_pushdown::scalar_expr_supported_by_flash(
            &*ectx, &sf_str
        ));
        assert!(!tidb_expr_pushdown::scalar_expr_supported_by_flash(
            &*ectx, &sf_dec
        ));

        // Verify full plan building reaches DateSub (blocked-by:D08.7 until time funcs land).
        let mut dt_col_ft = FieldType::default();
        dt_col_ft.set_type(tidb_base::mysql::TypeDatetime);
        dt_col_ft.set_charset("binary");
        dt_col_ft.set_collate("binary");
        let mut tbl = (*make_test_table_t()).clone();
        tbl.columns.as_mut().unwrap().push(Arc::new(ColumnInfo {
            id: 3,
            name: CIStr::new("d"),
            offset: 2,
            field_type: dt_col_ft,
            state: tidb_model::StatePublic,
            ..ColumnInfo::default()
        }));
        let is =
            crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(Arc::new(tbl))) as _);
        let sql = "SELECT SUM(a) OVER (ORDER BY d RANGE BETWEEN INTERVAL 1.0+0.5 DAY PRECEDING AND CURRENT ROW) FROM t";
        let (stmt, rctx) = parse_and_preprocess_t(sql, &is);
        let err = build_stmt_once(&stmt, sql, &rctx, &is, 0, vec![], false).unwrap_err();
        assert_eq!(tidb_base::blocked::blocked_by(&err), Some("D08.7"));
    }

    /// Emulation of Go's shared `*FieldType` mutation (`M-FT`, #402): `first_value` / `last_value` /
    /// `nth_value` strip `NotNullFlag` from the source column in `LogicalWindow.schema` and
    /// descendants down to `DataSource` (without mutating `TableInfo`), and that persists across
    /// `ColumnPruner` and `ProjectionEliminator`. Other window functions (e.g. `max`, `lead`) do not.
    #[test]
    fn window_first_last_nth_value_clears_schema_source_col_not_null() {
        use tidb_sessionctx::exprstatic as es;

        let make_nn_table = || {
            let mut ft = FieldType::default();
            ft.set_type(tidb_base::mysql::TypeLong);
            ft.set_flen(11);
            ft.add_flag(NotNullFlag);
            ft.set_charset("binary");
            ft.set_collate("binary");
            Arc::new(tidb_model::TableInfo {
                id: 100,
                name: CIStr::new("t"),
                columns: Some(vec![
                    Arc::new(ColumnInfo {
                        id: 1,
                        name: CIStr::new("a"),
                        offset: 0,
                        field_type: ft.clone(),
                        state: tidb_model::StatePublic,
                        ..ColumnInfo::default()
                    }),
                    Arc::new(ColumnInfo {
                        id: 2,
                        name: CIStr::new("b"),
                        offset: 1,
                        field_type: ft,
                        state: tidb_model::StatePublic,
                        ..ColumnInfo::default()
                    }),
                ]),
                state: tidb_model::StatePublic,
                ..tidb_model::TableInfo::default()
            })
        };

        let check = |sql: &str, expect_a_not_null: bool| {
            let tbl = make_nn_table();
            let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(Arc::clone(
                &tbl,
            ))) as _);
            let (stmt, rctx) = parse_and_preprocess_t(sql, &is);
            let mut vars = tidb_sessionctx::variable::session::SessionVars::new(Box::new(TestAcc));
            vars.current_db = "test".to_owned();
            let mut builder = new_plan_builder(&[]);
            builder.init(&mut vars, Some(is), None);
            builder.sql_text = sql;
            builder.resolve_ctx = Some(rctx);
            let ev = es::new_eval_context(vec![
                es::with_type_flags(tidb_types::context::DefaultStmtFlags),
                es::with_location(Some(tidb_base::gostd::time::utc())),
                es::with_current_db("test".to_owned()),
            ]);
            let expr_ctx = es::new_expr_context(vec![
                es::with_eval_ctx(ev),
                es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
                es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
            ]);
            let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
            ctx.has_dirty_content = Some(Arc::new(|_| false));
            let root = builder.build(&mut ctx, &stmt).unwrap();

            // TableInfo must never be mutated.
            assert!(has_not_null_flag(
                tbl.columns.as_ref().unwrap()[0].field_type.get_flag()
            ));

            // Check DataSource and LogicalWindow schemas before optimization.
            let mut stack = vec![root];
            let mut saw_win = false;
            let mut saw_ds = false;
            while let Some(id) = stack.pop() {
                let node = ctx.arena.get(id);
                stack.extend(node.children().iter().copied());
                if matches!(
                    node.kind(),
                    crate::LogicalKind::Window | crate::LogicalKind::DataSource
                ) {
                    if node.kind() == crate::LogicalKind::Window {
                        saw_win = true;
                    } else {
                        saw_ds = true;
                    }
                    let a_col = node
                        .schema(&ctx.arena)
                        .columns
                        .iter()
                        .find(|c| c.orig_name.ends_with(".a"))
                        .expect("column a in schema");
                    assert_eq!(
                        has_not_null_flag(a_col.ret_type.get_flag()),
                        expect_a_not_null,
                        "before opt ({:?}) for {sql}",
                        node.kind()
                    );
                }
            }
            assert!(saw_win && saw_ds);

            // Check after ColumnPruner + ProjectionEliminator.
            use crate::base::LogicalOptRule;
            let opt = crate::util::optimizetrace::default_logical_optimize_option();
            let used = ctx.arena.get(root).schema(&ctx.arena).columns.clone();
            let pruned = crate::base::prune_columns(&mut ctx, root, used, &opt).unwrap();
            let (elim, _) = crate::core::rule_eliminate_projection::ProjectionEliminator
                .optimize(&mut ctx, pruned, &opt)
                .unwrap();
            let a_col = ctx
                .arena
                .get(elim)
                .schema(&ctx.arena)
                .columns
                .iter()
                .find(|c| c.orig_name.ends_with(".a"))
                .expect("column a in final schema");
            assert_eq!(
                has_not_null_flag(a_col.ret_type.get_flag()),
                expect_a_not_null,
                "after prune+elim for {sql}"
            );
        };

        check("SELECT a, FIRST_VALUE(a) OVER (ORDER BY b) FROM t", false);
        check("SELECT a, LAST_VALUE(a) OVER (ORDER BY b) FROM t", false);
        check("SELECT a, NTH_VALUE(a, 1) OVER (ORDER BY b) FROM t", false);
        check("SELECT a, MAX(a) OVER (ORDER BY b) FROM t", true);
        check("SELECT a, LEAD(a, 1) OVER (ORDER BY b) FROM t", true);

        // Cross-group ordering (#402): LEAD's group is built before FIRST_VALUE's group,
        // so stripping NotNullFlag during check_window_func_args makes the LEAD output nullable.
        let tbl = make_nn_table();
        let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(tbl)) as _);
        let sql = "SELECT LEAD(a, 1, 0) OVER (ORDER BY a), FIRST_VALUE(a) OVER (ORDER BY b) FROM t";
        let (stmt, rctx) = parse_and_preprocess_t(sql, &is);
        let mut vars = tidb_sessionctx::variable::session::SessionVars::new(Box::new(TestAcc));
        vars.current_db = "test".to_owned();
        let mut builder = new_plan_builder(&[]);
        builder.init(&mut vars, Some(is), None);
        builder.sql_text = sql;
        builder.resolve_ctx = Some(rctx);
        let ev = es::new_eval_context(vec![
            es::with_type_flags(tidb_types::context::DefaultStmtFlags),
            es::with_location(Some(tidb_base::gostd::time::utc())),
            es::with_current_db("test".to_owned()),
        ]);
        let expr_ctx = es::new_expr_context(vec![
            es::with_eval_ctx(ev),
            es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
            es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
        ]);
        let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
        ctx.has_dirty_content = Some(Arc::new(|_| false));
        let root = builder.build(&mut ctx, &stmt).unwrap();
        let out_cols = &ctx.arena.get(root).schema(&ctx.arena).columns;
        assert_eq!(out_cols.len(), 2);
        assert!(
            !has_not_null_flag(out_cols[0].ret_type.get_flag()),
            "LEAD output column must be nullable because FIRST_VALUE cleared NotNullFlag in check_window_func_args"
        );
        assert!(
            !has_not_null_flag(out_cols[1].ret_type.get_flag()),
            "FIRST_VALUE output column must be nullable"
        );
    }

    #[test]
    fn prepared_repeated_execute_named_window_idempotent() {
        let is = crate::core::preprocess::InfoSchemaHandle(Arc::new(SingleTableIS(
            make_test_table_t(),
        )) as _);
        for sql in [
            "SELECT SUM(a) OVER (w ORDER BY b) FROM t WINDOW w AS (PARTITION BY a)",
            "SELECT ROW_NUMBER() OVER w, SUM(a) OVER (w1 ROWS 1 PRECEDING) FROM t WINDOW w AS (PARTITION BY a ORDER BY b), w1 AS (w)",
        ] {
            let (stmt, rctx) = parse_and_preprocess_t(sql, &is);
            let run = || {
                let mut vars =
                    tidb_sessionctx::variable::session::SessionVars::new(Box::new(TestAcc));
                vars.current_db = "test".to_owned();
                vars.sql_mode = tidb_base::mysql::ModeOnlyFullGroupBy.0;
                vars.state
                    .plan_id
                    .0
                    .store(0, std::sync::atomic::Ordering::SeqCst);
                vars.state
                    .plan_column_id
                    .0
                    .store(0, std::sync::atomic::Ordering::SeqCst);
                vars.stmt_ctx_mut().in_prepared_plan_building = true;
                let mut builder = new_plan_builder(&[]);
                builder.init(&mut vars, Some(is.clone()), None);
                builder.sql_text = sql;
                builder.resolve_ctx = Some(Arc::clone(&rctx));
                let ev = tidb_sessionctx::exprstatic::new_eval_context(vec![
                    tidb_sessionctx::exprstatic::with_type_flags(
                        tidb_types::context::DefaultStmtFlags,
                    ),
                    tidb_sessionctx::exprstatic::with_sql_mode(SQLMode(
                        tidb_base::mysql::ModeOnlyFullGroupBy.0,
                    )),
                    tidb_sessionctx::exprstatic::with_location(Some(tidb_base::gostd::time::utc())),
                    tidb_sessionctx::exprstatic::with_current_db("test".to_owned()),
                ]);
                let expr_ctx = tidb_sessionctx::exprstatic::new_expr_context(vec![
                    tidb_sessionctx::exprstatic::with_eval_ctx(ev),
                    tidb_sessionctx::exprstatic::with_charset(
                        "utf8mb4".to_owned(),
                        "utf8mb4_bin".to_owned(),
                    ),
                    tidb_sessionctx::exprstatic::with_default_collation_for_utf8mb4(
                        "utf8mb4_bin".to_owned(),
                    ),
                ]);
                let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
                ctx.has_dirty_content = Some(Arc::new(|_| false));
                let root = builder.build(&mut ctx, &stmt).unwrap();
                let mut summary = Vec::new();
                let mut stack = vec![root];
                while let Some(id) = stack.pop() {
                    let node = ctx.arena.get(id);
                    summary.push((
                        node.tp().to_owned(),
                        node.id(),
                        node.schema(&ctx.arena).len(),
                    ));
                    stack.extend(node.children().iter().copied());
                }
                summary
            };
            assert_eq!(run(), run(), "repeated EXECUTE must be identical for {sql}");
        }
    }
}
