//! Unit tests of the physical operators, the tasks, `Attach2Task` and
//! `exhaustPhysicalPlans` of the first vertical slice (`physical_plans.rs`,
//! `task_base.rs`, `task.rs`, `exhaust_physical_plans.rs`, `initialize.rs`).
//!
//! The expected values are Go v8.5.8's behaviour read off the code (the functions are
//! small); the plan-level comparison with Go comes with `FindBestTask` (the EXPLAIN plan
//! vectors of the FindBestTask task).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::kv;
use tidb_base::mysql::TypeLonglong;
use tidb_expression::{Column, Expr, ExprRef, Schema, new_schema};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, ExtraHandleID, TableInfo};
use tidb_parser::model::CIStr;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_type::new_field_type;

use crate::base::{LogicalPlan, PhysicalKind, PhysicalPlan, exhaust_physical_plans};
use crate::core::exhaust_physical_plans::match_items;
use crate::core::find_best_task::get_prop_by_order_by_items;
use crate::core::initialize::{flatten_list_push_down_plan, flatten_tree_push_down_plan};
use crate::core::physical_plans::*;
use crate::core::plan::enforce_property;
use crate::core::task::{attach_plan2_task, attach2_task, calc_paging_cost, extract_rows};
use crate::core::task_base::{CopTask, RootTask, Task};
use crate::logicalop::{DataSource, LogicalLimit, LogicalSort, LogicalTableDual, LogicalTopN};
use crate::physicalop::PhysicalPlanRef;
use crate::property::{
    CopMultiReadTaskType, CopSingleReadTaskType, MppTaskType, PhysicalProperty, RootTaskType,
    SortItem, StatsInfo,
};
use crate::util::{AccessPath, ByItems};
use crate::{PlanCtx, PlanId};

struct Acc;

impl GlobalVarAccessor for Acc {
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

/// Go's `mock.Client`: every request type `kv.RequestTypeSupportedChecker` accepts.
struct MockClient;

impl Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("MockClient.send")
    }

    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool {
        RequestTypeSupportedChecker.is_request_type_supported(req_type, sub_type)
    }
}

fn session() -> SessionVars {
    SessionVars::new(Box::new(Acc))
}

fn ctx_of(vars: &mut SessionVars) -> PlanCtx<'_> {
    let mut ctx = PlanCtx::new(vars, es::new_expr_context(Vec::new()));
    ctx.client = Some(Arc::new(MockClient));
    ctx
}

fn col(uid: i64) -> Arc<Column> {
    Arc::new(Column {
        unique_id: uid,
        id: uid,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        ..Column::default()
    })
}

fn col_expr(c: &Arc<Column>) -> ExprRef {
    Arc::new(Expr::Column((**c).clone()))
}

fn stats(rows: f64) -> Option<Arc<StatsInfo>> {
    Some(Arc::new(StatsInfo {
        row_count: rows,
        ..Default::default()
    }))
}

/// A TiKV table scan of `t` with columns 1, 2 and `rows` rows.
fn table_scan(ctx: &PlanCtx<'_>, rows: f64) -> PhysicalPlanRef {
    let mut ts = PhysicalTableScan {
        table: Arc::new(TableInfo {
            name: CIStr::new("t"),
            ..Default::default()
        }),
        columns: vec![
            Arc::new(ColumnInfo {
                id: 1,
                ..Default::default()
            }),
            Arc::new(ColumnInfo {
                id: 2,
                ..Default::default()
            }),
        ],
        store_type: kv::TiKV,
        ..Default::default()
    }
    .init(ctx, 0);
    ts.set_schema(new_schema(vec![col(1), col(2)]));
    ts.set_stats(stats(rows));
    Arc::new(ts)
}

/// The cop task of a table read (as `convertToTableScan` builds it).
fn table_cop_task(ctx: &PlanCtx<'_>, rows: f64) -> CopTask {
    CopTask {
        table_plan: Some(table_scan(ctx, rows)),
        index_plan_finished: true,
        ..Default::default()
    }
}

fn root_plan(t: &Task) -> &PhysicalPlanRef {
    let Task::Root(r) = t else {
        panic!("not a root task: {t:?}");
    };
    r.get_plan().expect("plan")
}

fn kinds(p: &PhysicalPlanRef) -> Vec<PhysicalKind> {
    let mut out = vec![p.kind()];
    let mut cur = p;
    loop {
        if let Some(r) = cur.as_any().downcast_ref::<PhysicalTableReader>() {
            cur = r.table_plan.as_ref().expect("table plan");
        } else if let Some(c) = cur.children().first() {
            cur = c;
        } else {
            break;
        }
        out.push(cur.kind());
    }
    out
}

#[test]
fn read_req_type_names() {
    assert_eq!(Cop.name(), "cop");
    assert_eq!(BatchCop.name(), "batchCop");
    assert_eq!(MPP.name(), "mpp");
    assert_eq!(ReadReqType(9).name(), "cop");
}

/// `flattenTreePushDownPlan`'s doc example: [H, E, C, F, G, D, B, A], with the map of the
/// children whose parent is not next to them.
#[test]
fn flatten_push_down_plans_follow_go_order() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let node = |id: i32, children: Vec<PhysicalPlanRef>| -> PhysicalPlanRef {
        let mut p = PhysicalSelection::default().init(&ctx, None, 0, Vec::new());
        p.set_id(id);
        p.set_children(children);
        Arc::new(p)
    };
    let h = node(8, vec![]);
    let e = node(5, vec![h]);
    let c = node(3, vec![e]);
    let f = node(6, vec![]);
    let g = node(7, vec![]);
    let d = node(4, vec![f, g]);
    let b = node(2, vec![c, d]);
    let a = node(1, vec![b]);
    let (plans, map) = flatten_tree_push_down_plan(&a);
    let ids: Vec<i32> = plans.iter().map(|p| p.id()).collect();
    assert_eq!(ids, [8, 5, 3, 6, 7, 4, 2, 1]);
    let mut map: Vec<(isize, isize)> = map.expect("map").into_iter().collect();
    map.sort();
    // C (2) -> B (6), F (3) -> D (5).
    assert_eq!(map, [(2, 6), (3, 5)]);

    let list = flatten_list_push_down_plan(&node(10, vec![node(11, vec![node(12, vec![])])]));
    let ids: Vec<i32> = list.iter().map(|p| p.id()).collect();
    assert_eq!(ids, [12, 11, 10]);
    let (_, map) = flatten_tree_push_down_plan(&node(10, vec![node(11, vec![])]));
    assert!(
        map.is_none(),
        "Go's map stays nil without a multi-child node"
    );
}

