//! Physical plans as `Arc` trees (N1 §3.4–§3.5, pilot questions Q4 and proposal P2):
//! in-place `postOptimize` while the statement owns the tree, copy-on-write once shared,
//! and plan-cache range rebuild by path copying.

use std::sync::Arc;

use tidb_base::mysql::TypeLonglong;
use tidb_expression::{Column, Expr, ExprRef, Schema, new_schema};
use tidb_model::TableInfo;
use tidb_planner_pilot::physicalop::{
    PhysicalPlan, PhysicalPlanRef, PhysicalProjection, PhysicalSelection, PhysicalTableReader,
    PhysicalTableScan, Range,
};
use tidb_planner_pilot::plan_cache_rebuild::rebuild_range;
use tidb_planner_pilot::rule_eliminate_projection::eliminate_physical_projection;
use tidb_types::field_type::new_field_type;

const _: () = {
    const fn send_sync<T: Send + Sync + ?Sized>() {}
    send_sync::<dyn PhysicalPlan>();
    send_sync::<Expr>();
};

fn col(id: i64) -> Arc<Column> {
    Arc::new(Column {
        unique_id: id,
        ret_type: new_field_type(TypeLonglong),
        orig_name: format!("test.t.c{id}"),
        ..Column::default()
    })
}

fn col_expr(c: &Arc<Column>) -> ExprRef {
    Expr::Column((**c).clone()).into_ref()
}

fn schema(cols: &[Arc<Column>]) -> Schema {
    new_schema(cols.to_vec())
}

fn reader(cols: &[Arc<Column>], ranges: Vec<Range>) -> PhysicalPlanRef {
    let ts = PhysicalTableScan {
        id: 1,
        schema: schema(cols),
        children: vec![],
        access_condition: vec![],
        table: Arc::new(TableInfo::default()),
        ranges,
    };
    Arc::new(PhysicalTableReader {
        id: 2,
        schema: schema(cols),
        children: vec![],
        table_plans: vec![Arc::new(ts)],
    })
}

fn proj(id: i32, child: PhysicalPlanRef, cols: &[Arc<Column>]) -> PhysicalPlanRef {
    Arc::new(PhysicalProjection {
        id,
        schema: schema(cols),
        children: vec![child],
        exprs: cols.iter().map(col_expr).collect(),
        calculate_no_delay: false,
    })
}

fn scan_ranges(p: &PhysicalPlanRef) -> Vec<Range> {
    let mut p = p.clone();
    loop {
        if let Some(r) = p.as_any().downcast_ref::<PhysicalTableReader>() {
            let ts = r.table_plans[0]
                .as_any()
                .downcast_ref::<PhysicalTableScan>()
                .unwrap();
            return ts.ranges.clone();
        }
        p = p.children()[0].clone();
    }
}

/// postOptimize on a tree the statement owns alone: the projection that only copies its
/// child's columns is removed and the remaining nodes are changed in place (no copies).
#[test]
fn post_optimize_in_place_on_owned_tree() {
    let cols = [col(1), col(2)];
    let r = reader(&cols, vec![]);
    let r_ptr = Arc::as_ptr(&r) as *const ();
    let sel: PhysicalPlanRef = Arc::new(PhysicalSelection {
        id: 3,
        schema: schema(&cols),
        children: vec![proj(4, r, &cols)],
        conditions: vec![],
    });
    let sel_ptr = Arc::as_ptr(&sel) as *const ();
    let root = eliminate_physical_projection(sel);
    // Same Selection node (changed in place), its child is now the reader itself.
    assert_eq!(Arc::as_ptr(&root) as *const (), sel_ptr);
    assert_eq!(Arc::as_ptr(&root.children()[0]) as *const (), r_ptr);
}

/// A child shared by two candidate parents (as a memoized best task is in FindBestTask):
/// once the losing candidate is dropped, the winner's subtree is uniquely owned again and
/// postOptimize changes it in place; while the loser is alive, the shared node is copied.
#[test]
fn memo_shared_child_is_copied_only_while_shared() {
    let cols = [col(1)];
    let shared_child = proj(5, reader(&cols, vec![]), &cols);
    let winner = proj(6, shared_child.clone(), &cols);
    let loser = proj(7, shared_child.clone(), &cols);
    drop(shared_child);

    // Loser alive: eliminating the winner's projections must not touch the loser's child.
    let loser_child_before = Arc::as_ptr(&loser.children()[0]) as *const ();
    let out = eliminate_physical_projection(winner.clone());
    assert_eq!(
        out.id(),
        2,
        "both projections removed: the reader is the root"
    );
    assert_eq!(
        Arc::as_ptr(&loser.children()[0]) as *const (),
        loser_child_before
    );
    assert_eq!(loser.children()[0].id(), 5);
    drop(loser);
    drop(winner);
}

/// Q4: an instance plan cache entry is shared by sessions; a hit rebuilds the ranges for
/// its parameters by copying only the path to the scan. The cached plan is unchanged and
/// concurrent rebuilds do not interfere.
#[test]
fn plan_cache_rebuild_by_path_copy() {
    let cols = [col(1), col(2)];
    let cached: PhysicalPlanRef = Arc::new(PhysicalSelection {
        id: 9,
        schema: schema(&cols),
        children: vec![proj(
            10,
            reader(&cols, vec![Range { low: 1, high: 1 }]),
            &cols,
        )],
        conditions: vec![],
    });

    std::thread::scope(|s| {
        for k in 0..4i64 {
            let cached = cached.clone();
            s.spawn(move || {
                let mut plan = cached.clone();
                rebuild_range(&mut plan, &|_ts| {
                    Ok(vec![Range {
                        low: k,
                        high: k + 10,
                    }])
                })
                .unwrap();
                assert_eq!(
                    scan_ranges(&plan),
                    [Range {
                        low: k,
                        high: k + 10
                    }]
                );
                // The root and the path to the scan were copied ...
                assert!(!Arc::ptr_eq(&plan, &cached));
                assert!(!Arc::ptr_eq(&plan.children()[0], &cached.children()[0]));
                // ... the expressions and schemas inside them are shared, not cloned.
                let new_p = plan.children()[0]
                    .as_any()
                    .downcast_ref::<PhysicalProjection>()
                    .unwrap();
                let old_p = cached.children()[0]
                    .as_any()
                    .downcast_ref::<PhysicalProjection>()
                    .unwrap();
                assert!(Arc::ptr_eq(&new_p.exprs[0], &old_p.exprs[0]));
                assert!(Arc::ptr_eq(
                    &new_p.schema.columns[0],
                    &old_p.schema.columns[0]
                ));
            });
        }
    });
    // The cached plan still has its original ranges.
    assert_eq!(scan_ranges(&cached), [Range { low: 1, high: 1 }]);
}
