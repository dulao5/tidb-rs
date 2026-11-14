//! The Go unit tests of `pkg/planner/util` (VS1-19): `path_test.go` (`TestCompareCol2Len`,
//! `TestOnlyPointRange`) and `column_test.go` (`TestIndexInfo2Cols`), ported with their
//! assertions; plus Rust unit tests of the functions the govec `planutil` domain exempts
//! (the clone helpers, `PhysicalProperty.String`, `GetPushDownCtx*`) and of
//! `BasePhysicalPlan` (`physicalop/base_physical_plan.rs`) and `optimizetrace`.

use std::sync::Arc;

use parking_lot::Mutex;
use tidb_base::mysql::TypeLonglong;
use tidb_execdetails::tracing::opt_trace::{CandidatePlanTrace, PhysicalOptimizeTracer, PlanTrace};
use tidb_expression::{Column, Expr, Schema, new_schema};
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo};
use tidb_parser::model::CIStr;
use tidb_planner::base::PhysicalPlan;
use tidb_planner::physicalop::{BasePhysicalPlan, PhysicalPlanRef};
use tidb_planner::property::{PhysicalProperty, RootTaskType, SortItem, StatsInfo};
use tidb_planner::util::costusage::{CostFlagRecalculate, CostFlagTrace};
use tidb_planner::util::misc::{
    clone_by_itemss, clone_cols, clone_datums, clone_exprs, clone_field_names, clone_ranges,
    clone_sort_items,
};
use tidb_planner::util::optimizetrace::{
    PhysicalOptimizeOp, PlanCostOption, default_logical_optimize_option,
    new_default_plan_cost_option,
};
use tidb_planner::util::path::{AccessPath, Col2Len, compare_col2_len};
use tidb_planner::util::{ByItems, column};
use tidb_planner::{PhysicalKind, physical_plan_boilerplate};
use tidb_sessionctx::ranger::types::Range;
use tidb_types::collate;
use tidb_types::context::DefaultStmtNoWarningContext;
use tidb_types::datum::{Datum, min_not_null_datum, new_int_datum};
use tidb_types::field_name::FieldName;
use tidb_types::field_type::new_field_type;

fn c2l(pairs: &[(i64, isize)]) -> Col2Len {
    pairs.iter().copied().collect()
}

// from: pkg/planner/util/path_test.go TestCompareCol2Len
#[test]
fn test_compare_col2_len() {
    let tests = [
        (
            c2l(&[(1, -1), (2, -1), (3, -1)]),
            c2l(&[(1, -1), (2, 10)]),
            1,
            true,
        ),
        (c2l(&[(1, 5)]), c2l(&[(1, 10), (2, -1)]), -1, true),
        (
            c2l(&[(1, -1), (2, -1)]),
            c2l(&[(1, -1), (2, 5), (3, -1)]),
            0,
            false,
        ),
        (
            c2l(&[(1, -1), (2, 10)]),
            c2l(&[(1, -1), (2, 5), (3, -1)]),
            0,
            false,
        ),
        (c2l(&[(1, -1), (2, 10)]), c2l(&[(1, -1), (2, 10)]), 0, true),
        (c2l(&[(1, -1), (2, -1)]), c2l(&[(1, -1), (2, 10)]), 0, false),
    ];
    for (c1, c2, res, comparable) in tests {
        let (r, comparable1) = compare_col2_len(&c1, &c2);
        assert_eq!(res, r);
        assert_eq!(comparable, comparable1);
    }
}

fn point(low: Datum, high: Datum) -> Range {
    Range {
        low_val: vec![low],
        high_val: vec![high],
        collators: collate::get_binary_collator_slice(1),
        ..Default::default()
    }
}

// from: pkg/planner/util/path_test.go TestOnlyPointRange
#[test]
fn test_only_point_range() {
    let mut null_datum = min_not_null_datum();
    null_datum.set_null();
    let null_point_range = point(null_datum.clone(), null_datum.clone());
    let one_point_range = point(new_int_datum(1), new_int_datum(1));
    let one2_two_range = point(new_int_datum(1), new_int_datum(2));

    let tc = &*DefaultStmtNoWarningContext;
    let mut int_handle_path = AccessPath {
        is_int_handle_path: true,
        ..Default::default()
    };
    int_handle_path.ranges = vec![null_point_range.clone(), one_point_range.clone()];
    assert!(int_handle_path.only_point_range(tc));
    int_handle_path.ranges = vec![one_point_range.clone(), one2_two_range.clone()];
    assert!(!int_handle_path.only_point_range(tc));

    let index = |n: usize| {
        Some(Arc::new(IndexInfo {
            columns: Some((0..n).map(|_| IndexColumn::default()).collect()),
            ..Default::default()
        }))
    };
    let mut index_path = AccessPath {
        index: index(1),
        ..Default::default()
    };
    index_path.ranges = vec![one_point_range.clone()];
    assert!(index_path.only_point_range(tc));
    index_path.ranges = vec![null_point_range.clone(), one_point_range.clone()];
    assert!(!index_path.only_point_range(tc));
    index_path.ranges = vec![one_point_range.clone(), one2_two_range.clone()];
    assert!(!index_path.only_point_range(tc));

    index_path.index = index(2);
    index_path.ranges = vec![one_point_range.clone()];
    assert!(!index_path.only_point_range(tc));
}