#[test]
fn expand_virtual_column_moves_extra_columns_and_adds_base_columns() {
    let info = |id: i64| {
        Arc::new(ColumnInfo {
            id,
            ..Default::default()
        })
    };
    let table_cols = vec![info(1), info(2), info(3)];
    // No virtual column: same columns and schema.
    let mut schema = new_schema(vec![col(1), col(2)]);
    let cols = expand_virtual_column(&table_cols[..2], &mut schema, &table_cols);
    assert_eq!(cols.len(), 2);
    assert_eq!(schema.len(), 2);

    // Column 2 is virtual on column 3; the extra handle stays last.
    let mut v = (*col(2)).clone();
    v.virtual_expr = Some(col_expr(&col(3)));
    let handle = Arc::new(Column {
        unique_id: 100,
        id: ExtraHandleID as i64,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        ..Column::default()
    });
    let mut schema = new_schema(vec![col(1), Arc::new(v), Arc::clone(&handle)]);
    let handle_info = info(ExtraHandleID as i64);
    let cols = expand_virtual_column(&[info(1), info(2), handle_info], &mut schema, &table_cols);
    let ids: Vec<i64> = cols.iter().map(|c| c.id).collect();
    assert_eq!(ids, [1, 2, 3, ExtraHandleID as i64]);
    let ids: Vec<i64> = schema.columns.iter().map(|c| c.id).collect();
    assert_eq!(ids, [1, 2, 3, ExtraHandleID as i64]);
}

#[test]
fn root_task_copy_drops_is_empty_and_invalid_task() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let mut r = RootTask::default();
    assert!(r.invalid());
    assert!(Task::invalid_task().invalid());
    r.set_plan(table_scan(&ctx, 10.0));
    r.set_empty(true);
    assert!(
        !r.copy().is_empty(),
        "Go's RootTask.Copy does not copy isEmpty"
    );
    assert!(r.clone().is_empty());
    assert_eq!(r.count(), 10.0);
    assert!(Task::Cop(CopTask::default()).invalid());
}

/// `attachPlan2Task` moves the task's plan under `p` without another reference.
#[test]
fn attach_plan2_task_moves_the_child() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let cop = table_cop_task(&ctx, 10.0);
    let sel = PhysicalSelection::default().init(&ctx, stats(5.0), 0, Vec::new());
    let t = attach_plan2_task(Arc::new(sel), Task::Cop(cop));
    let Task::Cop(cop) = &t else { panic!() };
    let top = cop.table_plan.as_ref().unwrap();
    assert_eq!(top.kind(), PhysicalKind::Selection);
    assert_eq!(Arc::strong_count(top), 1);
    assert_eq!(Arc::strong_count(&top.children()[0]), 1);
    assert_eq!(t.count(), 5.0);

    // Not finished: the index side.
    let mut cop = CopTask {
        index_plan: Some(table_scan(&ctx, 3.0)),
        ..Default::default()
    };
    cop.index_plan_finished = false;
    let sel = PhysicalSelection::default().init(&ctx, stats(2.0), 0, Vec::new());
    let Task::Cop(cop) = attach_plan2_task(Arc::new(sel), Task::Cop(cop)) else {
        panic!()
    };
    assert_eq!(cop.index_plan.unwrap().kind(), PhysicalKind::Selection);
}

#[test]
fn cop_task_converts_to_a_table_reader() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let cop = table_cop_task(&ctx, 10.0);
    let t = Task::Cop(cop.clone()).convert_to_root_task(&ctx).unwrap();
    let p = root_plan(&t);
    assert_eq!(p.kind(), PhysicalKind::TableReader);
    let r = p.as_any().downcast_ref::<PhysicalTableReader>().unwrap();
    assert_eq!(r.store_type, kv::TiKV);
    assert_eq!(r.read_req_type, Cop);
    assert_eq!(r.table_plans.len(), 1);
    assert_eq!(r.schema().len(), 2);
    assert_eq!(p.stats_info().unwrap().row_count, 10.0);
    assert_eq!(r.get_table_scan().unwrap().columns.len(), 2);
    // The cop task is unchanged (Go converts a copy).
    assert!(cop.table_plan.is_some());

    // A single-read index cop task converts to a PhysicalIndexReader.
    let mut is = PhysicalIndexScan {
        table: Arc::new(TableInfo {
            name: CIStr::new("t"),
            ..Default::default()
        }),
        data_source_schema: Some(new_schema(vec![col(1)])),
        ..Default::default()
    }
    .init(&ctx, 0);
    is.set_schema(new_schema(vec![col(1)]));
    is.set_stats(stats(4.0));
    let idx_cop = CopTask {
        index_plan: Some(Arc::new(is)),
        ..Default::default()
    };
    let idx_root = Task::Cop(idx_cop).convert_to_root_task(&ctx).unwrap();
    let idx_p = root_plan(&idx_root);
    assert_eq!(idx_p.kind(), PhysicalKind::IndexReader);
    let ir = idx_p
        .as_any()
        .downcast_ref::<PhysicalIndexReader>()
        .unwrap();
    assert_eq!(ir.index_plans.len(), 1);
    assert_eq!(ir.schema().len(), 1);
    assert_eq!(ir.output_columns.len(), 1);

    // An index lookup and an index merge are outside part 2A.
    for cop in [
        CopTask {
            index_plan: Some(table_scan(&ctx, 1.0)),
            table_plan: Some(table_scan(&ctx, 1.0)),
            ..Default::default()
        },
        CopTask {
            idx_merge_part_plans: Some(vec![table_scan(&ctx, 1.0)]),
            table_plan: Some(table_scan(&ctx, 1.0)),
            index_plan_finished: true,
            ..Default::default()
        },
    ] {
        let err = Task::Cop(cop).convert_to_root_task(&ctx).unwrap_err();
        assert_eq!(blocked_by(&err), Some("D10.1"), "{err}");
    }
}

/// Limit over a table cop task: a pushed-down limit of offset + count in the cop task, the
/// reader, then the root limit (`PhysicalLimit.Attach2Task`, the normal case).
#[test]
fn limit_attach_pushes_offset_plus_count_into_the_cop_task() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let cop = table_cop_task(&ctx, 1000.0);
    let limit = PhysicalLimit {
        offset: 5,
        count: 10,
        ..Default::default()
    }
    .init(&ctx, stats(10.0), 0, Vec::new());
    let t = attach2_task(&ctx, Arc::new(limit), &[Task::Cop(cop)]).unwrap();
    let p = root_plan(&t);
    assert_eq!(
        kinds(p),
        [
            PhysicalKind::Limit,
            PhysicalKind::TableReader,
            PhysicalKind::Limit,
            PhysicalKind::TableScan
        ]
    );
    let r = p.children()[0]
        .as_any()
        .downcast_ref::<PhysicalTableReader>()
        .unwrap();
    let pushed = r.table_plan.as_ref().unwrap();
    let pushed_limit = pushed.as_any().downcast_ref::<PhysicalLimit>().unwrap();
    assert_eq!((pushed_limit.offset, pushed_limit.count), (0, 15));
    assert_eq!(pushed.stats_info().unwrap().row_count, 15.0);
    assert_eq!(
        pushed.schema().len(),
        2,
        "the pushed limit takes its child's schema"
    );
    assert_eq!(r.stats_info().unwrap().row_count, 15.0);

    // With PARTITION BY the root limit is skipped.
    let cop = table_cop_task(&ctx, 1000.0);
    let limit = PhysicalLimit {
        count: 1,
        partition_by: vec![SortItem {
            col: col(1),
            desc: false,
        }],
        ..Default::default()
    }
    .init(&ctx, stats(1.0), 0, Vec::new());
    let t = attach2_task(&ctx, Arc::new(limit), &[Task::Cop(cop)]).unwrap();
    assert_eq!(root_plan(&t).kind(), PhysicalKind::TableReader);

    // An index merge cop task is a boundary stub.
    let cop = CopTask {
        idx_merge_part_plans: Some(vec![table_scan(&ctx, 1.0)]),
        ..Default::default()
    };
    let limit = PhysicalLimit::default().init(&ctx, stats(1.0), 0, Vec::new());
    let err = attach2_task(&ctx, Arc::new(limit), &[Task::Cop(cop)]).unwrap_err();
    assert_eq!(blocked_by(&err), Some("D10.1"));
}

