//! Column pruning on hand-built logical plans, against Go's golden outputs.
//!
//! Expected values: `pkg/planner/core/testdata/plan_suite_unexported_out.json`
//! (`TestColumnPruning`, `TestSortByItemsPruning`): the columns of each DataSource / Limit
//! after `FlagPredicatePushDown|FlagPruneColumns|FlagPruneColumnsAgain`, keyed by Go plan id.
//! Plans are built as Go's `PlanBuilder` builds them (operators and plan-id order); where
//! Go's predicate push-down would have moved a filter into `DataSource.AllConds` the test
//! builds that state directly.

mod common;

use std::sync::Arc;

use common::*;
use tidb_expression::{Expr, merge_schema};
use tidb_parser::ast::funcs as ast;
use tidb_planner_pilot::logicalop::{
    JoinType, LogicalJoin, LogicalLimit, LogicalSelection, LogicalSort, LogicalTableDual,
};
use tidb_planner_pilot::optimizetrace::LogicalOptimizeOp;
use tidb_planner_pilot::rule_column_pruning::ColumnPruner;
use tidb_planner_pilot::util::ByItems;
use tidb_planner_pilot::{LogicalKind, LogicalOptRule, LogicalPlan, PlanCtx, PlanId};

fn prune(ctx: &mut PlanCtx<'_>, root: PlanId) -> PlanId {
    let mut opt = LogicalOptimizeOp::tracing();
    let (root, _) = ColumnPruner.optimize(ctx, root, &mut opt).unwrap();
    // FlagPruneColumnsAgain: the rule runs a second time at the end of the rule list.
    let (root, _) = ColumnPruner.optimize(ctx, root, &mut opt).unwrap();
    root
}

fn ds_cols(ctx: &PlanCtx<'_>, ds: PlanId) -> Vec<String> {
    assert_eq!(ctx.arena.get(ds).kind(), LogicalKind::DataSource);
    col_strings(&schema_of(ctx, ds))
}

fn join(ctx: &mut PlanCtx<'_>, l: PlanId, r: PlanId, jt: JoinType) -> PlanId {
    let mut j = LogicalJoin::init(jt, ctx, 1);
    j.set_children(vec![l, r]);
    let schema = merge_schema(Some(&schema_of(ctx, l)), Some(&schema_of(ctx, r))).unwrap();
    j.sp.set_schema(Some(schema));
    ctx.arena.alloc(Box::new(j))
}

/// `select count(*) from t group by a` → {1: [test.t.a]}
#[test]
fn count_group_by_a() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let a = schema_of(&ctx, ds).columns[0].clone();
    let (agg, out) = aggregation(
        &mut ctx,
        ds,
        vec![(ast::AggFuncCount, vec![one()])],
        vec![col_expr(&a)],
    );
    let c = new_col(&ctx, (*out[0].ret_type).clone());
    let proj = projection(&mut ctx, agg, vec![(col_expr(&out[0]), c)]);
    let root = prune(&mut ctx, proj);
    assert_eq!(ctx.arena.get(ds).id(), 1);
    assert_eq!(ds_cols(&ctx, ds), ["test.t.a"]);
    assert_eq!(root, proj);
    // Only count(1) is left in the aggregation (the first_row functions are pruned).
    let agg_node = ctx
        .arena
        .downcast_ref::<tidb_planner_pilot::logicalop::LogicalAggregation>(agg)
        .unwrap();
    assert_eq!(agg_node.agg_funcs.len(), 1);
    assert_eq!(agg_node.agg_funcs[0].name, ast::AggFuncCount);
}

/// `select count(*) from t` → {1: [test.t.a]} (no column used: the int handle is kept)
#[test]
fn count_star_keeps_handle() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let (agg, out) = aggregation(&mut ctx, ds, vec![(ast::AggFuncCount, vec![one()])], vec![]);
    let c = new_col(&ctx, (*out[0].ret_type).clone());
    let proj = projection(&mut ctx, agg, vec![(col_expr(&out[0]), c)]);
    prune(&mut ctx, proj);
    assert_eq!(ds_cols(&ctx, ds), ["test.t.a"]);
}

