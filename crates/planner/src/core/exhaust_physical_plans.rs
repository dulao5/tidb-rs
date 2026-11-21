//! go-file: pkg/planner/core/exhaust_physical_plans.go
//!
//! `exhaustPhysicalPlans` of the logical operators of the first vertical slice (Selection,
//! Projection, Limit, TopN, Sort): the physical candidates of an operator for a required
//! property, each with the properties it requires from its child. The join, aggregation,
//! window, apply, union, lock, CTE, sequence and max-one-row parts of the file are outside
//! the slice and not ported yet.
//!
//! - Go's `exhaustPhysicalPlans4LogicalXxx(lp base.LogicalPlan, ...)` type-asserts `lp`;
//!   here the operator's `ExhaustPhysicalPlans` passes itself, detached from the arena
//!   (N1 §4), so the type assertion is gone (`M-IFACE`). Its children are read from the
//!   arena (`ctx.arena`).
//! - The candidates are new physical operators in `Arc`s, owned only by the returned
//!   `Vec` (`Attach2Task` changes them in place). Go's nil slice is an empty `Vec`.
//! - Go's candidates share the logical operator's expression slices and schema object
//!   (`Exprs: p.Exprs`, `proj.SetSchema(p.Schema())`); here the expression `Arc`s are
//!   shared and the schema is copied (spec §5, schema sharing).
//! - The push-down checks read the push-down context, which is a boundary until the
//!   planning context has a store client (D09.13): an `Err` is that boundary stub.
//! - `Offset + Count` wraps like Go's `uint64` addition.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::kv;
use tidb_expr_pushdown::infer_pushdown::can_exprs_push_down;
use tidb_expression::Expr;
use tidb_expression::builtin_vec::extract_vector_helper;
use tidb_expression::util::{contain_virtual_column, projection_benefits_from_pushed_down};

use crate::arena::{PlanArena, PlanId};
use crate::base::{LogicalKind, LogicalPlan, PhysicalPlan};
use crate::core::find_best_task::{
    get_prop_by_order_by_items, get_prop_by_order_by_items_contain_scalar_func,
};
use crate::core::physical_plans::{
    NominalSort, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort, PhysicalTopN,
};
use crate::logicalop::logical_plans_misc::can_push_to_cop_impl;
use crate::logicalop::{
    DataSource, LogicalLimit, LogicalProjection, LogicalSelection, LogicalSort, LogicalTopN,
};
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::PlanCtx;
use crate::property::{
    CopMultiReadTaskType, CopSingleReadTaskType, MppTaskType, PartialOrderInfo, PhysicalProperty,
    RootTaskType, SortItem, StatsInfo, TaskType,
};
use crate::util::ByItems;

/// The logical operator's stats, which Go dereferences without a check (derived before the
/// physical optimization).
// glue: Go's nil pointer dereference
fn stats_of(p: &dyn LogicalPlan) -> &Arc<StatsInfo> {
    p.base()
        .plan
        .stats_info()
        .expect("runtime error: invalid memory address or nil pointer dereference")
}