#[test]
fn top_n_attach_pushes_a_top_n_into_the_table_plan() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let cop = table_cop_task(&ctx, 1000.0);
    let req = Arc::new(PhysicalProperty {
        task_tp: CopSingleReadTaskType,
        expected_cnt: f64::MAX,
        ..Default::default()
    });
    let top_n = PhysicalTopN {
        by_items: vec![ByItems {
            expr: col_expr(&col(1)),
            desc: true,
        }],
        offset: 2,
        count: 3,
        ..Default::default()
    }
    .init(&ctx, stats(3.0), 0, vec![Some(req)]);
    let t = attach2_task(&ctx, Arc::new(top_n), &[Task::Cop(cop)]).unwrap();
    let p = root_plan(&t);
    assert_eq!(
        kinds(p),
        [
            PhysicalKind::TopN,
            PhysicalKind::TableReader,
            PhysicalKind::TopN,
            PhysicalKind::TableScan
        ]
    );
    let r = p.children()[0]
        .as_any()
        .downcast_ref::<PhysicalTableReader>()
        .unwrap();
    let pushed = r.table_plan.as_ref().unwrap();
    let pushed_top_n = pushed.as_any().downcast_ref::<PhysicalTopN>().unwrap();
    assert_eq!((pushed_top_n.offset, pushed_top_n.count), (0, 5));
    assert!(pushed_top_n.by_items[0].desc);
    assert_eq!(pushed.stats_info().unwrap().row_count, 5.0);
    assert_eq!(
        pushed.get_child_req_props(0).unwrap().task_tp,
        CopSingleReadTaskType
    );

    // Without columns in the ORDER BY items nothing is pushed.
    let cop = table_cop_task(&ctx, 1000.0);
    let top_n = PhysicalTopN {
        by_items: vec![ByItems {
            expr: Arc::new(Expr::Constant(tidb_expression::constant::new_one())),
            desc: false,
        }],
        count: 3,
        ..Default::default()
    }
    .init(&ctx, stats(3.0), 0, vec![None]);
    let t = attach2_task(&ctx, Arc::new(top_n), &[Task::Cop(cop)]).unwrap();
    assert_eq!(
        kinds(root_plan(&t)),
        [
            PhysicalKind::TopN,
            PhysicalKind::TableReader,
            PhysicalKind::TableScan
        ]
    );
}

#[test]
fn selection_projection_sort_attach() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    // Selection: converted to a root task first.
    let sel = PhysicalSelection::default().init(&ctx, stats(4.0), 0, Vec::new());
    let t = attach2_task(&ctx, Arc::new(sel), &[Task::Cop(table_cop_task(&ctx, 8.0))]).unwrap();
    assert_eq!(
        kinds(root_plan(&t)),
        [
            PhysicalKind::Selection,
            PhysicalKind::TableReader,
            PhysicalKind::TableScan
        ]
    );

    // Projection of columns: pushed into the cop task.
    let mut proj = PhysicalProjection {
        exprs: vec![col_expr(&col(1))],
        ..Default::default()
    }
    .init(&ctx, stats(8.0), 0, Vec::new());
    proj.set_schema(new_schema(vec![col(1)]));
    let t = attach2_task(
        &ctx,
        Arc::new(proj),
        &[Task::Cop(table_cop_task(&ctx, 8.0))],
    )
    .unwrap();
    let Task::Cop(cop) = &t else { panic!("{t:?}") };
    assert_eq!(
        cop.table_plan.as_ref().unwrap().kind(),
        PhysicalKind::Projection
    );

    // Projection over an empty root task keeps it empty.
    let mut root = RootTask::default();
    root.set_plan(table_scan(&ctx, 0.0));
    root.set_empty(true);
    let proj = PhysicalProjection::default().init(&ctx, stats(0.0), 0, Vec::new());
    let t = attach2_task(&ctx, Arc::new(proj), &[Task::Root(root.clone())]).unwrap();
    let Task::Root(r) = &t else { panic!() };
    assert!(r.is_empty());

    // Sort attaches to a copy; a NominalSort of columns only returns the task itself.
    let sort = PhysicalSort::default().init(&ctx, stats(0.0), 0, Vec::new());
    let t = attach2_task(&ctx, Arc::new(sort), &[Task::Root(root.clone())]).unwrap();
    let Task::Root(r) = &t else { panic!() };
    assert_eq!(r.get_plan().unwrap().kind(), PhysicalKind::Sort);
    assert!(!r.is_empty());
    let ns = NominalSort {
        only_column: true,
        ..Default::default()
    }
    .init(&ctx, stats(0.0), 0, Vec::new());
    let t = attach2_task(&ctx, Arc::new(ns), &[Task::Root(root.clone())]).unwrap();
    let Task::Root(r) = &t else { panic!() };
    assert_eq!(r.get_plan().unwrap().kind(), PhysicalKind::TableScan);
    assert!(r.is_empty(), "the task itself, not a copy");
}

#[test]
fn enforce_property_adds_a_root_sort() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let prop = PhysicalProperty {
        task_tp: RootTaskType,
        sort_items: vec![SortItem {
            col: col(2),
            desc: true,
        }],
        ..Default::default()
    };
    let t = enforce_property(&prop, Task::Cop(table_cop_task(&ctx, 7.0)), &ctx).unwrap();
    let p = root_plan(&t);
    assert_eq!(
        kinds(p),
        [
            PhysicalKind::Sort,
            PhysicalKind::TableReader,
            PhysicalKind::TableScan
        ]
    );
    let sort = p.as_any().downcast_ref::<PhysicalSort>().unwrap();
    assert!(sort.by_items[0].desc);
    assert_eq!(p.stats_info().unwrap().row_count, 7.0);
    // No sort items: the task as is.
    let t = enforce_property(
        &PhysicalProperty::default(),
        Task::Cop(table_cop_task(&ctx, 7.0)),
        &ctx,
    )
    .unwrap();
    assert!(matches!(t, Task::Cop(_)));
}

