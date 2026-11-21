//! Spike VS1-26 (N1 §3.2 / §7 item 3): does the N1 representation carry Go's `FindBestTask`
//! memo (`taskMap` / `GetTask` / `StoreTask` / `RollBackTaskMap`) with Go's sharing, and does
//! cost calculation copy shared nodes?
//!
//! The harness is a mock of `findBestTask` / `enumeratePhysicalPlans4Task` /
//! `compareTaskCost` (find_best_task.go L505-605, L188-268, L355-371) over the real VS1
//! operators (`exhaust_physical_plans`, `attach2_task`, `enforce_property`, the
//! `Mutex<PlanCostCache>` cost memo). `DataSource` is a mock leaf with its own memo entry
//! (findBestTask4DataSource L1792-1844). The memo is the N1 §3.2 side table keyed by
//! `(PlanId, prop.HashCode())`. `clone_node` calls are counted by `CLONE_NODE_CALLS`
//! (`make_mut_plan` is its only caller).
//!
//! Not a port of FindBestTask: VS1-28 ports it.

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::kv;
use tidb_base::mysql::TypeLonglong;
use tidb_expression::{Column, Expr, ExprRef, new_schema};
use tidb_model::{ColumnInfo, TableInfo};
use tidb_parser::model::CIStr;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_type::new_field_type;

use crate::base::{LogicalKind, PhysicalPlan, exhaust_physical_plans};
use crate::core::physical_plans::*;
use crate::core::plan::enforce_property;
use crate::core::task::attach2_task;
use crate::core::task_base::{CopTask, Task};
use crate::logicalop::{DataSource, LogicalLimit, LogicalTopN};
use crate::physicalop::{CLONE_NODE_CALLS, PhysicalPlanRef, make_mut_plan, map_children};
use crate::property::{CopSingleReadTaskType, PhysicalProperty, RootTaskType, StatsInfo};
use crate::util::optimizetrace::new_default_plan_cost_option;
use crate::util::{AccessPath, ByItems};
use crate::{PlanCtx, PlanId};

// ---------------------------------------------------------------------------------------
// fixtures (as physical_plans_tests.rs)

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

fn session() -> SessionVars {
    SessionVars::new(Box::new(Acc))
}

/// Go's `mock.Client`.
struct MockClient;

impl tidb_kv::kv::Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &tidb_kv::kv::Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &tidb_kv::kv::ClientSendOption,
    ) -> Box<dyn tidb_kv::kv::Response> {
        unreachable!("MockClient.send")
    }

    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool {
        tidb_kv::checker::RequestTypeSupportedChecker.is_request_type_supported(req_type, sub_type)
    }
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

fn over(
    ctx: &mut PlanCtx<'_>,
    op: Box<dyn crate::base::LogicalPlan>,
    child: PlanId,
    rows: f64,
) -> PlanId {
    let id = ctx.arena.alloc(op);
    let p = ctx.arena.get_mut(id);
    p.set_children(vec![child]);
    p.base_mut().plan.set_stats(stats(rows));
    id
}

fn clones() -> usize {
    CLONE_NODE_CALLS.with(|c| c.get())
}

fn reset_clones() {
    CLONE_NODE_CALLS.with(|c| c.set(0));
}

fn root_prop() -> Arc<PhysicalProperty> {
    Arc::new(PhysicalProperty {
        task_tp: RootTaskType,
        expected_cnt: f64::MAX,
        ..Default::default()
    })
}

/// The physical children of a node, with a reader's pushed-down plan (Go's `tablePlan`) as
/// its child.
fn phys_children(p: &PhysicalPlanRef) -> Vec<PhysicalPlanRef> {
    let mut out: Vec<PhysicalPlanRef> = p.children().to_vec();
    if let Some(r) = p.as_any().downcast_ref::<PhysicalTableReader>()
        && let Some(tp) = &r.table_plan
    {
        out.push(Arc::clone(tp));
    }
    out
}