/// `select count(*) from t a join t b where a.a < 1` → {1: [test.t.a], 2: [test.t.a]}
/// (after predicate push-down `a.a < 1` is in the left DataSource's AllConds)
#[test]
fn join_with_pushed_down_filter() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let l = data_source(&mut ctx);
    let r = data_source(&mut ctx);
    let la = schema_of(&ctx, l).columns[0].clone();
    let cond = func(&ctx, ast::LT, vec![col_expr(&la), one()]);
    ctx.arena
        .downcast_mut::<tidb_planner_pilot::logicalop::DataSource>(l)
        .unwrap()
        .all_conds = vec![cond];
    let j = join(&mut ctx, l, r, JoinType::InnerJoin);
    let (agg, out) = aggregation(&mut ctx, j, vec![(ast::AggFuncCount, vec![one()])], vec![]);
    let c = new_col(&ctx, (*out[0].ret_type).clone());
    let proj = projection(&mut ctx, agg, vec![(col_expr(&out[0]), c)]);
    prune(&mut ctx, proj);
    assert_eq!((ctx.arena.get(l).id(), ctx.arena.get(r).id()), (1, 2));
    assert_eq!(ds_cols(&ctx, l), ["test.t.a"]);
    assert_eq!(ds_cols(&ctx, r), ["test.t.a"]);
}

/// `select count(*) from t a join t b on a.a = b.d` → {1: [test.t.a], 2: [test.t.d]}
#[test]
fn join_on_equal_condition() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let l = data_source(&mut ctx);
    let r = data_source(&mut ctx);
    let la = schema_of(&ctx, l).columns[0].clone();
    let rd = schema_of(&ctx, r).columns[3].clone();
    let j = join(&mut ctx, l, r, JoinType::InnerJoin);
    let eq = func(&ctx, ast::EQ, vec![col_expr(&la), col_expr(&rd)]);
    ctx.arena
        .downcast_mut::<LogicalJoin>(j)
        .unwrap()
        .equal_conditions = vec![eq];
    let (agg, out) = aggregation(&mut ctx, j, vec![(ast::AggFuncCount, vec![one()])], vec![]);
    let c = new_col(&ctx, (*out[0].ret_type).clone());
    let proj = projection(&mut ctx, agg, vec![(col_expr(&out[0]), c)]);
    prune(&mut ctx, proj);
    assert_eq!(ds_cols(&ctx, l), ["test.t.a"]);
    assert_eq!(ds_cols(&ctx, r), ["test.t.d"]);
    // The join's schema was rebuilt from the pruned children (MergeSchema) and then
    // inline-projected to what the aggregation uses (nothing: its smallest column).
    assert_eq!(schema_of(&ctx, j).len(), 1);
}

/// `select a as c1, b as c2 from t order by 1, c1 + c2 + c` → {1: [a, b, c]}
/// Builder: DataSource → Projection[a, b, c(auxiliary)] → Sort[c1, c1+c2+c] → Projection[c1, c2]
#[test]
fn sort_by_expression_over_projection() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let cols = schema_of(&ctx, ds).columns;
    let (a, b, c) = (cols[0].clone(), cols[1].clone(), cols[2].clone());
    // Column fields keep the child's *Column as output column (buildProjectionField).
    let p1 = projection(
        &mut ctx,
        ds,
        vec![
            (col_expr(&a), a.clone()),
            (col_expr(&b), b.clone()),
            (col_expr(&c), c.clone()),
        ],
    );
    let sum = func(&ctx, ast::Plus, vec![col_expr(&a), col_expr(&b)]);
    let sum = func(&ctx, ast::Plus, vec![sum, col_expr(&c)]);
    let mut sort = LogicalSort::init(
        vec![
            ByItems {
                expr: col_expr(&a),
                desc: false,
            },
            ByItems {
                expr: sum,
                desc: false,
            },
        ],
        &ctx,
        1,
    );
    sort.set_children(vec![p1]);
    let sort = ctx.arena.alloc(Box::new(sort));
    let p2 = projection(
        &mut ctx,
        sort,
        vec![(col_expr(&a), a.clone()), (col_expr(&b), b.clone())],
    );
    prune(&mut ctx, p2);
    assert_eq!(ds_cols(&ctx, ds), ["test.t.a", "test.t.b", "test.t.c"]);
}