#[test]
fn paging_cost_and_rows() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let mut sel = PhysicalSelection::default().init(&ctx, stats(10.0), 0, Vec::new());
    sel.set_children(vec![table_scan(&ctx, 100.0)]);
    assert_eq!(extract_rows(&sel), 100.0);
    // seekCnt(10) = 1 (minPagingSize 128), selectivity 0.1:
    // max((1*seek + 10*cpu) * 0.1 - seek, 0).
    let seek = ctx.vars.get_seek_factor(None);
    let cpu = ctx.vars.get_cpu_factor();
    let want = ((seek + 10.0 * cpu) * 0.1 - seek).max(0.0);
    assert_eq!(calc_paging_cost(&ctx, &sel, 10), want);
    assert_eq!(calc_paging_cost(&ctx, &sel, 0), 0.0);
}

#[test]
fn match_items_and_order_by_props() {
    let by = vec![
        ByItems {
            expr: col_expr(&col(1)),
            desc: false,
        },
        ByItems {
            expr: col_expr(&col(2)),
            desc: true,
        },
    ];
    let prop = get_prop_by_order_by_items(&by).unwrap();
    assert_eq!(prop.sort_items.len(), 2);
    assert!(match_items(&prop, &by));
    assert!(match_items(&PhysicalProperty::default(), &by));
    let mut prefix = prop.clone();
    prefix.sort_items.truncate(1);
    assert!(match_items(&prefix, &by));
    assert!(!match_items(&prop, &by[..1]));
    let mut wrong = prop.clone();
    wrong.sort_items[1].desc = false;
    assert!(!match_items(&wrong, &by));
    let consts = vec![ByItems {
        expr: Arc::new(Expr::Constant(tidb_expression::constant::new_one())),
        desc: false,
    }];
    assert!(get_prop_by_order_by_items(&consts).is_none());
}

/// A logical `op` over `child`, with `rows` derived rows.
fn with_child(ctx: &mut PlanCtx<'_>, op: Box<dyn LogicalPlan>, child: PlanId, rows: f64) -> PlanId {
    let id = ctx.arena.alloc(op);
    let p = ctx.arena.get_mut(id);
    p.set_children(vec![child]);
    p.base_mut().plan.set_stats(stats(rows));
    id
}

fn dual(ctx: &mut PlanCtx<'_>) -> PlanId {
    let mut d = LogicalTableDual::init(ctx, 0);
    d.sp.set_schema(Some(new_schema(vec![col(1), col(2)])));
    d.base.plan.set_stats(stats(1.0));
    ctx.arena.alloc(Box::new(d))
}

fn data_source(ctx: &mut PlanCtx<'_>) -> PlanId {
    let mut ds = DataSource::init(
        ctx,
        Arc::new(TableInfo::default()),
        CIStr::new("test"),
        Vec::new(),
        new_schema(vec![col(1), col(2)]),
        0,
    );
    ds.possible_access_paths = vec![AccessPath {
        store_type: kv::TiKV,
        ..Default::default()
    }];
    ds.base.plan.set_stats(stats(1000.0));
    ctx.arena.alloc(Box::new(ds))
}

fn task_types(plans: &[PhysicalPlanRef]) -> Vec<(PhysicalKind, isize)> {
    plans
        .iter()
        .map(|p| (p.kind(), p.get_child_req_props(0).unwrap().task_tp.0))
        .collect()
}

#[test]
fn exhaust_limit() {
    let mut vars = session();
    let mut ctx = ctx_of(&mut vars);
    let d = dual(&mut ctx);
    let op = Box::new(LogicalLimit::init(1, 2, &ctx, 0));
    let limit = with_child(&mut ctx, op, d, 1.0);
    let (plans, hint_can_work) =
        exhaust_physical_plans(&mut ctx, limit, &PhysicalProperty::default()).unwrap();
    assert!(hint_can_work);
    // The dual cannot go to TiKV: the root candidate is added; no TiFlash path.
    assert_eq!(
        task_types(&plans),
        [
            (PhysicalKind::Limit, CopSingleReadTaskType.0),
            (PhysicalKind::Limit, CopMultiReadTaskType.0),
            (PhysicalKind::Limit, RootTaskType.0)
        ]
    );
    let l = plans[0].as_any().downcast_ref::<PhysicalLimit>().unwrap();
    assert_eq!((l.offset, l.count), (1, 2));
    assert_eq!(plans[0].get_child_req_props(0).unwrap().expected_cnt, 3.0);
    assert_eq!(plans[0].schema().len(), 2);
    // A sort property cannot be provided by a limit.
    let sorted = PhysicalProperty {
        sort_items: vec![SortItem {
            col: col(1),
            desc: false,
        }],
        ..Default::default()
    };
    let (plans, _) = exhaust_physical_plans(&mut ctx, limit, &sorted).unwrap();
    assert!(plans.is_empty());

    // Over a TiKV data source the limit is always pushed: no root candidate.
    let ds = data_source(&mut ctx);
    let op = Box::new(LogicalLimit::init(0, 2, &ctx, 0));
    let limit = with_child(&mut ctx, op, ds, 2.0);
    let (plans, _) = exhaust_physical_plans(&mut ctx, limit, &PhysicalProperty::default()).unwrap();
    assert_eq!(
        task_types(&plans),
        [
            (PhysicalKind::Limit, CopSingleReadTaskType.0),
            (PhysicalKind::Limit, CopMultiReadTaskType.0)
        ]
    );
}

#[test]
fn exhaust_sort() {
    let mut vars = session();
    let mut ctx = ctx_of(&mut vars);
    let d = dual(&mut ctx);
    let by = vec![ByItems {
        expr: col_expr(&col(1)),
        desc: false,
    }];
    let op = Box::new(LogicalSort::init(by, &ctx, 0));
    let sort = with_child(&mut ctx, op, d, 100.0);
    let prop = PhysicalProperty {
        task_tp: RootTaskType,
        expected_cnt: 10.0,
        ..Default::default()
    };
    let (plans, _) = exhaust_physical_plans(&mut ctx, sort, &prop).unwrap();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].kind(), PhysicalKind::Sort);
    let child = plans[0].get_child_req_props(0).unwrap();
    assert!(child.reject_sort);
    assert_eq!(child.expected_cnt, f64::MAX);
    assert_eq!(plans[0].stats_info().unwrap().row_count, 10.0);
    assert_eq!(plans[1].kind(), PhysicalKind::NominalSort);
    let ns = plans[1].as_any().downcast_ref::<NominalSort>().unwrap();
    assert!(ns.only_column);
    let child = plans[1].get_child_req_props(0).unwrap();
    assert_eq!(child.sort_items.len(), 1);
    assert_eq!(child.expected_cnt, 10.0);
    // A cop property: no candidate.
    let cop = PhysicalProperty {
        task_tp: CopSingleReadTaskType,
        ..Default::default()
    };
    assert!(
        exhaust_physical_plans(&mut ctx, sort, &cop)
            .unwrap()
            .0
            .is_empty()
    );
}