fn all_nodes(p: &PhysicalPlanRef, out: &mut Vec<PhysicalPlanRef>) {
    out.push(Arc::clone(p));
    for c in phys_children(p) {
        all_nodes(&c, out);
    }
}

fn contains_ptr(root: &PhysicalPlanRef, target: &PhysicalPlanRef) -> bool {
    let mut nodes = Vec::new();
    all_nodes(root, &mut nodes);
    nodes.iter().any(|n| Arc::ptr_eq(n, target))
}

/// The plans of a task (both sides of a cop task).
fn task_plans(t: &Task) -> Vec<PhysicalPlanRef> {
    match t {
        Task::Root(r) => r.get_plan().into_iter().cloned().collect(),
        Task::Cop(c) => c
            .index_plan
            .iter()
            .chain(c.table_plan.iter())
            .cloned()
            .collect(),
        Task::Mpp(m) => m.p.iter().cloned().collect(),
    }
}

// ---------------------------------------------------------------------------------------
// the memo: N1 §3.2 side table, Go's taskMap / taskMapBak / taskMapBakTS

#[derive(Default)]
struct Memo {
    /// `taskMap` of each logical node; `None` is Go's rolled-back `nil` entry.
    task_map: HashMap<(PlanId, Vec<u8>), Option<Task>>,
    /// `taskMapBak` / `taskMapBakTS` of each logical node.
    bak: HashMap<PlanId, Vec<(Vec<u8>, u64)>>,
    // observations
    hits: usize,
    stores: usize,
    /// (child plan of a memo hit, it is the same `Arc` as the stored one)
    hit_shares_stored: Vec<bool>,
    /// per attached candidate: every child task's plan is in the candidate's tree by pointer
    candidate_child_shared: Vec<bool>,
    /// clone_node calls during compareTaskCost only
    clones_in_cost: usize,
    cost_calls: usize,
    /// Port the enumerate loop the wrong way (`Arc::clone(pp)` while the Vec holds it).
    clone_candidates: bool,
}

impl Memo {
    /// GetLogicalTS4TaskMap (base_logical_plan.go L347).
    fn get_logical_ts(ctx: &mut PlanCtx<'_>) -> u64 {
        let sc = ctx.vars.stmt_ctx_mut();
        sc.task_map_bak_ts += 1;
        sc.task_map_bak_ts
    }

    /// GetTask (L353).
    fn get_task(&mut self, p: PlanId, prop: &PhysicalProperty) -> Option<Task> {
        let t = self
            .task_map
            .get(&(p, prop.hash_code().to_vec()))
            .and_then(|t| t.clone());
        if let Some(t) = &t {
            self.hits += 1;
            let stored = self.task_map[&(p, prop.hash_code().to_vec())]
                .as_ref()
                .unwrap();
            let same = task_plans(t)
                .iter()
                .zip(task_plans(stored).iter())
                .all(|(a, b)| Arc::ptr_eq(a, b));
            self.hit_shares_stored.push(same);
        }
        t
    }

    /// StoreTask (L359).
    fn store_task(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        prop: &PhysicalProperty,
        task: Task,
    ) {
        let key = prop.hash_code().to_vec();
        if ctx.vars.stmt_ctx().stmt_hints.task_map_need_back_up() {
            let ts = Self::get_logical_ts(ctx);
            self.bak.entry(p).or_default().push((key.clone(), ts));
        }
        self.stores += 1;
        self.task_map.insert((p, key), Some(task));
    }

    /// RollBackTaskMap (L279).
    fn roll_back(&mut self, ctx: &PlanCtx<'_>, p: PlanId, ts: u64) {
        if !ctx.vars.stmt_ctx().stmt_hints.task_map_need_back_up() {
            return;
        }
        if let Some(logs) = self.bak.get_mut(&p) {
            let mut kept = Vec::new();
            for (key, t) in logs.drain(..) {
                if t < ts {
                    kept.push((key, t));
                    continue;
                }
                self.task_map.insert((p, key), None);
            }
            *logs = kept;
        }
        for child in ctx.arena.get(p).children().to_vec() {
            self.roll_back(ctx, child, ts);
        }
    }
}