/// `select b from t where a > 0 limit 5, 10` → {1: [test.t.a, test.t.b], 4: [test.t.b]}
/// Builder: DataSource(1) → Selection(2) → Projection(3) → Limit(4)
#[test]
fn selection_projection_limit() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let cols = schema_of(&ctx, ds).columns;
    let (a, b) = (cols[0].clone(), cols[1].clone());
    let gt = func(&ctx, ast::GT, vec![col_expr(&a), one()]);
    let mut sel = LogicalSelection::init(vec![gt], &ctx, 1);
    sel.set_children(vec![ds]);
    let sel = ctx.arena.alloc(Box::new(sel));
    let proj = projection(&mut ctx, sel, vec![(col_expr(&b), b.clone())]);
    let mut limit = LogicalLimit::init(5, 10, &ctx, 1);
    limit.set_children(vec![proj]);
    let limit = ctx.arena.alloc(Box::new(limit));
    prune(&mut ctx, limit);
    assert_eq!(ctx.arena.get(limit).id(), 4);
    assert_eq!(ds_cols(&ctx, ds), ["test.t.a", "test.t.b"]);
    assert_eq!(col_strings(&schema_of(&ctx, limit)), ["test.t.b"]);
    // The Selection has no schema of its own: it is its child's, by reference.
    assert!(std::ptr::eq(
        ctx.arena.get(sel).schema(&ctx.arena),
        ctx.arena.get(ds).schema(&ctx.arena)
    ));
}

/// `select 1 from (select count(b) as cnt from t) t1` → {1: [test.t.a]}
/// The inner projection loses every column and is replaced by its child; count(b) is
/// pruned and count(1) added back so the row count survives.
#[test]
fn projection_with_all_columns_pruned_is_removed() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let b = schema_of(&ctx, ds).columns[1].clone();
    let (agg, out) = aggregation(
        &mut ctx,
        ds,
        vec![(ast::AggFuncCount, vec![col_expr(&b)])],
        vec![],
    );
    let cnt = new_col(&ctx, (*out[0].ret_type).clone());
    let inner = projection(&mut ctx, agg, vec![(col_expr(&out[0]), cnt)]);
    let c1 = new_col(&ctx, int_type(true));
    let outer = projection(&mut ctx, inner, vec![(one(), c1)]);
    let root = prune(&mut ctx, outer);
    assert_eq!(root, outer);
    assert_eq!(ds_cols(&ctx, ds), ["test.t.a"]);
    // The outer projection's child is now the aggregation: the inner one was dropped.
    assert_eq!(ctx.arena.get(outer).children(), [agg]);
    let agg_node = ctx
        .arena
        .downcast_ref::<tidb_planner_pilot::logicalop::LogicalAggregation>(agg)
        .unwrap();
    assert_eq!(agg_node.agg_funcs.len(), 1);
    assert_eq!(agg_node.agg_funcs[0].name, ast::AggFuncCount);
    assert!(matches!(*agg_node.agg_funcs[0].args[0], Expr::Constant(_)));
}