#[test]
fn exhaust_top_n_over_a_data_source() {
    let mut vars = session();
    let mut ctx = ctx_of(&mut vars);
    let ds = data_source(&mut ctx);
    let by = vec![ByItems {
        expr: col_expr(&col(1)),
        desc: false,
    }];
    let op = Box::new(LogicalTopN::init(by, 0, 10, &ctx, 0));
    let top_n = with_child(&mut ctx, op, ds, 10.0);
    let (plans, _) = exhaust_physical_plans(&mut ctx, top_n, &PhysicalProperty::default()).unwrap();
    // TopN: below the push-down threshold (100) and pushable to TiKV: no root candidate; the
    // extra advisory candidate on the multi-read task; MPP allowed by default. Then the
    // limits of the ordered child.
    assert_eq!(
        task_types(&plans),
        [
            (PhysicalKind::TopN, CopSingleReadTaskType.0),
            (PhysicalKind::TopN, CopMultiReadTaskType.0),
            (PhysicalKind::TopN, CopMultiReadTaskType.0),
            (PhysicalKind::TopN, MppTaskType.0),
            (PhysicalKind::Limit, CopSingleReadTaskType.0),
            (PhysicalKind::Limit, CopMultiReadTaskType.0)
        ]
    );
    assert!(
        plans[1]
            .get_child_req_props(0)
            .unwrap()
            .advisory_sort_items
            .is_empty()
    );
    assert_eq!(
        plans[2]
            .get_child_req_props(0)
            .unwrap()
            .advisory_sort_items
            .len(),
        1
    );
    let limit_prop = plans[4].get_child_req_props(0).unwrap();
    assert_eq!(limit_prop.sort_items.len(), 1);
    assert_eq!(limit_prop.expected_cnt, 10.0);
    // A required order the TopN does not give: no candidate.
    let other = PhysicalProperty {
        sort_items: vec![SortItem {
            col: col(2),
            desc: false,
        }],
        ..Default::default()
    };
    assert!(
        exhaust_physical_plans(&mut ctx, top_n, &other)
            .unwrap()
            .0
            .is_empty()
    );
}

#[test]
fn safe_clone_recovers_a_panic() {
    #[derive(Clone, Default)]
    struct Boom {
        base: crate::physicalop::BasePhysicalPlan,
    }
    impl PhysicalPlan for Boom {
        crate::physical_plan_boilerplate!(TableDual);
        fn clone_plan(&self) -> Result<PhysicalPlanRef, Error> {
            panic!("boom");
        }
    }
    let err = safe_clone(&Boom::default()).unwrap_err();
    assert_eq!(err.to_string(), "boom");
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let c = safe_clone(table_scan(&ctx, 1.0).as_ref()).unwrap();
    assert_eq!(c.kind(), PhysicalKind::TableScan);
}

#[test]
fn clones_follow_go() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let mut sel = PhysicalSelection {
        conditions: vec![col_expr(&col(1))],
        from_data_source: true,
        ..Default::default()
    }
    .init(&ctx, stats(1.0), 0, Vec::new());
    sel.set_children(vec![table_scan(&ctx, 1.0)]);
    let c = sel.clone_plan().unwrap();
    let c = c.as_any().downcast_ref::<PhysicalSelection>().unwrap();
    assert!(
        !c.from_data_source,
        "Go's Clone does not copy fromDataSource"
    );
    assert_eq!(c.conditions.len(), 1);
    assert_eq!(c.children().len(), 1);
    assert!(!Arc::ptr_eq(&c.children()[0], &sel.children()[0]));

    let mut reader = PhysicalTableReader {
        table_plan: Some(table_scan(&ctx, 1.0)),
        plan_part_info: Some(Arc::new(PhysPlanPartInfo::default())),
        ..Default::default()
    }
    .init(&ctx, 0);
    reader.set_stats(stats(1.0));
    let c = reader.clone_plan().unwrap();
    let c = c.as_any().downcast_ref::<PhysicalTableReader>().unwrap();
    assert_eq!(c.table_plans.len(), 1);
    assert!(
        c.plan_part_info.is_none(),
        "Go's Clone leaves PlanPartInfo nil"
    );
    assert_eq!(c.schema().len(), 2);
}

#[test]
fn collect_plan_stats_version_reads_the_scans() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let mut ts = PhysicalTableScan {
        table: Arc::new(TableInfo {
            name: CIStr::new("T1"),
            ..Default::default()
        }),
        ..Default::default()
    }
    .init(&ctx, 0);
    ts.set_stats(Some(Arc::new(StatsInfo {
        stats_version: 7,
        ..Default::default()
    })));
    ts.set_schema(Schema::default());
    let reader = PhysicalTableReader {
        table_plan: Some(Arc::new(ts)),
        ..Default::default()
    }
    .init(&ctx, 0);
    let mut m = std::collections::HashMap::new();
    collect_plan_stats_version(&reader, &mut m);
    assert_eq!(m.get("T1"), Some(&7));
}

#[test]
fn group_ranges_by_cols_deterministic_order() {
    use crate::core::find_best_task::group_ranges_by_cols;
    use tidb_sessionctx::ranger::Range;
    use tidb_types::datum::new_int_datum;

    let mk = |a: i64, b: i64| Range {
        low_val: vec![new_int_datum(a), new_int_datum(b)],
        high_val: vec![new_int_datum(a), new_int_datum(b + 10)],
        collators: vec![
            tidb_types::collate::get_binary_collator(),
            tidb_types::collate::get_binary_collator(),
        ],
        ..Default::default()
    };

    // Empty input -> empty output.
    assert!(group_ranges_by_cols(&[], &[0]).unwrap().is_empty());

    // Multi-group deterministic first-appearance order: 2, 1, 3.
    let ranges = vec![mk(2, 10), mk(1, 5), mk(2, 20), mk(3, 1), mk(1, 7)];
    let groups = group_ranges_by_cols(&ranges, &[0]).unwrap();
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].len(), 2);
    assert_eq!(groups[0][0].low_val[0].get_int64(), 2);
    assert_eq!(groups[0][0].low_val[1].get_int64(), 10);
    assert_eq!(groups[0][1].low_val[1].get_int64(), 20);
    assert_eq!(groups[1].len(), 2);
    assert_eq!(groups[1][0].low_val[0].get_int64(), 1);
    assert_eq!(groups[1][0].low_val[1].get_int64(), 5);
    assert_eq!(groups[1][1].low_val[1].get_int64(), 7);
    assert_eq!(groups[2].len(), 1);
    assert_eq!(groups[2][0].low_val[0].get_int64(), 3);

    // Grouping by multiple columns [0, 1] separates distinct (a, b) pairs.
    let multi = group_ranges_by_cols(&[mk(2, 10), mk(2, 20), mk(2, 10)], &[0, 1]).unwrap();
    assert_eq!(multi.len(), 2);
    assert_eq!(multi[0].len(), 2);
    assert_eq!(multi[1].len(), 1);

    // Single group preserves all ranges in order.
    let single = group_ranges_by_cols(&[mk(5, 1), mk(5, 2)], &[0]).unwrap();
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].len(), 2);
}

