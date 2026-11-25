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
//! - Joins (D09.3), subqueries (D09.6), aggregation, windows and GROUP BY (D09.4), CTEs and
//!   set operations (D09.5), views, virtual generated columns, locks, hints (D13.3),
//!   partitioned tables (D09.12) and the other branches the slice does not need are boundary
//!   stubs at the place Go takes them (spec §5).

#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::{blocked, blocked_by};
use tidb_base::dbterror::plannererrors::{
    ErrAggregateOrderNonAggQuery, ErrAmbiguous, ErrBadTable, ErrDupFieldName, ErrInvalidWildCard,
    ErrPartitionClauseOnNonpartitioned, ErrUnknownColumn, ErrUnsupportedType,
    ErrWindowInvalidWindowFuncAliasUse, ErrWindowInvalidWindowFuncUse, ErrWrongArguments,
};
use tidb_base::go_args;
use tidb_base::gostd::is_one_of;
use tidb_base::mysql::{
    ExtendedPriv, MaxRealWidth, RangeGraph, SQLMode, TypeDouble, has_pri_key_flag,
};
use tidb_chunk::Row;
use tidb_expression::builtin_cast::try_push_cast_into_control_function_for_hybrid_type;
use tidb_expression::constant::new_zero;
use tidb_expression::expression::{EvalState, split_cnf_items};
use tidb_expression::simple_rewriter::{
    column_name_orig_col_name, column_name_string, find_field_name,
};
use tidb_expression::util::{
    const_expr_consider_plan_cache, get_string_from_constant, param_marker_expression,
};
use tidb_expression::{
    Column, Expr as PExpr, ExprRef, Schema, column2_exprs, eval_bool, gc_column_expr_is_tidb_shard,
    new_schema,
};
use tidb_infoschema::metadef::is_mem_db;
use tidb_infoschema::tables::find_primary_index;
use tidb_model::{
    ColumnInfo, ExtraHandleID, ExtraHandleName, ExtraPhysTblID, StatePublic,
    TableCacheStatusEnable, TempTableLocal, new_extra_handle_col_info,
};
use tidb_parser::ast::{self, Expr};
use tidb_parser::model::CIStr;
use tidb_parser::types::ETString;
use tidb_parser::types::UnspecifiedLength;
use tidb_sessionctx::exprctx::BuildContext;
use tidb_types::convert::str_to_uint;
use tidb_types::etc::FieldType;
use tidb_types::field_name::{FieldName, NameSlice};
use tidb_types::field_type::set_bin_chs_cln_flag;

