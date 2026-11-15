//! Logical projection elimination (`ProjectionEliminator`), and the shared-expression
//! mutations it makes (audit items E2, E3 of the pilot report).

mod common;

use std::sync::Arc;

use common::*;
use tidb_base::mysql::NotNullFlag;
use tidb_expression::{Expr, ExprRef};
use tidb_parser::ast::funcs as ast;
use tidb_planner_pilot::logicalop::{LogicalAggregation, LogicalProjection};
use tidb_planner_pilot::optimizetrace::LogicalOptimizeOp;
use tidb_planner_pilot::rule_column_pruning::ColumnPruner;
use tidb_planner_pilot::rule_eliminate_projection::ProjectionEliminator;
use tidb_planner_pilot::{LogicalKind, LogicalOptRule, PlanCtx, PlanId};

fn expr_string(ctx: &PlanCtx<'_>, e: &ExprRef) -> String {
    String::from_utf8_lossy(
        &e.string_with_ctx(&*ctx.eval_ctx(), tidb_base::errors::RedactLogDisable),
    )
    .into_owned()
}

/// Go's `ToString` for the plan shapes used here.
fn to_string(ctx: &PlanCtx<'_>, p: PlanId) -> String {
    let n = ctx.arena.get(p);
    let me = match n.kind() {
        LogicalKind::DataSource => "DataScan(t)".to_owned(),
        LogicalKind::Projection => "Projection".to_owned(),
        LogicalKind::Aggregation => "Aggr".to_owned(),
        k => format!("{k:?}"),
    };
    match n.children() {
        [] => me,
        [c] => format!("{}->{me}", to_string(ctx, *c)),
        _ => unreachable!(),
    }
}

/// TestProjectionEliminator: `select 1+num from (select 1+a as num from t) t1` →
/// "DataScan(t)->Projection" (the two projections are merged).
#[test]
fn merges_duplicate_projections() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let a = schema_of(&ctx, ds).columns[0].clone();
    let inner_expr = func(&ctx, ast::Plus, vec![one(), col_expr(&a)]);
    let num = new_col(&ctx, int_type(false));
    let inner = projection(&mut ctx, ds, vec![(inner_expr, num.clone())]);
    let outer_expr = func(&ctx, ast::Plus, vec![one(), col_expr(&num)]);
    let out = new_col(&ctx, int_type(false));
    let outer = projection(&mut ctx, inner, vec![(outer_expr, out)]);

    // FlagBuildKeyInfo|FlagPruneColumns|FlagPruneColumnsAgain|FlagEliminateProjection
    let mut opt = LogicalOptimizeOp::default();
    let (root, _) = ColumnPruner.optimize(&mut ctx, outer, &mut opt).unwrap();
    let (root, _) = ProjectionEliminator
        .optimize(&mut ctx, root, &mut opt)
        .unwrap();
    let (root, _) = ColumnPruner.optimize(&mut ctx, root, &mut opt).unwrap();

    assert_eq!(to_string(&ctx, root), "DataScan(t)->Projection");
    let p = ctx.arena.downcast_ref::<LogicalProjection>(root).unwrap();
    assert_eq!(expr_string(&ctx, &p.exprs[0]), "plus(1, plus(1, test.t.a))");
}

/// A column-only projection under an aggregation is removed, and the aggregation's
/// arguments are rewritten through the replace map (`ResolveExprAndReplace`, E2).
#[test]
fn eliminates_column_projection_under_aggregation() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let b = schema_of(&ctx, ds).columns[1].clone();
    // (select b as x from t): a projection whose output column is new.
    let x = new_col(&ctx, (*b.ret_type).clone());
    let proj = projection(&mut ctx, ds, vec![(col_expr(&b), x.clone())]);
    // count(x + 1)
    let arg = func(&ctx, ast::Plus, vec![col_expr(&x), one()]);
    let shared_arg = arg.clone();
    let (agg, _) = aggregation(&mut ctx, proj, vec![(ast::AggFuncCount, vec![arg])], vec![]);

    let mut opt = LogicalOptimizeOp::default();
    let (root, _) = ProjectionEliminator
        .optimize(&mut ctx, agg, &mut opt)
        .unwrap();
    assert_eq!(root, agg);
    assert_eq!(to_string(&ctx, root), "DataScan(t)->Aggr");
    let a = ctx.arena.downcast_ref::<LogicalAggregation>(agg).unwrap();
    assert_eq!(
        expr_string(&ctx, &a.agg_funcs[0].args[0]),
        "plus(test.t.b, 1)"
    );
    // E2: the function node was shared (the test kept a reference), so it was copied;
    // the other holder still sees the old argument. Go would have changed it in place.
    assert!(!Arc::ptr_eq(&a.agg_funcs[0].args[0], &shared_arg));
    assert_eq!(
        expr_string(&ctx, &shared_arg),
        format!("plus(Column#{}, 1)", x.unique_id)
    );
}

/// E3: `SELECT IF(x, a, b) FROM (SELECT 1 AS x, a, b FROM t) s` with `a` NOT NULL and `b`
/// nullable. Merging the projections folds `IF(1, a, b)` to the column `a`, and Go then sets
/// the IF's nullability on the folded node in place: the `*Column` (and its `*FieldType`)
/// is shared with the DataSource, so Go clears NOT NULL on the DataSource's `a` as well
/// (code path confirmed in the #147 review; for memory tables the effect is global and was
/// reproduced on v8.5.8, report §3.3.1 and #148). With copy-on-write only the projection's copy
/// changes. This test pins the Rust behaviour; the divergence is a finding, not a fix.
#[test]
#[ignore = "blocked-by:D08.9 (the IF function class is not ported; run when it is)"]
fn fold_then_set_flag_is_copy_on_write() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx());
    let ds = data_source(&mut ctx);
    let cols = schema_of(&ctx, ds).columns;
    let (a, b) = (cols[0].clone(), cols[1].clone());
    assert!(a.ret_type.get_flag() & NotNullFlag != 0);
    let x = new_col(&ctx, int_type(true));
    let inner = projection(
        &mut ctx,
        ds,
        vec![
            (one(), x.clone()),
            (col_expr(&a), a.clone()),
            (col_expr(&b), b.clone()),
        ],
    );
    let if_expr = func(
        &ctx,
        ast::If,
        vec![col_expr(&x), col_expr(&a), col_expr(&b)],
    );
    assert!(if_expr.get_static_type().get_flag() & NotNullFlag == 0);
    let y = new_col(&ctx, if_expr.get_static_type().clone());
    let outer = projection(&mut ctx, inner, vec![(if_expr, y)]);

    let mut opt = LogicalOptimizeOp::default();
    let (root, _) = ProjectionEliminator
        .optimize(&mut ctx, outer, &mut opt)
        .unwrap();
    assert_eq!(to_string(&ctx, root), "DataScan(t)->Projection");
    let p = ctx.arena.downcast_ref::<LogicalProjection>(root).unwrap();
    let Expr::Column(folded) = &*p.exprs[0] else {
        panic!("IF(1, a, b) folds to a: {}", expr_string(&ctx, &p.exprs[0]));
    };
    assert_eq!(folded.unique_id, a.unique_id);
    assert_eq!(folded.ret_type.get_flag() & NotNullFlag, 0);
    // The DataSource keeps NOT NULL on `a` (Go: cleared through the shared pointer).
    assert!(schema_of(&ctx, ds).columns[0].ret_type.get_flag() & NotNullFlag != 0);
}