#[test]
fn adjust_row_count_for_index_scan_by_limit_matches_go() {
    use crate::cardinality::cross_estimation::adjust_row_count_for_index_scan_by_limit;
    use crate::util::AccessPath;
    use tidb_statistics::table::pseudo_table;

    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let tbl_info = TableInfo {
        id: 1,
        name: CIStr::new("t"),
        ..Default::default()
    };
    let stats_tbl = pseudo_table(&tbl_info, false, false);
    let ds_stats = |rows: f64| StatsInfo {
        row_count: rows,
        ..Default::default()
    };

    // Normal adjustment without filters: min(100, 10) = 10.
    let path = AccessPath {
        count_after_access: 100.0,
        ..Default::default()
    };
    let got = adjust_row_count_for_index_scan_by_limit(
        &ctx,
        &ds_stats(100.0),
        &ds_stats(100.0),
        &stats_tbl,
        &path,
        10.0,
        false,
    )
    .unwrap();
    assert_eq!(got, 10.0);

    // 0/0 produces NaN; Go's math.Min(NaN, 0.0) returns NaN (not 0.0 like Rust f64::min).
    let zero_path = AccessPath {
        count_after_access: 0.0,
        ..Default::default()
    };
    let got_nan = adjust_row_count_for_index_scan_by_limit(
        &ctx,
        &ds_stats(0.0),
        &ds_stats(0.0),
        &stats_tbl,
        &zero_path,
        10.0,
        false,
    )
    .unwrap();
    assert!(
        got_nan.is_nan(),
        "Go math.Min(NaN, 0) must return NaN, got {got_nan}"
    );

    // With table filters and positive order_ratio: exercises the M-FMA branch.
    let filter_path = AccessPath {
        count_after_access: 1000.0,
        count_after_index: 500.0,
        table_filters: vec![col_expr(&col(1))],
        ..Default::default()
    };
    let mut vars_ratio = session();
    vars_ratio.correlation_exp_factor = 0;
    vars_ratio.opt_ordering_idx_sel_ratio = 0.25;
    let ctx_ratio = ctx_of(&mut vars_ratio);
    let got_fma = adjust_row_count_for_index_scan_by_limit(
        &ctx_ratio,
        &ds_stats(100.0),
        &ds_stats(1000.0),
        &stats_tbl,
        &filter_path,
        10.0,
        false,
    )
    .unwrap();
    // sel = 100 / 1000 = 0.1; base = min(10 / 0.1 / 1.0, 1000) = 100;
    // row_count += (1000 - 100) * 0.25 = 100 + 225 = 325.
    assert!((got_fma - 325.0).abs() < 1e-9, "got {got_fma}");
}

#[test]
fn physical_index_scan_to_pb_and_check_cover_index() {
    use crate::core::plan_to_pb::check_cover_index;
    use tidb_model::{IndexColumn, IndexInfo};
    use tidb_parser::types::UnspecifiedLength;
    use tidb_sessionctx::planctx::BuildPBContext;
    use tidb_sessionctx::ranger::Range;
    use tidb_types::datum::{Datum, new_int_datum};

    let mut vars = session();
    let ctx = ctx_of(&mut vars);

    let idx_info = Arc::new(IndexInfo {
        id: 2,
        name: CIStr::new("uk_a"),
        unique: true,
        columns: Some(vec![IndexColumn {
            name: CIStr::new("a"),
            offset: 1,
            length: UnspecifiedLength,
            ..Default::default()
        }]),
        ..Default::default()
    });
    let non_unique_idx = IndexInfo {
        unique: false,
        ..(*idx_info).clone()
    };
    // 2-column unique index queried by only 1 column (prefix of composite unique key) -> false.
    let composite_uk = IndexInfo {
        columns: Some(vec![
            IndexColumn {
                name: CIStr::new("a"),
                offset: 1,
                length: UnspecifiedLength,
                ..Default::default()
            },
            IndexColumn {
                name: CIStr::new("b"),
                offset: 2,
                length: UnspecifiedLength,
                ..Default::default()
            },
        ]),
        ..(*idx_info).clone()
    };

    let pt_range = Range {
        low_val: vec![new_int_datum(5)],
        high_val: vec![new_int_datum(5)],
        collators: vec![tidb_types::collate::get_binary_collator()],
        ..Default::default()
    };
    let non_pt_range = Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(10)],
        collators: vec![tidb_types::collate::get_binary_collator()],
        ..Default::default()
    };
    let null_range = Range {
        low_val: vec![Datum::default()],
        high_val: vec![new_int_datum(5)],
        collators: vec![tidb_types::collate::get_binary_collator()],
        ..Default::default()
    };
    let high_null_range = Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![Datum::default()],
        collators: vec![tidb_types::collate::get_binary_collator()],
        ..Default::default()
    };

    assert!(!check_cover_index(
        &non_unique_idx,
        std::slice::from_ref(&pt_range)
    ));
    assert!(check_cover_index(
        &idx_info,
        std::slice::from_ref(&pt_range)
    ));
    assert!(check_cover_index(&idx_info, &[non_pt_range]));
    assert!(!check_cover_index(
        &composite_uk,
        std::slice::from_ref(&pt_range)
    ));
    assert!(!check_cover_index(&idx_info, &[null_range]));
    assert!(!check_cover_index(&idx_info, &[high_null_range]));

    // ToPB wire bytes: ExecutorId must remain unset (None), fine-grained shuffle fields set to 0.
    let col_a_info = Arc::new(ColumnInfo {
        id: 2,
        name: CIStr::new("a"),
        state: tidb_model::StatePublic,
        field_type: new_field_type(TypeLonglong),
        ..Default::default()
    });
    let mut is = PhysicalIndexScan {
        table: Arc::new(TableInfo {
            id: 42,
            name: CIStr::new("t"),
            pk_is_handle: true,
            columns: Some(vec![Arc::clone(&col_a_info)]),
            ..Default::default()
        }),
        index: Arc::clone(&idx_info),
        columns: vec![col_a_info],
        ranges: vec![pt_range],
        desc: true,
        ..Default::default()
    }
    .init(&ctx, 0);
    is.set_schema(new_schema(vec![col(2)]));

    let expr_ctx: tidb_sessionctx::exprctx::BuildContextRef =
        Arc::<dyn tidb_expression::context::AggFuncBuildContext>::clone(&ctx.expr_ctx);
    let bctx = BuildPBContext {
        expr_ctx,
        client: Arc::new(MockClient),
        tiflash_fast_scan: false,
        tiflash_fine_grained_shuffle_batch_size: 0,
        group_concat_max_len: 1024,
        in_explain_stmt: false,
        warn_handler: None,
        extra_warnghandler: None,
    };
    let exec = is.to_pb(&bctx, kv::TiKV).unwrap();
    assert_eq!(exec.get_tp(), tipb::ExecType::TypeIndexScan);
    assert!(
        !exec.has_executor_id(),
        "PhysicalIndexScan::to_pb must leave executor_id unset"
    );
    assert_eq!(exec.get_fine_grained_shuffle_stream_count(), 0);
    assert_eq!(exec.get_fine_grained_shuffle_batch_size(), 0);
    let idx_pb = exec.get_idx_scan();
    assert_eq!(idx_pb.get_table_id(), 42);
    assert_eq!(idx_pb.get_index_id(), 2);
    assert!(idx_pb.get_desc());
    assert!(idx_pb.get_unique());
}