use crate::arena::{PlanArena, PlanId};
use crate::base::{LogicalKind, LogicalPlan};
use crate::core::expression_rewriter::{
    AggMapper, ByAddr, CheckSelectColumnPrivOption, ClauseCode, RewritePlan, WindowMapper,
    clause_msg, eval_ast_expr,
};
use crate::core::planbuilder::{
    PlanBuilder, VisitInfo, filter_path_by_isolation_read, get_possible_access_paths,
    has_window_flag, is_for_update_read_select_lock, split_where,
};
use crate::core::preprocess::try_lock_mdl_and_update_schema_if_necessary;
use crate::core::rule::logical_rules::{
    FlagConstantPropagation, FlagDeriveTopNFromWindow, FlagEliminateProjection, FlagGcSubstitute,
    FlagPredicatePushDown, FlagPredicateSimplification, FlagPushDownTopN,
};
use crate::core::util::table_has_dirty_content;
use crate::hint::parse_plan_hints;
use crate::logicalop::{
    DataSource, LogicalLimit, LogicalProjection, LogicalSelection, LogicalSort, LogicalTableDual,
};
use crate::planctx::PlanCtx;
use crate::util::column::index_info2_full_cols;
use crate::util::handle_cols::{new_common_handle_cols, new_int_handle_cols};
use crate::util::{ByItems, HandleCols};

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

    /// buildJoin: only the one-sided join of a single table source (`joinNode.Right ==
    /// nil`); a join of two sources is `blocked-by:D09.3`.
    // go: PlanBuilder.buildJoin L2 (a join of two sources is blocked-by:D09.3)
    pub fn build_join(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        join_node: &'ast ast::Join,
    ) -> Result<PlanId, Error> {
        // We will construct a "Join" node for some statements like "INSERT",
        // "DELETE", "UPDATE", "REPLACE". For this scenario "joinNode.Right" is nil
        // and we only build the left "ResultSetNode".
        if join_node.right.is_none() {
            let Some(left) = &join_node.left else {
                // glue: blocked-by:D02.2 (a join without a left side from the Rust parser)
                return Err(blocked("D02.2", "a join without a left side"));
            };
            return self.build_result_set_node(ctx, ResultSetRef::Node(left), false);
        }
        // glue: blocked-by:D09.3 (joins of two sources: the join builder and LogicalJoin)
        Err(blocked("D09.3", "a join of two table sources (buildJoin)"))
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
    /// A column of the redundant side of a NATURAL / USING join (`findColFromNaturalUsingJoin`)
    /// is `blocked-by:D09.3`; correlated columns exist only in subqueries (D09.6).
    // go: PlanBuilder.buildProjectionField L2 (the natural-join columns are blocked-by:D09.3; correlated columns are subqueries')
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
        let (col_name, orig_col_name, tbl_name, orig_tbl_name, db_name) = if let (
            Expr::Column(col_name_field),
            Some(col),
        ) = (inner_node, col)
        {
            // Field is a column reference.
            let idx = schema_of(&ctx.arena, p).column_index(col);
            // The column maybe the one from join's redundant part.
            if idx == -1 {
                // name = findColFromNaturalUsingJoin(p, col)
                // glue: blocked-by:D09.3 (findColFromNaturalUsingJoin: the redundant columns of NATURAL / USING joins)
                return Err(blocked(
                    "D09.3",
                    "a column of the redundant side of a NATURAL / USING join (findColFromNaturalUsingJoin)",
                ));
            }
            let name = Arc::clone(&names_of(&ctx.arena, p)[idx as usize]);
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
        fields: &[SelectFieldW<'_>],
        mapper: Option<&AggMapper<'_>>,
        window_mapper: Option<&mut WindowMapper<'_>>,
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
        let window_mapper_ro = window_mapper.as_deref();
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
                window_mapper_ro,
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
            // (Windows are blocked-by:D09.4 before the projection that considers them.)

            // p = np: outside subqueries the plan never changes (D09.2 spec §3 item 2).
            proj.exprs.push(Arc::clone(&new_expr));

            let (col, name) = self.build_projection_field(ctx, p, field, Some(&new_expr))?;
            schema.append(col);
            new_names.push(Arc::new(name));
        }
        let _ = window_mapper;
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
            if has_distinct && proj_exprs.is_some() {
                check_order_by_in_distinct(i, old_len)?;
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

/// checkOrderByInDistinct runs only for SELECT DISTINCT, an aggregation (D09.4).
// glue: stub of PlanBuilder.checkOrderByInDistinct, blocked-by:D09.4 (SELECT DISTINCT is an aggregation)
fn check_order_by_in_distinct(_idx: usize, _length: usize) -> Result<(), Error> {
    Err(blocked("D09.4", "checkOrderByInDistinct (SELECT DISTINCT)"))
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
    gby_items: &'ast [ast::ByItem],
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

    // go: havingWindowAndOrderbyExprResolver.resolveFromPlan L2 (joins and applies are blocked-by:D09.3)
    fn resolve_from_plan(
        &mut self,
        v: &ast::ColumnName,
        p: PlanId,
        resolve_fields_first: bool,
    ) -> Result<isize, Error> {
        let arena = self.arena;
        let idx = find_field_name(names_of(arena, p), v)?;
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
                // LogicalMaxOneRow, LogicalJoin and LogicalApply are not built yet (subqueries
                // D09.6, joins D09.3).
                _ => {}
            }
            // nowhere to be found.
            return Ok(-1);
        }
        let idx = idx as usize;
        let col = &schema_of(arena, p).columns[idx];
        if col.is_hidden {
            return Err(ErrUnknownColumn.gen_with_stack_by_args(go_args![
                column_name_string(v),
                clause_msg(self.cur_clause)
            ]));
        }
        let name = &names_of(arena, p)[idx];
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
    /// Inside an aggregate, Go replaces the column name by the select field's expression it
    /// resolves to (`select a+1 as b from t having sum(b) < 0` aggregates `a+1`). Only
    /// `buildAggregation` reads the replaced arguments, and aggregation is `blocked-by:D09.4`,
    /// so the replacement is not made (D09.1 spec §3 item 3).
    // go: havingWindowAndOrderbyExprResolver.Leave L2 (the replacement inside aggregates is read only by the aggregation, blocked-by:D09.4)
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
                if let Expr::Column(col) = &item.expr
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
            // return a.selectFields[index].Expr, true: see `leave`.
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
        if self.enter(node) {
            return;
        }
        for c in expr_children(node) {
            self.accept(c);
        }
        // Leave does nothing.
    }

    /// Returns whether to skip the children.
    // go: colResolverForOnlyFullGroupBy.Enter L1
    fn enter(&mut self, node: &Expr) -> bool {
        match node {
            Expr::Aggregate(_) => {
                self.has_agg_func_or_any_value = true;
                if self.cur_clause == ClauseCode::OrderByClause {
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
}

impl<'ast> PlanBuilder<'ast> {
    /// resolveHavingAndOrderBy will process aggregate functions and resolve the columns that don't exist in select fields.
    /// If we found some columns that are not in select fields, we will append it to select fields and update the colMapper.
    /// When we rewrite the order by / having expression, we will find column in map at first.
    ///
    /// Returns the aggregate mappers of HAVING and ORDER BY and the select fields with the
    /// auxiliary fields appended (Go appends to `sel.Fields.Fields`).
    // go: PlanBuilder.resolveHavingAndOrderBy L2 (an ORDER BY subquery is blocked-by:D09.6)
    #[allow(clippy::type_complexity)]
    pub fn resolve_having_and_order_by(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
        p: PlanId,
        fields: Vec<SelectFieldW<'ast>>,
    ) -> Result<(AggMapper<'ast>, AggMapper<'ast>, Vec<SelectFieldW<'ast>>), Error> {
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
            gby_items: &[],
            cur_clause: ClauseCode::UnknowClause,
            prev_clause: Vec::new(),
        };
        if let Some(g) = &sel.group_by {
            extractor.gby_items = &g.items;
        }
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
            self.check_col_priv = origin_col_priv;
        }
        Ok((having_agg_mapper, order_agg_mapper, fields))
    }

    /// checkOnlyFullGroupBy: with GROUP BY the builder stopped before (`blocked-by:D09.4`).
    // go: PlanBuilder.checkOnlyFullGroupBy L2 (with GROUP BY: blocked-by:D09.4)
    fn check_only_full_group_by(
        &self,
        sel: &ast::SelectStmt,
        fields: &[SelectFieldW<'_>],
    ) -> Result<(), Error> {
        if sel.group_by.is_some() {
            // glue: blocked-by:D09.4 (checkOnlyFullGroupByWithGroupClause)
            return Err(blocked(
                "D09.4",
                "the ONLY_FULL_GROUP_BY check with GROUP BY",
            ));
        }
        self.check_only_full_group_by_with_out_group_clause(sel, fields)
    }

    /// checkOnlyFullGroupByWithOutGroupClause. A query block without aggregates (or
    /// `ANY_VALUE`) passes after the walk; one that mixes them with plain columns needs the
    /// functional dependencies of WHERE and the joins (`buildWhereFuncDepend`,
    /// `buildJoinFuncDepend`, `checkColFuncDepend`), which are `blocked-by:D09.4` (the
    /// aggregation stops at the same boundary later).
    // go: PlanBuilder.checkOnlyFullGroupByWithOutGroupClause L2 (the functional dependencies with aggregates are blocked-by:D09.4)
    fn check_only_full_group_by_with_out_group_clause(
        &self,
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
        // glue: blocked-by:D09.4 (buildWhereFuncDepend / buildJoinFuncDepend / checkColFuncDepend)
        Err(blocked(
            "D09.4",
            "the ONLY_FULL_GROUP_BY check of aggregates mixed with columns (functional dependencies)",
        ))
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

    // go: PlanBuilder.unfoldWildStar L2 (the FullSchema of joins: joins are blocked-by:D09.3)
    fn unfold_wild_star(
        &self,
        arena: &PlanArena,
        p: PlanId,
        select_fields: Vec<SelectFieldW<'ast>>,
    ) -> Result<Vec<SelectFieldW<'ast>>, Error> {
        // Extract FullSchema/FullNames from LogicalJoin or LogicalApply (which embeds LogicalJoin):
        // no join is built yet (D09.3), so there is none.
        let mut result_list = Vec::with_capacity(select_fields.len().max(2));
        for (i, field) in select_fields.into_iter().enumerate() {
            let Some(wild_card) = field.wild_card else {
                result_list.push(field);
                continue;
            };
            if wild_card.table.l().is_empty() && i > 0 {
                return Err(ErrInvalidWildCard.clone());
            }
            let list =
                unfold_wild_star(wild_card, names_of(arena, p), &schema_of(arena, p).columns);
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

/// The ONLY_FULL_GROUP_BY helpers of GROUP BY and of aggregates without GROUP BY.
// glue: stub of extractSingeValueColNamesFromWhere, blocked-by:D09.4 (the ONLY_FULL_GROUP_BY check with aggregates)
pub fn extract_singe_value_col_names_from_where() -> Result<(), Error> {
    Err(blocked(
        "D09.4",
        "extractSingeValueColNamesFromWhere (ONLY_FULL_GROUP_BY with aggregates)",
    ))
}

// glue: stub of allColFromAggExprNode, blocked-by:D09.4 (correlated aggregates and ONLY_FULL_GROUP_BY with aggregates)
pub fn all_col_from_agg_expr_node() -> Result<(), Error> {
    Err(blocked("D09.4", "allColFromAggExprNode (aggregates)"))
}

// glue: stub of colNameResolver.Enter, blocked-by:D09.4 (allColFromExprNode, ONLY_FULL_GROUP_BY with GROUP BY)
// glue: stub of colNameResolver.Leave, blocked-by:D09.4 (allColFromExprNode, ONLY_FULL_GROUP_BY with GROUP BY)
// glue: stub of allColFromExprNode, blocked-by:D09.4 (the ONLY_FULL_GROUP_BY check with GROUP BY)
pub fn all_col_from_expr_node() -> Result<(), Error> {
    Err(blocked("D09.4", "allColFromExprNode (GROUP BY)"))
}

/// The window grouping helpers of `groupWindowFuncs`.
// glue: stub of getAllByItems, blocked-by:D09.4 (window functions)
// glue: stub of restoreByItemText, blocked-by:D09.4 (window functions)
// glue: stub of compareItems, blocked-by:D09.4 (window functions)
pub fn compare_items() -> Result<(), Error> {
    Err(blocked("D09.4", "compareItems (window functions)"))
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
    // go: PlanBuilder.buildSelect L2 (the branches of hints, CTEs, GROUP BY, windows, aggregation, DISTINCT, subqueries and locks are boundary stubs)
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

        let p = self.build_table_refs(ctx, sel.from.as_deref())?;

        let fields: Vec<SelectFieldW<'ast>> = sel
            .fields
            .fields
            .iter()
            .map(|f| SelectFieldW::of_ast(f, self.sql_text))
            .collect();
        let fields = self.unfold_wild_star(&ctx.arena, p, fields)?;
        // b.capFlag&canExpandAST: set only by CREATE VIEW (addAliasName, D09.15).

        if sel.group_by.is_some() {
            // gbyExprs, err = b.resolveGbyExprs(p, sel.GroupBy, sel.Fields.Fields)
            // glue: blocked-by:D09.4 (GROUP BY: resolveGbyExprs / rewriteGbyExprs)
            return Err(blocked("D09.4", "GROUP BY (resolveGbyExprs)"));
        }

        // `checkOnlyFullGroupBy` should be executed before rewrite gbyExprs, because the field type of the fields
        // may change. For example, the length of a string field may change in `adjustRetFtForCastString`
        if SQLMode(ctx.vars.sql_mode).has_only_full_group_by()
            && sel.from.is_some()
            && !ctx.vars.optimizer_enable_new_only_full_group_by_check
        {
            self.check_only_full_group_by(sel, &fields)?;
        }

        let has_window_func_field = self.detect_select_window(sel);
        // Some SQL statements define WINDOW but do not use them. But we also need to check the window specification list.
        // For example: select id from t group by id WINDOW w AS (ORDER BY uids DESC) ORDER BY id;
        // We don't use the WINDOW w, but if the 'uids' column is not in the table t, we still need to report an error.
        if has_window_func_field || !sel.window_specs.is_empty() {
            // windowAggMap, err = b.resolveWindowFunction(sel, p)
            // glue: blocked-by:D09.4 (window functions: resolveWindowFunction)
            return Err(blocked("D09.4", "window functions (resolveWindowFunction)"));
        }
        // We must resolve having and order by clause before build projection,
        // because when the query is "select a+1 as b from t having sum(b) < 0", we must replace sum(b) to sum(a+1),
        // which only can be done before building projection and extracting Agg functions.
        let (having_map, order_map, fields) =
            self.resolve_having_and_order_by(ctx, sel, p, fields)?;

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
        let res = self.build_select_rest(ctx, sel, p, &fields, &having_map, &order_map);
        self.all_names.pop();
        res
    }

    // go-part: PlanBuilder.buildSelect
    fn build_select_rest(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        sel: &'ast ast::SelectStmt,
        mut p: PlanId,
        fields: &[SelectFieldW<'ast>],
        having_map: &AggMapper<'ast>,
        order_map: &AggMapper<'ast>,
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

        let has_agg = self.detect_select_agg(sel);
        if has_agg {
            // aggFuncs, totalMap = b.extractAggFuncsInSelectFields(sel.Fields.Fields), then
            // b.buildAggregation (a correlated aggregate of an outer query exists only in
            // subqueries, which stopped before).
            // glue: blocked-by:D09.4 (aggregation: buildAggregation, LogicalAggregation)
            return Err(blocked("D09.4", "aggregation (buildAggregation)"));
        }

        // According to https://dev.mysql.com/doc/refman/8.0/en/window-functions-usage.html,
        // we can only process window functions after having clause, so `considerWindow` is false now.
        let (np, proj_exprs, old_len) =
            self.build_projection(ctx, p, fields, None, None, false, sel.order_by.is_some())?;
        p = np;

        if let Some(h) = &sel.having {
            self.cur_clause = ClauseCode::HavingClause;
            p = self.build_selection(ctx, p, &h.expr, Some(having_map))?;
        }

        // b.windowSpecs, err = buildWindowSpecs(sel.WindowSpecs): no window specification
        // here (windows stopped before), so the map is empty and the window branch is not
        // taken.

        if sel.distinct {
            // glue: blocked-by:D09.4 (SELECT DISTINCT: buildDistinct builds a LogicalAggregation)
            return Err(blocked("D09.4", "SELECT DISTINCT (buildDistinct)"));
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
                        None,
                        Some(&proj_exprs),
                        old_len,
                        sel.distinct,
                    )?;
                } else {
                    p = self.build_sort(ctx, p, &o.items, Some(order_map), None)?;
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
            // glue: blocked-by:D09.1 (views: BuildDataSourceFromView, part 2 of this block)
            return Err(blocked("D09.1", "a view (BuildDataSourceFromView)"));
        }

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

    #[test]
    fn handle_col_helper_stack() {
        let mut h = HandleColHelper::default();
        h.push_map(None);
        h.push_map(Some(HashMap::new()));
        assert!(h.pop_map().is_some());
        assert!(h.pop_map().is_none());
        assert_eq!((h.stack_tail, h.id2_handle_map_stack.len()), (0, 0));
    }
}