fn int_col(id: i64) -> Arc<Column> {
    Arc::new(Column {
        unique_id: id,
        id,
        ret_type: new_field_type(TypeLonglong),
        ..Default::default()
    })
}

fn col_info(id: i64, name: &str) -> ColumnInfo {
    ColumnInfo {
        id,
        name: CIStr::new(name),
        ..Default::default()
    }
}

// from: pkg/planner/util/column_test.go TestIndexInfo2Cols
#[test]
fn test_index_info2_cols() {
    let (col0, col1, col2) = (int_col(0), int_col(1), int_col(2));
    let (col_info0, col_info1, col_info2) = (col_info(0, "0"), col_info(1, "1"), col_info(2, "2"));
    let index_col = |n: &str| IndexColumn {
        name: CIStr::new(n),
        ..Default::default()
    };
    let index_info = IndexInfo {
        columns: Some(vec![index_col("0"), index_col("1"), index_col("2")]),
        ..Default::default()
    };

    let cols = vec![Arc::clone(&col0)];
    let col_infos = vec![col_info0.clone()];
    let (res_cols, lengths) = column::index_info2_prefix_cols(&col_infos, &cols, &index_info);
    assert_eq!(res_cols.len(), 1);
    assert_eq!(lengths.len(), 1);
    assert!(res_cols[0].equal_col(&col0));

    let cols = vec![Arc::clone(&col1)];
    let col_infos = vec![col_info1.clone()];
    let (res_cols, lengths) = column::index_info2_prefix_cols(&col_infos, &cols, &index_info);
    assert_eq!(res_cols.len(), 0);
    assert_eq!(lengths.len(), 0);

    let cols = vec![Arc::clone(&col0), Arc::clone(&col1)];
    let col_infos = vec![col_info0.clone(), col_info1.clone()];
    let (res_cols, lengths) = column::index_info2_prefix_cols(&col_infos, &cols, &index_info);
    assert_eq!(res_cols.len(), 2);
    assert_eq!(lengths.len(), 2);
    assert!(res_cols[0].equal_col(&col0));
    assert!(res_cols[1].equal_col(&col1));

    // If col1 has been pruned, the prefix columns should just be [col0]
    let cols = vec![Arc::clone(&col0), Arc::clone(&col2)];
    let col_infos = vec![col_info0.clone(), col_info2.clone()];
    let (prefix_cols, prefix_lens, full_cols, full_lens) =
        column::index_info2_cols(&col_infos, &cols, &index_info);
    let (res_cols, lengths) = column::index_info2_prefix_cols(&col_infos, &cols, &index_info);
    assert!(
        res_cols
            .iter()
            .zip(&prefix_cols)
            .all(|(a, b)| Arc::ptr_eq(a, b))
    );
    assert_eq!(res_cols.len(), prefix_cols.len());
    assert_eq!(lengths, prefix_lens);
    assert_eq!(res_cols.len(), 1);
    assert_eq!(lengths.len(), 1);
    assert!(res_cols[0].equal_col(&col0));

    // If col1 has been pruned, the full columns should be [col0, nil, col2]
    let (res_cols, lengths) = column::index_info2_full_cols(&col_infos, &cols, &index_info);
    assert_eq!(res_cols.len(), full_cols.len());
    for (a, b) in res_cols.iter().zip(&full_cols) {
        match (a, b) {
            (Some(a), Some(b)) => assert!(Arc::ptr_eq(a, b)),
            (None, None) => {}
            _ => panic!("full columns differ"),
        }
    }
    assert_eq!(lengths, full_lens);
    assert_eq!(res_cols.len(), 3);
    assert_eq!(lengths.len(), 3);
    assert!(res_cols[0].as_ref().unwrap().equal_col(&col0));
    assert!(res_cols[1].is_none());
    assert!(res_cols[2].as_ref().unwrap().equal_col(&col2));
}