#[test]
fn physical_index_reader_memory_usage_and_resolve_indices() {
    let mut vars = session();
    let ctx = ctx_of(&mut vars);

    let c1 = col(10);
    let c2 = col(20);
    let mut is = PhysicalIndexScan {
        table: Arc::new(TableInfo {
            id: 7,
            name: CIStr::new("t"),
            ..Default::default()
        }),
        data_source_schema: Some(new_schema(vec![Arc::clone(&c1)])),
        ..Default::default()
    }
    .init(&ctx, 0);
    is.set_schema(new_schema(vec![Arc::clone(&c1), Arc::clone(&c2)]));
    is.set_stats(Some(Arc::new(StatsInfo {
        stats_version: 11,
        ..Default::default()
    })));
    let scan_mem = is.memory_usage();
    let is_ref: PhysicalPlanRef = Arc::new(is);

    // MemoryUsage matches Go physical_plans.go:428-445: sums sp + IndexPlans + OutputColumns
    // (discarding index_plan.memory_usage()). PhysicalIndexReader::init sets schema and
    // output_columns from is.data_source_schema.
    let mut reader = PhysicalIndexReader {
        index_plan: Some(Arc::clone(&is_ref)),
        ..Default::default()
    }
    .init(&ctx, 0);
    assert_eq!(reader.output_columns.len(), 1);
    let base_mem = reader.sp.memory_usage() + reader.output_columns[0].memory_usage();
    assert_eq!(reader.memory_usage(), base_mem + scan_mem);
    reader.index_plans.clear();
    assert_eq!(reader.memory_usage(), base_mem);

    // collect_plan_stats_version walks IndexReader -> IndexScan.
    let mut ver_map = std::collections::HashMap::new();
    collect_plan_stats_version(&reader, &mut ver_map);
    assert_eq!(ver_map.get("t"), Some(&11));

    // ResolveIndices: when index_plan is IndexScan, reader schema Index is NOT copied from
    // index_plan; output_columns resolves via virtual_expr fallback when a duplicate virtual
    // expression column (ScalarFunction) exists in index_plan.schema().
    let virt_fn = tidb_expression::scalar_function::new_function(
        &*ctx.expr_ctx,
        tidb_expression::scalar_function::ast_func::Plus,
        new_field_type(TypeLonglong),
        vec![
            col_expr(&c1),
            Arc::new(Expr::Constant(tidb_expression::new_one())),
        ],
    )
    .unwrap();
    let mut idx_virt_col = (*col(20)).clone();
    idx_virt_col.virtual_expr = Some(Arc::clone(&virt_fn));
    let mut is_virt = PhysicalIndexScan {
        table: Arc::new(TableInfo {
            id: 7,
            name: CIStr::new("t"),
            ..Default::default()
        }),
        data_source_schema: Some(new_schema(vec![Arc::clone(&c1)])),
        ..Default::default()
    }
    .init(&ctx, 0);
    is_virt.set_schema(new_schema(vec![Arc::clone(&c1), Arc::new(idx_virt_col)]));
    let mut out_virt_col = (*col(99)).clone();
    out_virt_col.virtual_expr = Some(virt_fn);
    let mut r_scan = PhysicalIndexReader {
        index_plan: Some(Arc::new(is_virt)),
        ..Default::default()
    }
    .init(&ctx, 0);
    r_scan.output_columns = vec![Arc::new(out_virt_col)];
    let mut s_col = (*c1).clone();
    s_col.index = 7;
    r_scan
        .sp
        .set_schema(Some(new_schema(vec![Arc::new(s_col)])));
    r_scan.resolve_indices(&ctx).unwrap();
    assert_eq!(r_scan.index_plans.len(), 1);
    assert_eq!(r_scan.schema().columns[0].index, 7);
    assert_eq!(
        r_scan.output_columns[0].index, 1,
        "virtual_expr fallback resolves to duplicate column at index 1 in index_plan schema"
    );

    // ResolveIndices: when index_plan is Projection, reader schema copies resolved Index.
    let mut proj = PhysicalProjection {
        exprs: vec![col_expr(&c2)],
        ..Default::default()
    }
    .init(&ctx, stats(1.0), 0, Vec::new());
    proj.set_children(vec![Arc::clone(&is_ref)]);
    proj.set_schema(new_schema(vec![Arc::clone(&c2)]));
    let mut r_proj = PhysicalIndexReader {
        index_plan: Some(Arc::new(proj)),
        ..Default::default()
    }
    .init(&ctx, 0);
    let mut r_proj_col = (*c2).clone();
    r_proj_col.index = 99;
    r_proj
        .sp
        .set_schema(Some(new_schema(vec![Arc::new(r_proj_col)])));
    r_proj.resolve_indices(&ctx).unwrap();
    assert_eq!(r_proj.schema().columns[0].index, 0);
    assert_eq!(r_proj.output_columns[0].index, 0);
}