// ---------------------------------------------------------------------------------------
// the mock findBestTask

fn get_task_plan_cost(t: &Task) -> Result<f64, Error> {
    if t.invalid() {
        return Ok(f64::MAX);
    }
    let opt = new_default_plan_cost_option();
    let (plan, tp) = match t {
        Task::Root(r) => (r.get_plan().unwrap(), RootTaskType),
        Task::Cop(c) => (c.table_plan.as_ref().unwrap(), CopSingleReadTaskType),
        Task::Mpp(m) => (m.p.as_ref().unwrap(), RootTaskType),
    };
    Ok(plan.get_plan_cost_ver2(tp, &opt)?.get_cost())
}

/// compareTaskCost (L355), counting clone_node in it.
fn compare_task_cost(memo: &mut Memo, cur: &Task, best: &Task) -> Result<bool, Error> {
    let before = clones();
    memo.cost_calls += 1;
    let cur_cost = get_task_plan_cost(cur)?;
    let best_cost = get_task_plan_cost(best)?;
    memo.clones_in_cost += clones() - before;
    if cur.invalid() {
        return Ok(false);
    }
    if best.invalid() {
        return Ok(true);
    }
    Ok(cur_cost < best_cost)
}

/// A mock of findBestTask4DataSource: a single table path, no index, so no order.
fn find_best_task_ds(
    ctx: &mut PlanCtx<'_>,
    memo: &mut Memo,
    p: PlanId,
    prop: &PhysicalProperty,
) -> Result<Task, Error> {
    if let Some(t) = memo.get_task(p, prop) {
        return Ok(t);
    }
    let t = if !prop.is_sort_item_empty() {
        Task::invalid_task()
    } else if prop.task_tp == CopSingleReadTaskType || prop.task_tp == RootTaskType {
        let rows = ctx.arena.get(p).base().plan.stats_info().unwrap().row_count;
        let cop = Task::Cop(CopTask {
            table_plan: Some(table_scan(ctx, rows)),
            index_plan_finished: true,
            ..Default::default()
        });
        if prop.task_tp == RootTaskType {
            cop.convert_to_root_task(ctx)?
        } else {
            cop
        }
    } else {
        Task::invalid_task()
    };
    memo.store_task(ctx, p, prop, t.clone());
    Ok(t)
}

/// findBestTask (L505).
fn find_best_task(
    ctx: &mut PlanCtx<'_>,
    memo: &mut Memo,
    p: PlanId,
    prop: &Arc<PhysicalProperty>,
) -> Result<Task, Error> {
    if ctx.arena.get(p).kind() == LogicalKind::DataSource {
        return find_best_task_ds(ctx, memo, p, prop);
    }
    if let Some(t) = memo.get_task(p, prop) {
        return Ok(t);
    }
    if prop.task_tp != RootTaskType && !prop.is_flash_prop() {
        memo.store_task(ctx, p, prop, Task::invalid_task());
        return Ok(Task::invalid_task());
    }
    let mut can_add_enforcer = prop.can_add_enforcer;
    let mut new_prop = prop.clone_essential_fields();
    let (mut fits, hint_works_with_prop) = exhaust_physical_plans(ctx, p, &new_prop)?;
    if !hint_works_with_prop && !new_prop.is_sort_item_empty() {
        can_add_enforcer = true;
    }
    let mut need_enforce = Vec::new();
    let enum_prop: Arc<PhysicalProperty>;
    if can_add_enforcer {
        new_prop.sort_items = Vec::new();
        new_prop.sort_items_for_partition = Vec::new();
        new_prop.expected_cnt = f64::MAX;
        new_prop.mpp_partition_cols = Vec::new();
        let (plans, hint_can_work) = exhaust_physical_plans(ctx, p, &new_prop)?;
        need_enforce = plans;
        if hint_can_work && !hint_works_with_prop {
            fits = Vec::new();
        }
        if !hint_can_work && !hint_works_with_prop && !prop.can_add_enforcer {
            need_enforce = Vec::new();
        }
        enum_prop = Arc::clone(prop);
    } else {
        enum_prop = Arc::new(new_prop);
    }
    let mut best = enumerate(ctx, memo, p, fits, &enum_prop, false)?;
    let cur = enumerate(ctx, memo, p, need_enforce, &enum_prop, true)?;
    if compare_task_cost(memo, &cur, &best)? {
        best = cur;
    }
    memo.store_task(ctx, p, prop, best.clone());
    Ok(best)
}