// ---------------------------------------------------------------- clone helpers

#[test]
fn clone_helpers_share_nodes_and_copy_names() {
    let c = int_col(7);
    let cloned = clone_cols(&[Some(Arc::clone(&c)), None]);
    assert!(Arc::ptr_eq(cloned[0].as_ref().unwrap(), &c));
    assert!(cloned[1].is_none());
    let e = Expr::Column((*c).clone()).into_ref();
    assert!(Arc::ptr_eq(&clone_exprs(std::slice::from_ref(&e))[0], &e));
    let name = Arc::new(FieldName {
        col_name: CIStr::new("a"),
        ..Default::default()
    });
    let names = clone_field_names(std::slice::from_ref(&name));
    assert!(!Arc::ptr_eq(&names[0], &name));
    assert_eq!(*names[0], *name);
    assert_eq!(clone_datums(&[new_int_datum(3)])[0].get_int64(), 3);
    let r = point(new_int_datum(1), new_int_datum(2));
    assert_eq!(
        format!("{:?}", clone_ranges(std::slice::from_ref(&r))[0]),
        format!("{r:?}")
    );
    let items = clone_sort_items(&[SortItem {
        col: Arc::clone(&c),
        desc: true,
    }]);
    assert!(items[0].desc && Arc::ptr_eq(&items[0].col, &c));
    let by = clone_by_itemss(&[ByItems {
        expr: Arc::clone(&e),
        desc: false,
    }]);
    assert!(Arc::ptr_eq(&by[0].expr, &e));
    let path = AccessPath {
        idx_cols: vec![Arc::clone(&c)],
        ranges: vec![r],
        partial_index_paths: vec![AccessPath::default()],
        ..Default::default()
    }
    .clone_path();
    assert_eq!(path.idx_cols.len(), 1);
    assert_eq!(path.partial_index_paths.len(), 1);
}

#[test]
fn physical_property_string() {
    let c = int_col(1);
    let p = PhysicalProperty {
        sort_items: vec![SortItem {
            col: Arc::clone(&c),
            desc: true,
        }],
        task_tp: RootTaskType,
        expected_cnt: 10.0,
        ..Default::default()
    };
    // Go prints the *Column pointer by reflection.
    assert_eq!(
        String::from_utf8(p.to_string()).unwrap(),
        format!(
            "Prop{{cols: [{{{:p} true}}], TaskTp: rootTask, expectedCount: 10}}",
            Arc::as_ptr(&c)
        )
    );
}

// ---------------------------------------------------------------- BasePhysicalPlan

#[derive(Clone)]
struct Leaf {
    base: BasePhysicalPlan,
    schema: Schema,
    resolved: usize,
}

impl PhysicalPlan for Leaf {
    physical_plan_boilerplate!(TableScan);
    fn schema(&self) -> &Schema {
        &self.schema
    }
    fn resolve_indices(&mut self) -> Result<(), tidb_base::Error> {
        self.resolved += 1;
        Ok(())
    }
}

#[derive(Clone)]
struct Inner {
    base: BasePhysicalPlan,
}

impl PhysicalPlan for Inner {
    physical_plan_boilerplate!(Selection);
}

fn leaf(id: i32) -> PhysicalPlanRef {
    let mut base = BasePhysicalPlan::default();
    base.plan.set_tp("TableFullScan");
    base.plan.set_id(id);
    base.plan.set_stats(Some(Arc::new(StatsInfo {
        row_count: 5.0,
        ..Default::default()
    })));
    Arc::new(Leaf {
        base,
        schema: new_schema(vec![int_col(1)]),
        resolved: 0,
    })
}

fn inner(id: i32, children: Vec<PhysicalPlanRef>) -> PhysicalPlanRef {
    let mut base = BasePhysicalPlan::default();
    base.plan.set_tp("Selection");
    base.plan.set_id(id);
    base.children = children;
    base.plan.set_stats(Some(Arc::new(StatsInfo {
        row_count: 2.5,
        ..Default::default()
    })));
    Arc::new(Inner { base })
}