/// TestSortByItemsPruning: `select * from t where a > 1 order by a asc, b asc, a asc, c asc
/// limit 10` → by-items [test.t.a, test.t.b, test.t.c] (the duplicated `a` is dropped).
#[test]
fn sort_by_items_dedup() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let cols = schema_of(&ctx, ds).columns;
    let fields = cols.iter().map(|c| (col_expr(c), c.clone())).collect();
    let proj = projection(&mut ctx, ds, fields);
    let items = [0usize, 1, 0, 2]
        .iter()
        .map(|&i| ByItems {
            expr: col_expr(&cols[i]),
            desc: false,
        })
        .collect();
    let mut sort = LogicalSort::init(items, &ctx, 1);
    sort.set_children(vec![proj]);
    let sort = ctx.arena.alloc(Box::new(sort));
    let mut limit = LogicalLimit::init(0, 10, &ctx, 1);
    limit.set_children(vec![sort]);
    let limit = ctx.arena.alloc(Box::new(limit));
    prune(&mut ctx, limit);
    let s = ctx.arena.downcast_ref::<LogicalSort>(sort).unwrap();
    let got: Vec<String> = s
        .by_items
        .iter()
        .map(|b| {
            String::from_utf8_lossy(
                &b.expr
                    .string_with_ctx(&*ctx.eval_ctx(), tidb_base::errors::RedactLogDisable),
            )
            .into_owned()
        })
        .collect();
    assert_eq!(got, ["test.t.a", "test.t.b", "test.t.c"]);
}

/// Left outer join: the inner side's columns lose NOT NULL in the join schema
/// (`BuildLogicalJoinSchema` → `ResetNotNullFlag`, which copies the column first), and the
/// child's own column keeps it.
#[test]
fn left_outer_join_resets_not_null_by_copy() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let l = data_source(&mut ctx);
    let r = data_source(&mut ctx);
    let (la, ra) = (
        schema_of(&ctx, l).columns[0].clone(),
        schema_of(&ctx, r).columns[0].clone(),
    );
    let j = join(&mut ctx, l, r, JoinType::LeftOuterJoin);
    let eq = func(&ctx, ast::EQ, vec![col_expr(&la), col_expr(&ra)]);
    ctx.arena
        .downcast_mut::<LogicalJoin>(j)
        .unwrap()
        .equal_conditions = vec![eq];
    let proj = projection(
        &mut ctx,
        j,
        vec![(col_expr(&la), la.clone()), (col_expr(&ra), ra.clone())],
    );
    prune(&mut ctx, proj);
    let js = schema_of(&ctx, j);
    assert_eq!(col_strings(&js), ["test.t.a", "test.t.a"]);
    let not_null = |c: &Arc<tidb_expression::Column>| {
        c.ret_type.get_flag() & tidb_base::mysql::NotNullFlag != 0
    };
    assert!(not_null(&js.columns[0]));
    assert!(!not_null(&js.columns[1]));
    assert!(not_null(&schema_of(&ctx, r).columns[0]));
}

/// Projection over a dual whose columns are all unused: Go keeps the projection with one
/// constant 0 and a freshly allocated output column.
#[test]
fn projection_over_dual_keeps_one_constant() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let dual = LogicalTableDual::init(1, tidb_expression::new_schema(vec![]), &ctx, 1);
    let dual = ctx.arena.alloc(Box::new(dual));
    let x = new_col(&ctx, int_type(true));
    let inner = projection(&mut ctx, dual, vec![(one(), x.clone())]);
    let y = new_col(&ctx, int_type(true));
    let outer = projection(&mut ctx, inner, vec![(one(), y)]);
    let before = ctx.vars.alloc_plan_column_id();
    prune(&mut ctx, outer);
    let p = ctx
        .arena
        .downcast_ref::<tidb_planner_pilot::logicalop::LogicalProjection>(inner)
        .unwrap();
    assert_eq!(p.exprs.len(), 1);
    let s = p.schema(&ctx.arena);
    assert_eq!(s.len(), 1);
    assert!(s.columns[0].unique_id > before);
    assert_ne!(s.columns[0].unique_id, x.unique_id);
}

/// The trace stub records what was pruned (blocked-by:D09.13 for Go's format).
#[test]
fn trace_records_pruned_columns() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let b = schema_of(&ctx, ds).columns[1].clone();
    let proj = projection(&mut ctx, ds, vec![(col_expr(&b), b.clone())]);
    let mut opt = LogicalOptimizeOp::tracing();
    ColumnPruner.optimize(&mut ctx, proj, &mut opt).unwrap();
    let steps = opt.steps.unwrap();
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert!(
        steps[0].starts_with("DataSource_1: columns[test.t.e,test.t.d,test.t.c,test.t.a]"),
        "{steps:?}"
    );
}