/// `exhaustPhysicalPlans4LogicalProjection`.
// go: exhaustPhysicalPlans4LogicalProjection L1 [M-IFACE, M-CTX]
pub fn exhaust_physical_plans4_logical_projection(
    p: &mut LogicalProjection,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
    let ctx = &*ctx;
    let Some(new_prop) = p.try_to_get_child_prop(ctx, prop) else {
        return Ok((Vec::new(), true));
    };
    let new_prop_task_tp = new_prop.task_tp;
    let mut new_props = vec![new_prop];
    // generate a mpp task candidate if mpp mode is allowed
    // (Go builds the push-down context here; it is built where it is first read, see the
    // module doc.)
    if new_prop_task_tp != MppTaskType
        && ctx.vars.is_mpp_allowed()
        && p.can_push_to_cop(ctx, kv::TiFlash)
        && can_exprs_push_down(&ctx.push_down_ctx()?, &p.exprs, kv::TiFlash)
    {
        let mut mpp_prop = new_props[0].clone_essential_fields();
        mpp_prop.task_tp = MppTaskType;
        new_props.push(mpp_prop);
    }
    if new_prop_task_tp != CopSingleReadTaskType
        && ctx.vars.allow_projection_push_down
        && p.can_push_to_cop(ctx, kv::TiKV)
        && can_exprs_push_down(&ctx.push_down_ctx()?, &p.exprs, kv::TiKV)
        && !contain_virtual_column(&p.exprs)
        && projection_benefits_from_pushed_down(
            &p.exprs,
            ctx.arena.get(p.children()[0]).schema(&ctx.arena).len(),
        )
    {
        let mut cop_prop = new_props[0].clone_essential_fields();
        cop_prop.task_tp = CopSingleReadTaskType;
        new_props.push(cop_prop);
    }

    let mut ret: Vec<PhysicalPlanRef> = Vec::with_capacity(new_props.len());
    for new_prop in new_props {
        let mut proj = PhysicalProjection {
            exprs: p.exprs.clone(),
            calculate_no_delay: p.calculate_no_delay,
            ..Default::default()
        }
        .init(
            ctx,
            Some(stats_of(p).scale_by_expect_cnt(prop.expected_cnt)),
            p.query_block_offset(),
            vec![Some(Arc::new(new_prop))],
        );
        proj.set_schema(p.schema(&ctx.arena).clone_schema());
        ret.push(Arc::new(proj));
    }
    Ok((ret, true))
}

/// `pushLimitOrTopNForcibly`: whether the Limit / TopN must be pushed to TiKV (the
/// `LIMIT_TO_COP` hint, or a small enough limit). An inapplicable hint is reset with a
/// warning.
// go: pushLimitOrTopNForcibly L1 [M-IFACE, M-CTX]
fn push_limit_or_top_n_forcibly(
    p: &mut dyn LogicalPlan,
    ctx: &PlanCtx<'_>,
    is_physical_limit: bool,
) -> bool {
    let meet_threshold;
    let prefer_push_down;
    match p.kind() {
        LogicalKind::TopN => {
            let lp = p
                .as_any()
                .downcast_ref::<LogicalTopN>()
                .expect("LogicalTopN");
            prefer_push_down = lp.prefer_limit_to_cop;
            if is_physical_limit {
                // For query using orderby + limit, the physicalop can be PhysicalLimit
                // when its corresponding logicalop is LogicalTopn.
                // And for PhysicalLimit, it's always better to let it pushdown to tikv.
                meet_threshold = true;
            } else {
                meet_threshold =
                    lp.count.wrapping_add(lp.offset) <= ctx.vars.limit_push_down_threshold as u64;
            }
        }
        LogicalKind::Limit => {
            let lp = p
                .as_any()
                .downcast_ref::<LogicalLimit>()
                .expect("LogicalLimit");
            prefer_push_down = lp.prefer_limit_to_cop;
            // Always push Limit down in this case since it has no side effect
            meet_threshold = true;
        }
        _ => return false,
    }

    if prefer_push_down || meet_threshold {
        if p.can_push_to_cop(ctx, kv::TiKV) {
            return true;
        }
        if prefer_push_down {
            ctx.vars
                .stmt_ctx()
                .set_hint_warning("Optimizer Hint LIMIT_TO_COP is inapplicable");
            // *preferPushDown = false
            match p.kind() {
                LogicalKind::TopN => {
                    p.as_any_mut()
                        .downcast_mut::<LogicalTopN>()
                        .expect("LogicalTopN")
                        .prefer_limit_to_cop = false;
                }
                _ => {
                    p.as_any_mut()
                        .downcast_mut::<LogicalLimit>()
                        .expect("LogicalLimit")
                        .prefer_limit_to_cop = false;
                }
            }
        }
    }

    false
}