/// enumeratePhysicalPlans4Task (L188) with iteratePhysicalPlan4BaseLogical (L271).
fn enumerate(
    ctx: &mut PlanCtx<'_>,
    memo: &mut Memo,
    p: PlanId,
    physical_plans: Vec<PhysicalPlanRef>,
    prop: &Arc<PhysicalProperty>,
    add_enforcer: bool,
) -> Result<Task, Error> {
    let mut best = Task::invalid_task();
    let children = ctx.arena.get(p).children().to_vec();
    // The right port moves each candidate out of the Vec; the wrong one keeps the Vec alive
    // and clones the Arc (refcount 2 at attach time).
    let held = if memo.clone_candidates {
        physical_plans.clone()
    } else {
        Vec::new()
    };
    for pp in physical_plans {
        let _ts_now = Memo::get_logical_ts(ctx);
        let mut child_tasks = Vec::with_capacity(children.len());
        let mut ok = true;
        for (j, child) in children.iter().enumerate() {
            let Some(child_prop) = pp.get_child_req_props(j as isize).cloned() else {
                ok = false;
                break;
            };
            let t = find_best_task(ctx, memo, *child, &child_prop)?;
            if t.invalid() {
                ok = false;
                break;
            }
            child_tasks.push(t);
        }
        if !ok {
            continue;
        }
        let child_plans: Vec<PhysicalPlanRef> = child_tasks.iter().flat_map(task_plans).collect();
        let mut cur = attach2_task(ctx, pp, &child_tasks)?;
        if cur.invalid() {
            continue;
        }
        if !matches!(cur, Task::Root(_)) && prop.task_tp == RootTaskType {
            cur = cur.convert_to_root_task(ctx)?;
        }
        if add_enforcer {
            cur = enforce_property(prop, cur, ctx)?;
        }
        let roots = task_plans(&cur);
        memo.candidate_child_shared.push(
            child_plans
                .iter()
                .all(|c| roots.iter().any(|r| contains_ptr(r, c))),
        );
        if compare_task_cost(memo, &cur, &best)? {
            best = cur;
        }
    }
    drop(held);
    Ok(best)
}

/// The shape of postOptimize: every node written through `make_mut_plan` (`SetID` here),
/// children through `map_children`.
fn post_optimize_like(p: &mut PhysicalPlanRef, reflatten: bool) {
    let id = p.id();
    make_mut_plan(p).set_id(id);
    map_children(p, |mut c| {
        post_optimize_like(&mut c, reflatten);
        c
    });
    if p.kind() == crate::base::PhysicalKind::TableReader {
        let r = make_mut_plan(p)
            .as_any_mut()
            .downcast_mut::<PhysicalTableReader>()
            .unwrap();
        if reflatten {
            // Proposed rule: drop the flattened copy (Go's TablePlans, the same pointers as
            // the tablePlan chain) before writing the table plan, flatten again after, as
            // PhysicalTableReader.Init already does around setMppOrBatchCopForTableScan.
            r.table_plans.clear();
        }
        if let Some(tp) = r.table_plan.as_mut() {
            post_optimize_like(tp, reflatten);
        }
        if reflatten {
            r.table_plans = crate::core::initialize::flatten_list_push_down_plan(
                r.table_plan.as_ref().unwrap(),
            );
        }
    }
}