#[test]
fn is_point_get_with_pk_or_unique_key_by_auto_commit_readers() {
    use crate::core::common_plans::is_point_get_with_pk_or_unique_key_by_auto_commit;
    use tidb_model::{IndexColumn, IndexInfo};
    use tidb_parser::types::UnspecifiedLength;
    use tidb_sessionctx::ranger::Range;
    use tidb_types::datum::new_int_datum;

    let mut vars = session();
    vars.set_status_flag(tidb_base::mysql::ServerStatusAutocommit, true);
    let ctx = ctx_of(&mut vars);

    let pt = Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(1)],
        collators: vec![tidb_types::collate::get_binary_collator()],
        ..Default::default()
    };
    let rng = Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(2)],
        collators: vec![tidb_types::collate::get_binary_collator()],
        ..Default::default()
    };
    let uk = Arc::new(IndexInfo {
        unique: true,
        columns: Some(vec![IndexColumn {
            name: CIStr::new("a"),
            offset: 0,
            length: UnspecifiedLength,
            ..Default::default()
        }]),
        ..Default::default()
    });
    let non_uk = Arc::new(IndexInfo {
        unique: false,
        ..(*uk).clone()
    });

    // Unique index point scan -> true; non-unique -> false; unique range -> false.
    let mk_idx_reader = |idx: Arc<IndexInfo>, ranges: Vec<Range>| -> PhysicalPlanRef {
        let mut is = PhysicalIndexScan {
            index: idx,
            ranges,
            ..Default::default()
        }
        .init(&ctx, 0);
        is.set_schema(new_schema(vec![col(1)]));
        Arc::new(
            PhysicalIndexReader {
                index_plan: Some(Arc::new(is)),
                ..Default::default()
            }
            .init(&ctx, 0),
        )
    };
    let uk_pt_reader = mk_idx_reader(Arc::clone(&uk), vec![pt.clone()]);
    assert!(is_point_get_with_pk_or_unique_key_by_auto_commit(ctx.vars, &*uk_pt_reader).unwrap());
    let non_uk_reader = mk_idx_reader(non_uk, vec![pt.clone()]);
    assert!(!is_point_get_with_pk_or_unique_key_by_auto_commit(ctx.vars, &*non_uk_reader).unwrap());
    let uk_rng_reader = mk_idx_reader(uk, vec![rng.clone()]);
    assert!(!is_point_get_with_pk_or_unique_key_by_auto_commit(ctx.vars, &*uk_rng_reader).unwrap());

    // Projection wrapping unique IndexReader -> true.
    let mut proj = PhysicalProjection::default().init(&ctx, stats(1.0), 0, Vec::new());
    proj.set_children(vec![Arc::clone(&uk_pt_reader)]);
    assert!(is_point_get_with_pk_or_unique_key_by_auto_commit(ctx.vars, &proj).unwrap());

    // TableReader: int PK point -> true; range -> false; 2-col common handle with 1-col point -> false.
    let mk_tbl_reader = |common_handle: bool, ranges: Vec<Range>| -> PhysicalPlanRef {
        let indices = if common_handle {
            Some(vec![Arc::new(IndexInfo {
                primary: true,
                unique: true,
                columns: Some(vec![
                    IndexColumn {
                        name: CIStr::new("a"),
                        offset: 0,
                        length: UnspecifiedLength,
                        ..Default::default()
                    },
                    IndexColumn {
                        name: CIStr::new("b"),
                        offset: 1,
                        length: UnspecifiedLength,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            })])
        } else {
            None
        };
        let mut ts = PhysicalTableScan {
            table: Arc::new(TableInfo {
                pk_is_handle: !common_handle,
                is_common_handle: common_handle,
                indices,
                ..Default::default()
            }),
            ranges,
            ..Default::default()
        }
        .init(&ctx, 0);
        ts.set_schema(new_schema(vec![col(1)]));
        Arc::new(
            PhysicalTableReader {
                table_plan: Some(Arc::new(ts)),
                ..Default::default()
            }
            .init(&ctx, 0),
        )
    };
    assert!(
        is_point_get_with_pk_or_unique_key_by_auto_commit(
            ctx.vars,
            &*mk_tbl_reader(false, vec![pt.clone()])
        )
        .unwrap()
    );
    assert!(
        !is_point_get_with_pk_or_unique_key_by_auto_commit(
            ctx.vars,
            &*mk_tbl_reader(false, vec![rng])
        )
        .unwrap()
    );
    assert!(
        !is_point_get_with_pk_or_unique_key_by_auto_commit(
            ctx.vars,
            &*mk_tbl_reader(true, vec![pt])
        )
        .unwrap()
    );
    let pt2 = Range {
        low_val: vec![new_int_datum(1), new_int_datum(2)],
        high_val: vec![new_int_datum(1), new_int_datum(2)],
        collators: vec![
            tidb_types::collate::get_binary_collator(),
            tidb_types::collate::get_binary_collator(),
        ],
        ..Default::default()
    };
    assert!(
        is_point_get_with_pk_or_unique_key_by_auto_commit(
            ctx.vars,
            &*mk_tbl_reader(true, vec![pt2])
        )
        .unwrap()
    );
}

#[test]
fn match_property_group_ranges_guards() {
    use crate::core::find_best_task::match_property;
    use crate::property::{
        PhysicalProperty, PropMatched, PropMatchedNeedMergeSort, PropNotMatched, SortItem,
    };
    use crate::util::path::AccessPath;
    use tidb_parser::types::UnspecifiedLength;
    use tidb_sessionctx::ranger::Range;
    use tidb_types::datum::new_int_datum;

    let mut vars = session();
    let ctx = ctx_of(&mut vars);
    let c_a = col(1);
    let c_b = col(2);
    let ds = DataSource::init(
        &ctx,
        Arc::new(TableInfo::default()),
        CIStr::new("test"),
        Vec::new(),
        new_schema(vec![Arc::clone(&c_a), Arc::clone(&c_b)]),
        0,
    );
    let prop = PhysicalProperty {
        sort_items: vec![SortItem {
            col: Arc::clone(&c_b),
            desc: false,
        }],
        ..Default::default()
    };
    let make_path = |n_points: usize| -> AccessPath {
        let mut ranges = Vec::with_capacity(n_points);
        for i in 0..n_points {
            let d = new_int_datum((i * 2) as i64);
            ranges.push(Range {
                low_val: vec![d.clone(), new_int_datum(0)],
                high_val: vec![d, new_int_datum(100)],
                collators: vec![
                    tidb_types::collate::get_binary_collator(),
                    tidb_types::collate::get_binary_collator(),
                ],
                ..Default::default()
            });
        }
        AccessPath {
            ranges,
            idx_cols: vec![Arc::clone(&c_a), Arc::clone(&c_b)],
            idx_col_lens: vec![UnspecifiedLength, UnspecifiedLength],
            eq_cond_count: 0,
            ..Default::default()
        }
    };

    // Empty ranges: trivially all-point loop, but Go keeps PropMatched without setting group fields.
    let mut empty = make_path(0);
    assert_eq!(
        match_property(&ds, &ctx, &mut empty, &prop).unwrap(),
        PropMatched
    );
    assert!(empty.grouped_ranges.is_empty());
    assert!(empty.group_by_col_idxs.is_empty());

    // 2048 distinct groups: matched with merge sort.
    let mut at_cap = make_path(2048);
    assert_eq!(
        match_property(&ds, &ctx, &mut at_cap, &prop).unwrap(),
        PropMatchedNeedMergeSort
    );
    assert_eq!(at_cap.grouped_ranges.len(), 2048);
    assert_eq!(at_cap.group_by_col_idxs, vec![0]);

    // 2049 distinct groups: exceeds maxGroupCount (2048) -> PropNotMatched, fields untouched.
    let mut over_cap = make_path(2049);
    assert_eq!(
        match_property(&ds, &ctx, &mut over_cap, &prop).unwrap(),
        PropNotMatched
    );
    assert!(over_cap.grouped_ranges.is_empty());
    assert!(over_cap.group_by_col_idxs.is_empty());
}