/// The `PhysicalTopN` candidate of `lt` for the required child property `result_prop`.
// glue: the struct literal and Init repeated by getPhysTopN / getPhysTopNWithPartialOrderProperty
fn new_phys_top_n(
    lt: &LogicalTopN,
    ctx: &PlanCtx<'_>,
    result_prop: PhysicalProperty,
) -> PhysicalPlanRef {
    Arc::new(
        PhysicalTopN {
            by_items: lt.by_items.clone(),
            partition_by: lt.partition_by.clone(),
            count: lt.count,
            offset: lt.offset,
            ..Default::default()
        }
        .init(
            ctx,
            Some(Arc::clone(stats_of(lt))),
            lt.query_block_offset(),
            vec![Some(Arc::new(result_prop))],
        ),
    )
}

/// `getPhysTopN`.
// go: getPhysTopN L1 [M-IFACE, M-CTX]
fn get_phys_top_n(
    lt: &mut LogicalTopN,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Vec<PhysicalPlanRef> {
    let mut all_task_types: Vec<TaskType> = vec![CopSingleReadTaskType, CopMultiReadTaskType];
    if !push_limit_or_top_n_forcibly(lt, ctx, false) {
        all_task_types.push(RootTaskType);
    }
    let mpp_allowed = ctx.vars.is_mpp_allowed();
    if mpp_allowed {
        all_task_types.push(MppTaskType);
    }
    let mut ret: Vec<PhysicalPlanRef> = Vec::with_capacity(all_task_types.len() + 1);

    // The AdvisorySortItems optimization may not fully succeed (e.g. the global filter blocks the LIMIT pushdown),
    // in this case, it may generate a plan with unnecessary `keep order: true`. So we add this plan as an extra
    // candidate instead of replacing the original plan.
    let mut advisory_sort_items: Vec<SortItem> = Vec::new();
    let child_is_data_source = ctx.arena.get(lt.children()[0]).kind() == LogicalKind::DataSource;
    if child_is_data_source && !lt.by_items.is_empty() {
        advisory_sort_items = Vec::with_capacity(lt.by_items.len());
        for by_item in &lt.by_items {
            let Expr::Column(col) = &*by_item.expr else {
                advisory_sort_items = Vec::new();
                break;
            };
            advisory_sort_items.push(SortItem {
                col: Arc::new(col.clone()),
                desc: by_item.desc,
            });
        }
    }

    if can_use_partial_order4_top_n(lt, ctx) {
        ret.extend(get_phys_top_n_with_partial_order_property(lt, ctx, prop));
    }
    for tp in all_task_types {
        let result_prop = PhysicalProperty {
            task_tp: tp,
            expected_cnt: f64::MAX,
            cte_producer_status: prop.cte_producer_status,
            ..Default::default()
        };
        let advisory_prop = (tp == CopMultiReadTaskType && !advisory_sort_items.is_empty())
            .then(|| result_prop.clone_essential_fields());
        ret.push(new_phys_top_n(lt, ctx, result_prop));

        // The AdvisorySortItems optimization may not fully succeed, for
        // example when the global filter blocks LIMIT pushdown. In that case,
        // it may generate a plan with unnecessary keep order, so add this plan
        // as an extra candidate instead of replacing the original plan.
        if let Some(mut result_prop) = advisory_prop {
            result_prop.advisory_sort_items = advisory_sort_items.clone();
            ret.push(new_phys_top_n(lt, ctx, result_prop));
        }
    }
    // If we can generate MPP task and there's vector distance function in the order by column.
    // We will try to generate a property for possible vector indexes.
    if mpp_allowed {
        if lt.by_items.len() != 1 {
            return ret;
        }
        let Some(vs) = extract_vector_helper(&lt.by_items[0].expr) else {
            return ret;
        };
        // Currently vector index only accept ascending order.
        if lt.by_items[0].desc {
            return ret;
        }
        // Currently, we only deal with the case the TopN is directly above a DataSource.
        let child = ctx.arena.get(lt.children()[0]);
        let Some(ds) = child.as_any().downcast_ref::<DataSource>() else {
            return ret;
        };
        // Reject any filters.
        if !ds.pushed_down_conds.is_empty() {
            return ret;
        }
        let mut result_prop = PhysicalProperty {
            task_tp: MppTaskType,
            expected_cnt: f64::MAX,
            cte_producer_status: prop.cte_producer_status,
            ..Default::default()
        };
        result_prop.vector_prop.vector_helper = Some(Arc::new(vs));
        result_prop.vector_prop.top_k = lt.count.wrapping_add(lt.offset) as u32;
        ret.push(new_phys_top_n(lt, ctx, result_prop));
    }
    ret
}

/// canUsePartialOrder4TopN checks if the TopN's child tree satisfies the supported partial-order pattern.
// go: canUsePartialOrder4TopN L1 [M-CTX]
fn can_use_partial_order4_top_n(lt: &LogicalTopN, ctx: &PlanCtx<'_>) -> bool {
    if !ctx.vars.is_partial_ordered_index_for_top_n_enabled() {
        return false;
    }
    if lt.by_items.is_empty() {
        return false;
    }
    check_partial_order_pattern(&ctx.arena, lt.children()[0])
}

/// `checkPartialOrderPattern`: DataSource, under only Selections and Projections.
// go: checkPartialOrderPattern L1 [M-IFACE, M-PLAN]
fn check_partial_order_pattern(arena: &PlanArena, plan: PlanId) -> bool {
    let p = arena.get(plan);
    match p.kind() {
        LogicalKind::DataSource => true,
        LogicalKind::Selection | LogicalKind::Projection => {
            p.children().len() == 1 && check_partial_order_pattern(arena, p.children()[0])
        }
        _ => false,
    }
}

/// `getPhysTopNWithPartialOrderProperty`.
// go: getPhysTopNWithPartialOrderProperty L1 [M-CTX]
fn get_phys_top_n_with_partial_order_property(
    lt: &LogicalTopN,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Vec<PhysicalPlanRef> {
    let mut sort_items = Vec::with_capacity(lt.by_items.len());
    for by_item in &lt.by_items {
        let Expr::Column(col) = &*by_item.expr else {
            return Vec::new();
        };
        sort_items.push(SortItem {
            col: Arc::new(col.clone()),
            desc: by_item.desc,
        });
    }
    let partial_order_prop = PhysicalProperty {
        task_tp: CopMultiReadTaskType,
        expected_cnt: f64::MAX,
        partial_order_info: Some(Arc::new(PartialOrderInfo { sort_items })),
        cte_producer_status: prop.cte_producer_status,
        ..Default::default()
    };
    vec![new_phys_top_n(lt, ctx, partial_order_prop)]
}

/// `getPhysLimits`: the Limit candidates of a TopN whose order the child provides.
// go: getPhysLimits L1 [M-CTX]
fn get_phys_limits(
    lt: &mut LogicalTopN,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Vec<PhysicalPlanRef> {
    let Some(p) = get_prop_by_order_by_items(&lt.by_items) else {
        return Vec::new();
    };

    let mut all_task_types: Vec<TaskType> = vec![CopSingleReadTaskType, CopMultiReadTaskType];
    if !push_limit_or_top_n_forcibly(lt, ctx, true) {
        all_task_types.push(RootTaskType);
    }
    let mut ret: Vec<PhysicalPlanRef> = Vec::with_capacity(all_task_types.len());
    for tp in all_task_types {
        let result_prop = PhysicalProperty {
            task_tp: tp,
            expected_cnt: lt.count.wrapping_add(lt.offset) as f64,
            sort_items: p.sort_items.clone(),
            cte_producer_status: prop.cte_producer_status,
            ..Default::default()
        };
        let mut limit = PhysicalLimit {
            count: lt.count,
            offset: lt.offset,
            partition_by: lt.get_partition_by().to_vec(),
            ..Default::default()
        }
        .init(
            ctx,
            Some(Arc::clone(stats_of(lt))),
            lt.query_block_offset(),
            vec![Some(Arc::new(result_prop))],
        );
        limit.set_schema(lt.schema(&ctx.arena).clone_schema());
        ret.push(Arc::new(limit));
    }
    ret
}

/// MatchItems checks if this prop's columns can match by items totally.
// go: MatchItems L1
pub fn match_items(p: &PhysicalProperty, items: &[ByItems]) -> bool {
    if items.len() < p.sort_items.len() {
        return false;
    }
    for (i, col) in p.sort_items.iter().enumerate() {
        let sort_item = &items[i];
        if sort_item.desc != col.desc || !col.col.equal_column(&sort_item.expr) {
            return false;
        }
    }
    true
}

/// `exhaustPhysicalPlans4LogicalSelection`.
// go: exhaustPhysicalPlans4LogicalSelection L1 [M-IFACE, M-CTX]
pub fn exhaust_physical_plans4_logical_selection(
    p: &mut LogicalSelection,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
    let ctx = &*ctx;
    let mut new_props = Vec::with_capacity(2);
    let child_prop = prop.clone_essential_fields();
    new_props.push(child_prop);

    if prop.task_tp != MppTaskType
        && ctx.vars.is_mpp_allowed()
        && p.can_push_down(ctx, kv::TiFlash)?
    {
        let mut child_prop_mpp = prop.clone_essential_fields();
        child_prop_mpp.task_tp = MppTaskType;
        new_props.push(child_prop_mpp);
    }

    let mut ret: Vec<PhysicalPlanRef> = Vec::with_capacity(new_props.len());
    for new_prop in new_props {
        let sel = PhysicalSelection {
            conditions: p.conditions.clone(),
            ..Default::default()
        }
        .init(
            ctx,
            Some(stats_of(p).scale_by_expect_cnt(prop.expected_cnt)),
            p.query_block_offset(),
            vec![Some(Arc::new(new_prop))],
        );
        ret.push(Arc::new(sel));
    }
    Ok((ret, true))
}

/// `exhaustPhysicalPlans4LogicalLimit`.
// go: exhaustPhysicalPlans4LogicalLimit L1 [M-IFACE]
pub fn exhaust_physical_plans4_logical_limit(
    p: &mut LogicalLimit,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
    get_limit_physical_plans(p, ctx, prop)
}

/// `getLimitPhysicalPlans`.
// go: getLimitPhysicalPlans L1 [M-CTX]
fn get_limit_physical_plans(
    p: &mut LogicalLimit,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
    let ctx = &*ctx;
    if !prop.is_sort_item_empty() {
        return Ok((Vec::new(), true));
    }

    let mut all_task_types: Vec<TaskType> = vec![CopSingleReadTaskType, CopMultiReadTaskType];
    if !push_limit_or_top_n_forcibly(p, ctx, true) {
        all_task_types.push(RootTaskType);
    }
    if p.can_push_to_cop(ctx, kv::TiFlash) && ctx.vars.is_mpp_allowed() {
        all_task_types.push(MppTaskType);
    }
    let mut ret: Vec<PhysicalPlanRef> = Vec::with_capacity(all_task_types.len());
    for tp in all_task_types {
        let result_prop = PhysicalProperty {
            task_tp: tp,
            expected_cnt: p.count.wrapping_add(p.offset) as f64,
            cte_producer_status: prop.cte_producer_status,
            ..Default::default()
        };
        let mut limit = PhysicalLimit {
            offset: p.offset,
            count: p.count,
            partition_by: p.get_partition_by().to_vec(),
            ..Default::default()
        }
        .init(
            ctx,
            Some(Arc::clone(stats_of(p))),
            p.query_block_offset(),
            vec![Some(Arc::new(result_prop))],
        );
        limit.set_schema(p.schema(&ctx.arena).clone_schema());
        ret.push(Arc::new(limit));
    }
    Ok((ret, true))
}

/// `exhaustPhysicalPlans4LogicalTopN`.
// go: exhaustPhysicalPlans4LogicalTopN L1 [M-IFACE]
pub fn exhaust_physical_plans4_logical_top_n(
    lt: &mut LogicalTopN,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
    let ctx = &*ctx;
    if match_items(prop, &lt.by_items) {
        let mut ret = get_phys_top_n(lt, ctx, prop);
        ret.extend(get_phys_limits(lt, ctx, prop));
        return Ok((ret, true));
    }
    Ok((Vec::new(), true))
}

/// `exhaustPhysicalPlans4LogicalSort`.
///
/// Go checks `CanPushToCopImpl(&ls.BaseLogicalPlan, ...)`; the embedded base is not an
/// operator of its own here, the sort is passed (the function reads only its children and
/// whether it is a TopN / Limit, which neither is).
// go: exhaustPhysicalPlans4LogicalSort L1 [M-IFACE, M-SELF]
pub fn exhaust_physical_plans4_logical_sort(
    ls: &mut LogicalSort,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
    let ctx = &*ctx;
    if prop.task_tp == RootTaskType {
        if match_items(prop, &ls.by_items) {
            let mut ret: Vec<PhysicalPlanRef> = Vec::with_capacity(2);
            ret.push(get_physical_sort(ls, ctx, prop));
            if let Some(ns) = get_nominal_sort(ls, ctx, prop) {
                ret.push(ns);
            }
            return Ok((ret, true));
        }
    } else if prop.task_tp == MppTaskType
        && prop.reject_sort
        && can_push_to_cop_impl(ls, ctx, kv::TiFlash, true)
    {
        let ps = get_nominal_sort_simple(ls, ctx, prop);
        return Ok((vec![ps], true));
    }
    Ok((Vec::new(), true))
}

/// `getPhysicalSort`.
// go: getPhysicalSort L1 [M-CTX]
fn get_physical_sort(
    ls: &LogicalSort,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> PhysicalPlanRef {
    let ps = PhysicalSort {
        by_items: ls.by_items.clone(),
        ..Default::default()
    }
    .init(
        ctx,
        Some(stats_of(ls).scale_by_expect_cnt(prop.expected_cnt)),
        ls.query_block_offset(),
        vec![Some(Arc::new(PhysicalProperty {
            task_tp: prop.task_tp,
            expected_cnt: f64::MAX,
            reject_sort: true,
            cte_producer_status: prop.cte_producer_status,
            ..Default::default()
        }))],
    );
    Arc::new(ps)
}

/// `getNominalSort`. Go's nil is `None`.
// go: getNominalSort L1 [M-CTX]
fn get_nominal_sort(
    ls: &LogicalSort,
    ctx: &PlanCtx<'_>,
    req_prop: &PhysicalProperty,
) -> Option<PhysicalPlanRef> {
    let (mut prop, only_column) = get_prop_by_order_by_items_contain_scalar_func(&ls.by_items)?;
    prop.reject_sort = true;
    prop.expected_cnt = req_prop.expected_cnt;
    let expected_cnt = prop.expected_cnt;
    let ps = NominalSort {
        only_column,
        by_items: ls.by_items.clone(),
        ..Default::default()
    }
    .init(
        ctx,
        Some(stats_of(ls).scale_by_expect_cnt(expected_cnt)),
        ls.query_block_offset(),
        vec![Some(Arc::new(prop))],
    );
    Some(Arc::new(ps))
}

/// `getNominalSortSimple`.
// go: getNominalSortSimple L1 [M-CTX]
fn get_nominal_sort_simple(
    ls: &LogicalSort,
    ctx: &PlanCtx<'_>,
    req_prop: &PhysicalProperty,
) -> PhysicalPlanRef {
    let mut new_prop = req_prop.clone_essential_fields();
    new_prop.reject_sort = true;
    let ps = NominalSort {
        only_column: true,
        by_items: ls.by_items.clone(),
        ..Default::default()
    }
    .init(
        ctx,
        Some(stats_of(ls).scale_by_expect_cnt(req_prop.expected_cnt)),
        ls.query_block_offset(),
        vec![Some(Arc::new(new_prop))],
    );
    Arc::new(ps)
}