/// The reader's flattened list is the same nodes as its table plan chain (Go: same pointers).
fn table_plans_in_sync(p: &PhysicalPlanRef) -> bool {
    let mut nodes = Vec::new();
    all_nodes(p, &mut nodes);
    nodes.iter().all(|n| {
        let Some(r) = n.as_any().downcast_ref::<PhysicalTableReader>() else {
            return true;
        };
        let flat =
            crate::core::initialize::flatten_list_push_down_plan(r.table_plan.as_ref().unwrap());
        flat.len() == r.table_plans.len()
            && flat
                .iter()
                .zip(&r.table_plans)
                .all(|(a, b)| Arc::ptr_eq(a, b))
    })
}

fn all_unique(p: &PhysicalPlanRef) -> (usize, usize) {
    let mut nodes = Vec::new();
    all_nodes(p, &mut nodes);
    let n = nodes.len();
    // `nodes` itself holds one extra reference to each; a node under a reader is also in
    // the reader's flattened `table_plans` (Go: the same pointer twice).
    let mut under_reader = Vec::new();
    for x in &nodes {
        if let Some(r) = x.as_any().downcast_ref::<PhysicalTableReader>() {
            under_reader.extend(r.table_plans.iter().map(|p| Arc::as_ptr(p) as *const ()));
        }
    }
    let unique = nodes
        .iter()
        .filter(|x| {
            let extra = usize::from(under_reader.contains(&(Arc::as_ptr(x) as *const ())));
            Arc::strong_count(x) == 2 + extra
        })
        .count();
    (unique, n)
}

fn report(name: &str, memo: &Memo, extra: &str) {
    eprintln!(
        "SPIKE {name}: stores={} hits={} hit_shares_stored={}/{} candidates_child_shared={}/{} cost_calls={} clone_node_in_cost={} {extra}",
        memo.stores,
        memo.hits,
        memo.hit_shares_stored.iter().filter(|x| **x).count(),
        memo.hit_shares_stored.len(),
        memo.candidate_child_shared.iter().filter(|x| **x).count(),
        memo.candidate_child_shared.len(),
        memo.cost_calls,
        memo.clones_in_cost,
    );
}

// ---------------------------------------------------------------------------------------
// experiments