#[test]
fn base_physical_plan_methods() {
    let mut p = inner(3, vec![leaf(1), leaf(2)]);
    // Schema() is the first child's.
    assert_eq!(p.schema().columns[0].unique_id, 1);
    assert_eq!(p.explain_info(), "");
    assert_eq!(p.explain_id(None), "Selection_3");
    assert_eq!(p.stats_count(), 2.5);
    assert_eq!(p.get_est_row_count_for_display(), 2.5);
    let trace = p.build_plan_trace();
    assert_eq!(
        (trace.id, trace.tp.as_str(), trace.children.len()),
        (3, "Selection", 2)
    );

    // ResolveIndices reaches every child, in place while the tree is owned.
    tidb_planner::make_mut_plan(&mut p)
        .resolve_indices()
        .unwrap();
    for c in p.children() {
        assert_eq!(c.as_any().downcast_ref::<Leaf>().unwrap().resolved, 1);
    }

    // The cost is computed once and reused unless CostFlagRecalculate is set.
    let mut opt = new_default_plan_cost_option();
    let node = tidb_planner::make_mut_plan(&mut p);
    let c = node.get_plan_cost_ver2(RootTaskType, &opt).unwrap();
    assert_eq!(c.get_cost(), 0.0);
    assert!(node.base_physical().cost.lock().plan_cost_init);
    opt.with_cost_flag(CostFlagRecalculate | CostFlagTrace);
    let c = node.get_plan_cost_ver2(RootTaskType, &opt).unwrap();
    assert!(c.get_trace().is_some());
    assert_eq!(node.get_plan_cost_ver1(RootTaskType, &opt).unwrap(), 0.0);

    // Clone is not supported by the base; CloneForPlanCache neither.
    assert!(p.clone_plan().is_err());
    assert!(p.clone_for_plan_cache().is_none());
    assert!(p.base_physical().clone_with_self().is_err());
    assert!(p.base_physical().clone_for_plan_cache_with_self().is_none());
    let mut leaf_only = BasePhysicalPlan::default();
    leaf_only.set_children_req_props(vec![
        Some(Arc::new(PhysicalProperty {
            can_add_enforcer: true,
            expected_cnt: 3.0,
            ..Default::default()
        })),
        None,
    ]);
    let cloned = leaf_only.clone_with_self().unwrap();
    assert!(
        cloned
            .get_child_req_props(0)
            .is_some_and(|p| !p.can_add_enforcer)
    );
    assert!(p.memory_usage() > 0);
    assert_eq!(p.kind(), PhysicalKind::Selection);
}

#[test]
fn append_child_candidate_records_children() {
    let p = inner(3, vec![leaf(1), leaf(2)]);
    let tracer = Arc::new(Mutex::new(PhysicalOptimizeTracer {
        candidates: Some(Default::default()),
        ..Default::default()
    }));
    let mut op = PhysicalOptimizeOp::default();
    op.with_enable_optimize_tracer(Some(Arc::clone(&tracer)));
    let mut root = PlanTrace::default();
    root.id = 3;
    root.tp = "Selection".into();
    op.append_candidate(CandidatePlanTrace {
        plan_trace: Some(root),
        ..Default::default()
    });
    p.append_child_candidate(&op);
    let t = tracer.lock();
    let cands = t.candidates.as_ref().unwrap();
    assert_eq!(cands.len(), 3);
    assert_eq!(
        cands[&3].plan_trace.as_ref().unwrap().children_id,
        Some(vec![1, 2])
    );
}

#[test]
fn optimize_options() {
    let op = default_logical_optimize_option();
    assert!(op.tracer_is_nil());
    op.append_step_to_current(1, "x", || unreachable!(), || unreachable!());
    let mut opt = PlanCostOption::default();
    opt.with_optimize_tracer(Some(PhysicalOptimizeOp::default()));
    assert_eq!(opt.cost_flag & CostFlagTrace, 0);
    opt.with_optimize_tracer(Some({
        let mut o = PhysicalOptimizeOp::default();
        o.with_enable_optimize_tracer(Some(Arc::new(
            Mutex::new(PhysicalOptimizeTracer::default()),
        )));
        o
    }));
    assert_ne!(opt.cost_flag & CostFlagTrace, 0);
}

/// The cost memo is written through `&self`: a child shared by two candidate parents (the
/// `taskMap` case) is costed once, on the shared node, and nothing is copied.
#[test]
fn cost_memo_on_a_shared_child_is_filled_in_place() {
    let shared = leaf(1);
    let a = inner(2, vec![Arc::clone(&shared)]);
    let b = inner(3, vec![Arc::clone(&shared)]);
    let opt = new_default_plan_cost_option();
    a.get_plan_cost_ver2(RootTaskType, &opt).unwrap();
    assert!(Arc::ptr_eq(&a.children()[0], &shared));
    assert!(shared.base_physical().cost.lock().plan_cost_init);
    b.get_plan_cost_ver2(RootTaskType, &opt).unwrap();
    assert!(Arc::ptr_eq(&b.children()[0], &shared));
    assert_eq!(Arc::strong_count(&shared), 3);
}