/// E1: the VS1 shapes Limit / TopN over a data source, enumerated with the real operators.
#[test]
fn e1_vs1_shapes_memo_and_cost_copy_nothing() {
    for shape in ["limit", "topn"] {
        let mut vars = session();
        let mut ctx = ctx_of(&mut vars);
        let ds = data_source(&mut ctx);
        let root = match shape {
            "limit" => {
                let op = Box::new(LogicalLimit::init(1, 2, &ctx, 0));
                over(&mut ctx, op, ds, 2.0)
            }
            _ => {
                let by = vec![ByItems {
                    expr: col_expr(&col(1)),
                    desc: false,
                }];
                let op = Box::new(LogicalTopN::init(by, 0, 10, &ctx, 0));
                over(&mut ctx, op, ds, 10.0)
            }
        };
        reset_clones();
        let mut memo = Memo::default();
        let best = find_best_task(&mut ctx, &mut memo, root, &root_prop()).unwrap();
        let clones_enum = clones();
        assert!(!best.invalid(), "{shape}");
        assert_eq!(memo.clones_in_cost, 0, "{shape}: cost copies nothing");
        assert_eq!(clones_enum, 0, "{shape}: enumeration copies nothing");
        assert!(memo.hit_shares_stored.iter().all(|x| *x));
        assert!(memo.candidate_child_shared.iter().all(|x| *x));
        // Release the memo (N1 §3.4), then the winning tree is owned alone.
        let mut plan = task_plans(&best).remove(0);
        drop(best);
        let (u_before, n) = all_unique(&plan);
        let memo_report = Memo {
            stores: memo.stores,
            hits: memo.hits,
            hit_shares_stored: memo.hit_shares_stored.clone(),
            candidate_child_shared: memo.candidate_child_shared.clone(),
            clones_in_cost: memo.clones_in_cost,
            cost_calls: memo.cost_calls,
            ..Default::default()
        };
        drop(memo);
        let (u_after, _) = all_unique(&plan);
        // Naive postOptimize: writes through the table plan while table_plans holds it too.
        let mut naive = plan.clone_plan().unwrap();
        reset_clones();
        post_optimize_like(&mut naive, false);
        let clones_naive = clones();
        let naive_in_sync = table_plans_in_sync(&naive);
        reset_clones();
        post_optimize_like(&mut plan, true);
        report(
            shape,
            &memo_report,
            &format!(
                "nodes={n} unique_with_memo={u_before} unique_after_drop={u_after} clone_node_enum={clones_enum} clone_node_post_optimize_naive={clones_naive} naive_table_plans_in_sync={naive_in_sync} clone_node_post_optimize_reflatten={}",
                clones()
            ),
        );
        assert_eq!(u_after, n);
        assert!(
            clones_naive > 0 && !naive_in_sync,
            "{shape}: the reader's double ownership"
        );
        assert_eq!(clones(), 0, "{shape}: postOptimize in place");
        assert!(table_plans_in_sync(&plan));
    }
}

/// E2: Go's canonical sharing (two candidates of one logical node asking a child for the
/// same property, as getHashJoins' two build sides do): the second request is a memo hit,
/// both candidates hold the very same child `Arc` (Go: the same pointer), the cost memo of
/// the shared child is written once through `&self` and read by the second candidate.
#[test]
fn e2_shared_child_across_candidates() {
    let mut vars = session();
    let mut ctx = ctx_of(&mut vars);
    let ds = data_source(&mut ctx);
    let op = Box::new(LogicalLimit::init(0, 5, &ctx, 0));
    let lim = over(&mut ctx, op, ds, 5.0);
    let mk = |ctx: &PlanCtx<'_>, count: u64| -> PhysicalPlanRef {
        Arc::new(
            PhysicalLimit {
                count,
                ..Default::default()
            }
            .init(ctx, stats(count as f64), 0, vec![Some(root_prop())]),
        )
    };
    let plans = vec![mk(&ctx, 5), mk(&ctx, 6), mk(&ctx, 7)];
    reset_clones();
    let mut memo = Memo::default();
    let best = enumerate(&mut ctx, &mut memo, lim, plans, &root_prop(), false).unwrap();
    // The child task stored for (ds, root prop) and every candidate's child: one Arc.
    let stored = memo.task_map[&(ds, root_prop().hash_code().to_vec())]
        .clone()
        .unwrap();
    let child = task_plans(&stored).remove(0);
    // memo + 3 candidates... the losers were dropped inside enumerate: memo + best + `child`.
    let rc_after_enum = Arc::strong_count(&child);
    assert_eq!(memo.hits, 2);
    assert!(memo.hit_shares_stored.iter().all(|x| *x));
    assert_eq!(memo.candidate_child_shared, [true, true, true]);
    assert!(Arc::ptr_eq(&task_plans(&best)[0].children()[0], &child));
    assert!(
        child.base_physical().cost.lock().plan_cost_init,
        "cost memo on the shared node"
    );
    assert_eq!(memo.clones_in_cost, 0);
    assert_eq!(clones(), 0);
    let mut plan = task_plans(&best).remove(0);
    drop(best);
    drop(stored);
    drop(child);
    // Negative control: postOptimize while the memo is alive copies the shared child.
    let mut plan2 = Arc::clone(&plan);
    reset_clones();
    post_optimize_like(&mut plan2, true);
    let clones_with_memo = clones();
    drop(plan2);
    let memo_report = Memo {
        stores: memo.stores,
        hits: memo.hits,
        hit_shares_stored: memo.hit_shares_stored.clone(),
        candidate_child_shared: memo.candidate_child_shared.clone(),
        clones_in_cost: memo.clones_in_cost,
        cost_calls: memo.cost_calls,
        ..Default::default()
    };
    drop(memo);
    reset_clones();
    post_optimize_like(&mut plan, true);
    report(
        "shared-child",
        &memo_report,
        &format!(
            "child_rc_after_enum={rc_after_enum} clone_node_post_optimize_memo_alive={clones_with_memo} clone_node_post_optimize_after_drop={}",
            clones()
        ),
    );
    assert!(clones_with_memo > 0);
    assert_eq!(clones(), 0);
}

/// E3: rollback with Go's timestamps (TaskMapNeedBackUp = nth_plan hint): entries stored at
/// or after `ts` become `nil`, a new search builds a new task; a candidate still holding the
/// old task keeps it alive (Go: the GC).
#[test]
fn e3_rollback_by_timestamp() {
    let mut vars = session();
    vars.stmt_ctx_mut().stmt_hints.force_nth_plan = 1;
    let mut ctx = ctx_of(&mut vars);
    let ds = data_source(&mut ctx);
    let op = Box::new(LogicalLimit::init(0, 5, &ctx, 0));
    let lim = over(&mut ctx, op, ds, 5.0);
    let mut memo = Memo::default();
    let cop_prop = Arc::new(PhysicalProperty {
        task_tp: CopSingleReadTaskType,
        expected_cnt: 5.0,
        ..Default::default()
    });
    let t_root = find_best_task(&mut ctx, &mut memo, ds, &root_prop()).unwrap();
    let ts = Memo::get_logical_ts(&mut ctx);
    let t_cop = find_best_task(&mut ctx, &mut memo, ds, &cop_prop).unwrap();
    memo.roll_back(&ctx, lim, ts);
    assert!(memo.task_map[&(ds, root_prop().hash_code().to_vec())].is_some());
    assert!(memo.task_map[&(ds, cop_prop.hash_code().to_vec())].is_none());
    let t_cop2 = find_best_task(&mut ctx, &mut memo, ds, &cop_prop).unwrap();
    assert!(!Arc::ptr_eq(
        &task_plans(&t_cop)[0],
        &task_plans(&t_cop2)[0]
    ));
    let t_root2 = find_best_task(&mut ctx, &mut memo, ds, &root_prop()).unwrap();
    assert!(Arc::ptr_eq(
        &task_plans(&t_root)[0],
        &task_plans(&t_root2)[0]
    ));
    assert_eq!(ctx.vars.stmt_ctx().task_map_bak_ts, 4);
}

/// E4: the porting pitfall of the enumerate loop: `for pp in &physical_plans` +
/// `Arc::clone(pp)` into Attach2Task makes every candidate look shared, and
/// `attachPlan2Task`'s `SetChildren` copies it.
#[test]
fn e4_enumerate_loop_must_move_candidates() {
    let mut vars = session();
    let mut ctx = ctx_of(&mut vars);
    let ds = data_source(&mut ctx);
    let by = vec![ByItems {
        expr: col_expr(&col(1)),
        desc: false,
    }];
    let op = Box::new(LogicalTopN::init(by, 0, 10, &ctx, 0));
    let root = over(&mut ctx, op, ds, 10.0);
    let mut memo = Memo {
        clone_candidates: true,
        ..Default::default()
    };
    reset_clones();
    find_best_task(&mut ctx, &mut memo, root, &root_prop()).unwrap();
    let n = clones();
    report("clone-candidates", &memo, &format!("clone_node_enum={n}"));
    assert!(n > 0);
    assert_eq!(memo.clones_in_cost, 0);
}
