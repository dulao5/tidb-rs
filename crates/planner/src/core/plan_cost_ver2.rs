//! go-file: pkg/planner/core/plan_cost_ver2.go
//!
//! The cost model v2 of the operators of the first vertical slice (D10.5;
//! `specs/D10.5-cost-model/spec.md`): `getPlanCost`, `GetPlanCostVer2` of Selection,
//! Projection, TableScan, TableReader, Sort, TopN and the point get, the formula helpers and
//! the factor table. The other operators (index readers, joins, aggregation, MPP, CTE, the
//! batch point get) are not ported yet: their nodes cannot exist, so the arms of the type
//! switches below that name them are left out (each place says which).
//!
//! - The context of `p.SCtx()` is a parameter (`M-CTX`).
//! - The memo (`PlanCostInit` / `PlanCostVer2`) is the embedded `BasePhysicalPlan`'s, written
//!   through `&self` like `BasePhysicalPlan.GetPlanCostVer2` (`M-LOCK`; the memo is shared by
//!   every candidate that holds the node, review of #223). Go sets `PlanCostInit` before it
//!   multiplies the cost by the operator's cost factor; the memo is written once, with the
//!   final value (nothing reads it in between).
//! - Go's builtin `min` / `max` on floats are `math::min` / `math::max` (the same NaN and
//!   signed zero rules); `math.Log2` is `math::log2` (Go's own, not Rust's).
//! - No line of `plan_cost_ver2.go` is in the tracker's FMA registry (mapping rule `M-FMA`):
//!   the sums go through `SumCostVer2`, and no product is added in the same expression.
//! - The `GenPlanCostTrace` hook is nil in TiDB (only Go tests set it), so `getPlanCost`
//!   never calls it; it is not carried.

// The constants and the factor table keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, LazyLock};

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::conv::f64_to_i64;
use tidb_base::gostd::math;
use tidb_base::kv::{self, StoreType};
use tidb_base::mysql::has_unsigned_flag;
use tidb_expression::{Expr, ExprRef};
use tidb_model::{TableInfo, TempTableNone};
use tidb_sessionctx::ranger::types::has_full_range;
use tidb_sessionctx::variable::tidb_vars::EnableTmpStorageOnOOM;

use crate::base::{PhysicalKind, PhysicalPlan};
use crate::core::physical_plans::{
    PhysicalIndexReader, PhysicalIndexScan, PhysicalProjection, PhysicalSelection, PhysicalSort,
    PhysicalTableReader, PhysicalTableScan, PhysicalTopN,
};
use crate::core::plan_cost_ver1::{get_cardinality, has_cost_flag, modelVer2};
use crate::core::point_get_plan::PointGetPlan;
use crate::core::task::get_avg_row_size;
use crate::physicalop::BasePhysicalPlan;
use crate::planctx::PlanCtx;
use crate::property::{CopSingleReadTaskType, MppTaskType, RootTaskType, StatsInfo, TaskType};
use crate::util::ByItems;
use crate::util::costusage::{
    CostFlagRecalculate, CostVer2, CostVer2Factor, ZeroCostVer2, div_cost_ver2, mul_cost_ver2,
    new_cost_ver2, sum_cost_ver2,
};
use crate::util::optimizetrace::PlanCostOption;

/// GetPlanCost returns the cost of this plan.
// go: GetPlanCost L1 [M-CTX]
pub fn get_plan_cost(
    ctx: &PlanCtx<'_>,
    p: &dyn PhysicalPlan,
    task_type: TaskType,
    option: &PlanCostOption,
) -> Result<f64, Error> {
    get_plan_cost_impl(ctx, p, task_type, option)
}

/// `getPlanCost`. Go returns `(planCost.GetCost(), err)`; on an error the cost is the zero
/// cost, which callers ignore.
///
/// The cost model v1 (`tidb_cost_model_version=1`) is exception B5: not ported. Go calls
/// `p.GetPlanCostVer1`; here it is a boundary error (the router falls back), never the base
/// implementation's sum of the children (which no operator of Go uses).
// go: getPlanCost L1 [M-CTX]
pub fn get_plan_cost_impl(
    ctx: &PlanCtx<'_>,
    p: &dyn PhysicalPlan,
    task_type: TaskType,
    option: &PlanCostOption,
) -> Result<f64, Error> {
    if ctx.vars.cost_model_version == modelVer2 {
        let plan_cost = p.get_plan_cost_ver2(ctx, task_type, option)?;
        // `GenPlanCostTrace` is nil (see the module doc).
        return Ok(plan_cost.get_cost());
    }
    // glue: blocked-by:EXC.B (the cost model v1, exception B5: tidb_cost_model_version=1 is not ported)
    Err(blocked(
        "EXC.B",
        "the cost model v1 (tidb_cost_model_version=1)",
    ))
}

/// `if p.PlanCostInit && !hasCostFlag(option.CostFlag, costusage.CostFlagRecalculate)
/// { return p.PlanCostVer2, nil }`.
// glue: the memo read of every GetPlanCostVer2 (M-LOCK, see the module doc)
fn cached_cost(base: &BasePhysicalPlan, option: &PlanCostOption) -> Option<CostVer2> {
    let c = base.cost.lock();
    if c.plan_cost_init && !has_cost_flag(option.cost_flag, CostFlagRecalculate) {
        return Some(c.plan_cost_ver2.clone());
    }
    None
}

/// `p.PlanCostVer2 = cost; p.PlanCostInit = true; return p.PlanCostVer2, nil`.
// glue: the memo write of every GetPlanCostVer2 (M-LOCK, see the module doc)
fn set_cost(base: &BasePhysicalPlan, cost: CostVer2) -> Result<CostVer2, Error> {
    if base.is_sealed() {
        return Ok(cost); // the plan-cache exception A3 spec §3.6: computed, not written
    }
    let mut c = base.cost.lock();
    c.plan_cost_ver2 = cost.clone();
    c.plan_cost_init = true;
    Ok(cost)
}

/// `p.StatsInfo()`, which Go dereferences without a check.
// glue: Go's nil pointer dereference
fn stats_of(p: &dyn PhysicalPlan) -> &Arc<StatsInfo> {
    p.stats_info()
        .expect("runtime error: invalid memory address or nil pointer dereference")
}

/// `fmt.Sprintf` of a cost formula (ASCII).
// glue: fmt.Sprintf of an ASCII cost formula as a String (gofmt::sprintf returns bytes)
fn sprintf_s(format: &str, args: &[Arg]) -> String {
    String::from_utf8_lossy(&sprintf(format, args)).into_owned()
}

/// A `CostVer2Factor` as a `fmt` argument: `%v` calls its `String` method.
// glue: fmt.Stringer argument
fn factor_arg(f: &CostVer2Factor) -> Arg {
    Arg::Stringer(f.to_string(), "costusage.CostVer2Factor")
}

/// A `float64` `fmt` argument.
// glue: fmt argument
fn f64_arg(f: f64) -> Arg {
    Arg::Float(f, 64)
}

impl PhysicalSelection {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    /// plan-cost = child-cost + filter-cost
    // go: PhysicalSelection.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        if let Some(c) = cached_cost(&self.base, option) {
            return Ok(c);
        }

        let child = self.children()[0].as_ref();
        let input_rows = get_cardinality(ctx, child, option.cost_flag);
        let cpu_factor = get_task_cpu_factor_ver2(self, task_type);

        let filter_cost = filter_cost_ver2(option, input_rows, &self.conditions, cpu_factor);

        let child_cost = child.get_plan_cost_ver2(ctx, task_type, option)?;

        set_cost(&self.base, sum_cost_ver2(&[filter_cost, child_cost]))
    }
}

impl PhysicalProjection {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    /// plan-cost = child-cost + proj-cost / concurrency
    /// proj-cost = input-rows * len(expressions) * cpu-factor
    // go: PhysicalProjection.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        let base = &self.sp.base;
        if let Some(c) = cached_cost(base, option) {
            return Ok(c);
        }

        let child = self.children()[0].as_ref();
        let input_rows = get_cardinality(ctx, child, option.cost_flag);
        let cpu_factor = get_task_cpu_factor_ver2(self, task_type);
        let mut concurrency = ctx.vars.projection_concurrency() as f64;
        if concurrency == 0.0 {
            concurrency = 1.0; // un-parallel execution
        }

        let proj_cost = filter_cost_ver2(option, input_rows, &self.exprs, cpu_factor);

        let child_cost = child.get_plan_cost_ver2(ctx, task_type, option)?;

        set_cost(
            base,
            sum_cost_ver2(&[child_cost, div_cost_ver2(&proj_cost, concurrency)]),
        )
    }
}

/// MinNumRows provides a minimum to avoid underestimation. As selectivity estimation approaches
/// zero, all plan choices result in a low cost - making it difficult to differentiate plan choices.
/// A low value of 1.0 here is used for most (non probe acceses) to reduce this risk.
pub const MinNumRows: f64 = 1.0;
/// MinRowSize provides a minimum column length to ensure that any adjustment or calculation
/// in costing does not go below this value. 2.0 is used as a reasonable lowest column length.
pub const MinRowSize: f64 = 2.0;
/// TiFlashStartupRowPenalty applies a startup penalty for TiFlash scan to encourage TiKV usage for small scans
pub const TiFlashStartupRowPenalty: f64 = 10000.0;
/// MaxPenaltyRowCount applies a penalty for high risk scans
pub const MaxPenaltyRowCount: f64 = 1000.0;

impl PhysicalIndexScan {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    /// plan-cost = rows * log2(row-size) * scan-factor
    /// log2(row-size) is from experiments.
    // go: PhysicalIndexScan.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        let base = &self.sp.base;
        if let Some(c) = cached_cost(base, option) {
            return Ok(c);
        }

        let rows = get_cardinality(ctx, self, option.cost_flag);
        let row_size = get_avg_row_size(stats_of(self), &self.schema().columns); // consider all index columns
        let scan_factor = get_task_scan_factor_ver2(self, kv::TiKV, task_type);

        let mut plan_cost = scan_cost_ver2(option, rows, row_size, scan_factor);
        // Multiply by cost factor - defaults to 1, but can be increased/decreased to influence the cost model
        plan_cost = mul_cost_ver2(&plan_cost, ctx.vars.index_scan_cost_factor);
        set_cost(base, plan_cost)
    }
}

impl PhysicalIndexReader {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    /// plan-cost = (child-cost + net-cost) / concurrency
    /// net-cost = rows * row-size * net-factor
    // go: PhysicalIndexReader.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        let base = &self.sp.base;
        if let Some(c) = cached_cost(base, option) {
            return Ok(c);
        }

        let index_plan = self
            .index_plan
            .as_deref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        let rows = get_cardinality(ctx, index_plan, option.cost_flag);
        let row_size = get_avg_row_size(stats_of(self), &self.schema().columns);
        let net_factor = get_task_net_factor_ver2(self, task_type);
        let concurrency = ctx.vars.dist_sql_scan_concurrency() as f64;

        let net_cost = net_cost_ver2(option, rows, row_size, net_factor);
        let child_cost = index_plan.get_plan_cost_ver2(ctx, CopSingleReadTaskType, option)?;

        let mut plan_cost = div_cost_ver2(&sum_cost_ver2(&[child_cost, net_cost]), concurrency);
        // Multiply by cost factor - defaults to 1, but can be increased/decreased to influence the cost model
        plan_cost = mul_cost_ver2(&plan_cost, ctx.vars.index_reader_cost_factor);
        set_cost(base, plan_cost)
    }
}

impl PhysicalTableScan {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    /// plan-cost = rows * log2(row-size) * scan-factor
    /// log2(row-size) is from experiments.
    ///
    /// The vector index branch (`p.AnnIndexExtra != nil`) is not carried: the scan has no
    /// `AnnIndexExtra` field (the vector index is outside the slice, D10.1 spec §3), so it is
    /// always Go's nil.
    // go: PhysicalTableScan.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        let base = &self.sp.base;
        if let Some(c) = cached_cost(base, option) {
            return Ok(c);
        }

        let columns = if self.store_type == kv::TiKV {
            // Assume all columns for TiKV
            &self.tbl_cols
        } else {
            // TiFlash
            &self.schema().columns
        };
        let mut rows = get_cardinality(ctx, self, option.cost_flag);
        let mut row_size = get_avg_row_size(stats_of(self), columns);
        // Ensure rows and rowSize have a reasonable minimum value to avoid underestimation
        if !self.is_child_of_index_look_up {
            rows = math::max(MinNumRows, rows);
            row_size = math::max(row_size, MinRowSize);
        }

        let scan_factor = get_task_scan_factor_ver2(self, self.store_type, task_type);
        let mut plan_cost = scan_cost_ver2(option, rows, row_size, scan_factor);

        let mut unsigned_int_handle = false;
        if self.table.pk_is_handle
            && let Some(pk_col_info) = self.table.get_pk_col_info()
        {
            unsigned_int_handle = has_unsigned_flag(pk_col_info.get_flag());
        }
        let has_full_range_scan = has_full_range(&self.ranges, unsigned_int_handle);

        // Apply TiFlash startup cost to prefer TiKV for small table scans
        if self.store_type == kv::TiFlash {
            plan_cost = sum_cost_ver2(&[
                plan_cost,
                scan_cost_ver2(option, TiFlashStartupRowPenalty, row_size, scan_factor),
            ]);
        } else if !self.is_child_of_index_look_up && has_full_range_scan {
            let new_row_count = get_table_scan_penalty(ctx, self, rows);
            if new_row_count > 0.0 {
                plan_cost = sum_cost_ver2(&[
                    plan_cost,
                    scan_cost_ver2(option, new_row_count, row_size, scan_factor),
                ]);
            }
        }

        // (`p.AnnIndexExtra != nil`: not carried, see the doc.)

        // Multiply by cost factor - defaults to 1, but can be increased/decreased to influence the cost model
        let vars = &*ctx.vars;
        if self.is_child_of_index_look_up {
            // This is a RowID table scan (child of IndexLookUp)
            plan_cost = mul_cost_ver2(&plan_cost, vars.table_row_id_scan_cost_factor);
        } else if !has_full_range_scan {
            // This is a table range scan (predicate exists on the PK)
            plan_cost = mul_cost_ver2(&plan_cost, vars.table_range_scan_cost_factor);
        } else {
            // This is a table full scan
            if self.store_type == kv::TiFlash {
                plan_cost = mul_cost_ver2(&plan_cost, vars.table_tiflash_scan_cost_factor);
            } else {
                plan_cost = mul_cost_ver2(&plan_cost, vars.table_full_scan_cost_factor);
            }
        }
        set_cost(base, plan_cost)
    }
}

// `PhysicalIndexReader.GetPlanCostVer2`: the index reader is not ported (see the module doc).

impl PhysicalTableReader {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    /// plan-cost = (child-cost + net-cost) / concurrency
    /// net-cost = rows * row-size * net-factor
    // go: PhysicalTableReader.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        let base = &self.sp.base;
        if let Some(c) = cached_cost(base, option) {
            return Ok(c);
        }

        let table_plan = self
            .table_plan
            .as_deref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        let rows = get_cardinality(ctx, table_plan, option.cost_flag);
        let row_size = math::max(
            MinRowSize,
            get_avg_row_size(stats_of(self), &self.schema().columns),
        );
        let net_factor = get_task_net_factor_ver2(self, task_type);
        let concurrency = ctx.vars.dist_sql_scan_concurrency() as f64;
        let mut child_type = CopSingleReadTaskType;
        if self.store_type == kv::TiFlash {
            // mpp protocol
            child_type = MppTaskType;
        }

        let net_cost = net_cost_ver2(option, rows, row_size, net_factor);

        let child_cost = table_plan.get_plan_cost_ver2(ctx, child_type, option)?;

        let mut plan_cost = div_cost_ver2(&sum_cost_ver2(&[child_cost, net_cost]), concurrency);

        // consider tidb_enforce_mpp
        if self.store_type == kv::TiFlash
            && ctx.vars.is_mpp_enforced()
            && !has_cost_flag(option.cost_flag, CostFlagRecalculate)
        {
            // show the real cost in explain-statements
            plan_cost = div_cost_ver2(&plan_cost, 1000000000.0);
        }
        // Multiply by cost factor - defaults to 1, but can be increased/decreased to influence the cost model
        plan_cost = mul_cost_ver2(&plan_cost, ctx.vars.table_reader_cost_factor);
        set_cost(base, plan_cost)
    }
}

// `PhysicalIndexLookUpReader.GetPlanCostVer2`, `PhysicalIndexMergeReader.GetPlanCostVer2`: not
// ported (see the module doc).

impl PhysicalSort {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    ///
    /// ```text
    /// plan-cost = child-cost + sort-cpu-cost + sort-mem-cost + sort-disk-cost
    /// sort-cpu-cost = rows * log2(rows) * len(sort-items) * cpu-factor
    /// if no spill:
    /// 1. sort-mem-cost = rows * row-size * mem-factor
    /// 2. sort-disk-cost = 0
    /// else if spill:
    /// 1. sort-mem-cost = mem-quota * mem-factor
    /// 2. sort-disk-cost = rows * row-size * disk-factor
    /// ```
    // go: PhysicalSort.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        if let Some(c) = cached_cost(&self.base, option) {
            return Ok(c);
        }

        let child = self.children()[0].as_ref();
        let rows = math::max(MinNumRows, get_cardinality(ctx, child, option.cost_flag));
        let row_size = math::max(
            MinRowSize,
            get_avg_row_size(stats_of(self), &self.schema().columns),
        );
        let cpu_factor = get_task_cpu_factor_ver2(self, task_type);
        let mem_factor = get_task_mem_factor_ver2(self, task_type);
        let disk_factor = &defaultVer2Factors.tidb_disk;
        let oom_use_tmp_storage = EnableTmpStorageOnOOM.load(SeqCst);
        let mem_quota = ctx.vars.state.mem_tracker_bytes_limit;
        let spill = task_type == RootTaskType && // only TiDB can spill
            oom_use_tmp_storage && // spill is enabled
            mem_quota > 0 && // mem-quota is set
            row_size * rows > mem_quota as f64; // exceed the mem-quota

        let sort_cpu_cost = order_cost_ver2(option, rows, rows, &self.by_items, cpu_factor);

        let (sort_mem_cost, sort_disk_cost);
        if !spill {
            sort_mem_cost = new_cost_ver2(
                Some(option),
                mem_factor,
                rows * row_size * mem_factor.value,
                || {
                    sprintf_s(
                        "sortMem(%v*%v*%v)",
                        &[f64_arg(rows), f64_arg(row_size), factor_arg(mem_factor)],
                    )
                },
            );
            sort_disk_cost = ZeroCostVer2.clone();
        } else {
            sort_mem_cost = new_cost_ver2(
                Some(option),
                mem_factor,
                mem_quota as f64 * mem_factor.value,
                || {
                    sprintf_s(
                        "sortMem(%v*%v)",
                        &[Arg::Int(mem_quota, "int64"), factor_arg(mem_factor)],
                    )
                },
            );
            sort_disk_cost = new_cost_ver2(
                Some(option),
                disk_factor,
                rows * row_size * disk_factor.value,
                || {
                    sprintf_s(
                        "sortDisk(%v*%v*%v)",
                        &[f64_arg(rows), f64_arg(row_size), factor_arg(disk_factor)],
                    )
                },
            );
        }

        let child_cost = child.get_plan_cost_ver2(ctx, task_type, option)?;

        let mut plan_cost =
            sum_cost_ver2(&[child_cost, sort_cpu_cost, sort_mem_cost, sort_disk_cost]);
        // Multiply by cost factor - defaults to 1, but can be increased/decreased to influence the cost model
        plan_cost = mul_cost_ver2(&plan_cost, ctx.vars.sort_cost_factor);
        set_cost(&self.base, plan_cost)
    }
}

impl PhysicalTopN {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    /// plan-cost = child-cost + topn-cpu-cost + topn-mem-cost
    /// topn-cpu-cost = rows * log2(N) * len(sort-items) * cpu-factor
    /// topn-mem-cost = N * row-size * mem-factor
    ///
    /// `p.Count+p.Offset` is a `uint64` sum that wraps in Go.
    // go: PhysicalTopN.GetPlanCostVer2 L1 [M-CTX, M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        if let Some(c) = cached_cost(&self.base, option) {
            return Ok(c);
        }

        let child = self.children()[0].as_ref();
        let rows = math::max(MinNumRows, get_cardinality(ctx, child, option.cost_flag));
        let mut n = math::max(1.0, self.count.wrapping_add(self.offset) as f64);
        if n > 10000.0 {
            // It's only used to prevent some extreme cases, e.g. `select * from t order by a limit 18446744073709551615`.
            // For normal cases, considering that `rows` may be under-estimated, better to keep `n` unchanged.
            n = math::min(n, rows);
        }
        let row_size = math::max(
            MinRowSize,
            get_avg_row_size(stats_of(self), &self.schema().columns),
        );
        let cpu_factor = get_task_cpu_factor_ver2(self, task_type);
        let mem_factor = get_task_mem_factor_ver2(self, task_type);

        let top_n_cpu_cost = order_cost_ver2(option, rows, n, &self.by_items, cpu_factor);
        let top_n_mem_cost = new_cost_ver2(
            Some(option),
            mem_factor,
            n * row_size * mem_factor.value,
            || {
                sprintf_s(
                    "topMem(%v*%v*%v)",
                    &[f64_arg(n), f64_arg(row_size), factor_arg(mem_factor)],
                )
            },
        );

        let child_cost = child.get_plan_cost_ver2(ctx, task_type, option)?;

        let mut plan_cost = sum_cost_ver2(&[child_cost, top_n_cpu_cost, top_n_mem_cost]);
        // Multiply by cost factor - defaults to 1, but can be increased/decreased to influence the cost model
        plan_cost = mul_cost_ver2(&plan_cost, ctx.vars.topn_cost_factor);
        set_cost(&self.base, plan_cost)
    }
}

// `PhysicalStreamAgg`, `PhysicalHashAgg`, `PhysicalMergeJoin`, `PhysicalHashJoin`, the index
// joins: not ported (see the module doc).

/// getNumberOfRanges returns the total number of ranges of a physical plan tree.
/// Some queries with large IN list like `a in (1, 2, 3...)` could generate a large number of ranges, which may slow down the query performance.
///
/// The `*PhysicalIndexReader`, `*PhysicalIndexLookUpReader` and `*PhysicalIndexScan` arms
/// are left out: those operators are not ported (see the module doc).
// go: getNumberOfRanges L1
pub fn get_number_of_ranges(pp: &dyn PhysicalPlan) -> isize {
    match pp.kind() {
        PhysicalKind::TableReader => {
            let p = pp
                .as_any()
                .downcast_ref::<PhysicalTableReader>()
                .expect("type assertion");
            return get_number_of_ranges(
                p.table_plan
                    .as_deref()
                    .expect("runtime error: invalid memory address or nil pointer dereference"),
            );
        }
        PhysicalKind::IndexReader => {
            let p = pp
                .as_any()
                .downcast_ref::<PhysicalIndexReader>()
                .expect("type assertion");
            return get_number_of_ranges(
                p.index_plan
                    .as_deref()
                    .expect("runtime error: invalid memory address or nil pointer dereference"),
            );
        }
        PhysicalKind::TableScan => {
            let p = pp
                .as_any()
                .downcast_ref::<PhysicalTableScan>()
                .expect("type assertion");
            return p.ranges.len() as isize;
        }
        PhysicalKind::IndexScan => {
            let p = pp
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .expect("type assertion");
            return p.ranges.len() as isize;
        }
        _ => {}
    }
    let mut tot_num_ranges = 0;
    for child in pp.children() {
        tot_num_ranges += get_number_of_ranges(child.as_ref());
    }
    tot_num_ranges
}

// `PhysicalApply`, `PhysicalUnionAll`, `PhysicalExchangeReceiver`: not ported (see the module
// doc).

impl PointGetPlan {
    /// GetPlanCostVer2 returns the plan-cost of this sub-plan, which is:
    ///
    /// Go's `planCostInit` / `planCostVer2` fields are the memo of the embedded
    /// `BasePhysicalPlan` (the module doc). Go tests `p.accessCols == nil` (the fast plan does
    /// not set it); a `Vec` has no nil, so an empty one is nil: `FindBestTask` sets the
    /// accessed columns of the table or the index, never an empty list.
    // go: PointGetPlan.GetPlanCostVer2 L2 (an empty accessCols is Go's nil)
    pub fn get_plan_cost_ver2(
        &self,
        _ctx: &PlanCtx<'_>,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        if let Some(c) = cached_cost(&self.base, option) {
            return Ok(c);
        }

        if self.access_cols.is_empty() {
            // from fast plan code path
            set_cost(&self.base, ZeroCostVer2.clone())?;
            return Ok(ZeroCostVer2.clone());
        }
        let row_size = get_avg_row_size(stats_of(self), &self.schema.columns);
        let net_factor = get_task_net_factor_ver2(self, task_type);

        set_cost(&self.base, net_cost_ver2(option, 1.0, row_size, net_factor))
    }
}

// `BatchPointGetPlan.GetPlanCostVer2`, `PhysicalCTE.GetPlanCostVer2`,
// `indexJoinSeekingCostVer2`: not ported (see the module doc).

// go: scanCostVer2 L1
pub fn scan_cost_ver2(
    option: &PlanCostOption,
    rows: f64,
    mut row_size: f64,
    scan_factor: &CostVer2Factor,
) -> CostVer2 {
    if row_size < 1.0 {
        row_size = 1.0;
    }
    new_cost_ver2(
        Some(option),
        scan_factor,
        // rows * log(row-size) * scanFactor, log2 from experiments
        rows * math::max(math::log2(row_size), 0.0) * scan_factor.value,
        || {
            sprintf_s(
                "scan(%v*logrowsize(%v)*%v)",
                &[f64_arg(rows), f64_arg(row_size), factor_arg(scan_factor)],
            )
        },
    )
}

// go: netCostVer2 L1
pub fn net_cost_ver2(
    option: &PlanCostOption,
    rows: f64,
    row_size: f64,
    net_factor: &CostVer2Factor,
) -> CostVer2 {
    new_cost_ver2(
        Some(option),
        net_factor,
        rows * row_size * net_factor.value,
        || {
            sprintf_s(
                "net(%v*rowsize(%v)*%v)",
                &[f64_arg(rows), f64_arg(row_size), factor_arg(net_factor)],
            )
        },
    )
}

// go: filterCostVer2 L1
pub fn filter_cost_ver2(
    option: &PlanCostOption,
    rows: f64,
    filters: &[ExprRef],
    cpu_factor: &CostVer2Factor,
) -> CostVer2 {
    let num_funcs = num_functions(filters);
    new_cost_ver2(
        Some(option),
        cpu_factor,
        rows * num_funcs * cpu_factor.value,
        || {
            sprintf_s(
                "cpu(%v*filters(%v)*%v)",
                &[f64_arg(rows), f64_arg(num_funcs), factor_arg(cpu_factor)],
            )
        },
    )
}

// `aggCostVer2`, `groupCostVer2`: aggregation is not ported (see the module doc).

// go: numFunctions L1
pub fn num_functions(exprs: &[ExprRef]) -> f64 {
    let mut num = 0.0;
    for e in exprs {
        if let Expr::ScalarFunction(_) = &**e {
            num += 1.0;
        } else {
            // Column and Constant
            num += 0.01; // an empirical value
        }
    }
    num
}

// go: orderCostVer2 L1
pub fn order_cost_ver2(
    option: &PlanCostOption,
    rows: f64,
    n: f64,
    by_items: &[ByItems],
    cpu_factor: &CostVer2Factor,
) -> CostVer2 {
    let mut num_funcs: i64 = 0;
    for by_item in by_items {
        if let Expr::ScalarFunction(_) = &*by_item.expr {
            num_funcs += 1;
        }
    }
    let expr_cost = new_cost_ver2(
        Some(option),
        cpu_factor,
        rows * num_funcs as f64 * cpu_factor.value,
        || {
            sprintf_s(
                "exprCPU(%v*%v*%v)",
                &[
                    f64_arg(rows),
                    Arg::Int(num_funcs, "int"),
                    factor_arg(cpu_factor),
                ],
            )
        },
    );
    let order_cost = new_cost_ver2(
        Some(option),
        cpu_factor,
        math::max(rows * math::log2(n), 0.0) * cpu_factor.value,
        || {
            sprintf_s(
                "orderCPU(%v*log(%v)*%v)",
                &[f64_arg(rows), f64_arg(n), factor_arg(cpu_factor)],
            )
        },
    );
    sum_cost_ver2(&[expr_cost, order_cost])
}

// `hashBuildCostVer2`, `hashProbeCostVer2`: joins are not ported (see the module doc).

/// For simplicity and robust, only operators that need double-read like IndexLookup and IndexJoin consider this cost.
// go: doubleReadCostVer2 L1
pub fn double_read_cost_ver2(
    option: &PlanCostOption,
    num_tasks: f64,
    request_factor: &CostVer2Factor,
) -> CostVer2 {
    new_cost_ver2(
        Some(option),
        request_factor,
        num_tasks * request_factor.value,
        || {
            sprintf_s(
                "doubleRead(tasks(%v)*%v)",
                &[f64_arg(num_tasks), factor_arg(request_factor)],
            )
        },
    )
}

/// `getTableScanPenalty`. Go dereferences `p.tblColHists` without a check (FindBestTask
/// always sets it); so does this.
// go: getTableScanPenalty L1 [M-CTX, M-F2I]
pub fn get_table_scan_penalty(ctx: &PlanCtx<'_>, p: &PhysicalTableScan, rows: f64) -> f64 {
    // Apply cost penalty for full scans that carry high risk of underestimation. Exclude those
    // that are the child of an index scan or child is TableRangeScan
    if !p.range_info.is_empty() {
        return 0.0;
    }
    let session_vars = &*ctx.vars;
    let allow_prefer_range_scan = session_vars.get_allow_prefer_range_scan();
    let tbl_col_hists = p
        .tbl_col_hists
        .as_deref()
        .expect("runtime error: invalid memory address or nil pointer dereference");
    let original_rows = f64_to_i64(tbl_col_hists.get_analyze_row_count()); // [M-F2I]

    // hasUnreliableStats is a check for pseudo or zero stats
    let has_unreliable_stats = tbl_col_hists.pseudo || original_rows < 1;
    // hasHighModifyCount tracks the high risk of a tablescan where auto-analyze had not yet updated the table row count
    let has_high_modify_count = tbl_col_hists.modify_count > original_rows;
    // hasLowEstimate is a check to capture a unique customer case where modifyCount is used for tablescan estimate (but it not adequately understood why)
    let has_low_estimate = rows > 1.0
        && tbl_col_hists.modify_count < original_rows
        && f64_to_i64(rows) <= tbl_col_hists.modify_count; // [M-F2I]
    // preferRangeScan check here is same as in skylinePruning
    let prefer_range_scan_condition = allow_prefer_range_scan
        && (has_unreliable_stats || has_high_modify_count || has_low_estimate);

    // differentiate a FullTableScan from a partition level scan - so we shouldn't penalize these
    let mut has_partition_scan = false;
    if let Some(info) = &p.plan_part_info
        && !info.pruning_conds.is_empty()
    {
        has_partition_scan = true;
    }

    // GetIndexForce assumes that the USE/FORCE index is to force a range scan, and thus the
    // penalty is applied to a full table scan (not range scan). This may also penalize a
    // full table scan where USE/FORCE was applied to the primary key.
    let has_index_force = session_vars.stmt_ctx().get_index_force();
    let should_apply_penalty = has_index_force || prefer_range_scan_condition;
    if should_apply_penalty {
        // MySQL will increase the cost of table scan if FORCE index is used. TiDB takes this one
        // step further - because we don't differentiate USE/FORCE - the added penalty applies to
        // both, and it also applies to any full table scan in the query. Use "max" to get the minimum
        // number of rows to add as a penalty to the table scan.
        let min_rows = math::max(MaxPenaltyRowCount, rows);
        if has_partition_scan {
            return min_rows;
        }
        // If it isn't a partitioned table - choose the max that includes ModifyCount
        return math::max(min_rows, tbl_col_hists.modify_count as f64);
    }
    0.0
}

/// In Cost Ver2, we hide cost factors from users and deprecate SQL variables like `tidb_opt_scan_factor`.
#[derive(Clone, Debug)]
pub struct CostVer2Factors {
    /// operations on TiDB temporary table
    pub tidb_temp: CostVer2Factor,
    /// per byte
    pub tikv_scan: CostVer2Factor,
    /// per byte
    pub tikv_desc_scan: CostVer2Factor,
    /// per byte
    pub tiflash_scan: CostVer2Factor,
    /// per column or expression
    pub tidb_cpu: CostVer2Factor,
    /// per column or expression
    pub tikv_cpu: CostVer2Factor,
    /// per column or expression
    pub tiflash_cpu: CostVer2Factor,
    /// per byte
    pub tidb2_kv_net: CostVer2Factor,
    /// per byte
    pub tidb2_flash_net: CostVer2Factor,
    /// per byte
    pub tiflash_mpp_net: CostVer2Factor,
    /// per byte
    pub tidb_mem: CostVer2Factor,
    /// per byte
    pub tikv_mem: CostVer2Factor,
    /// per byte
    pub tiflash_mem: CostVer2Factor,
    /// per byte
    pub tidb_disk: CostVer2Factor,
    /// per net request
    pub tidb_request: CostVer2Factor,
    /// ANN index's warmup cost, related to row num.
    pub ann_index_start: CostVer2Factor,
    /// ANN index's scan cost, by row.
    pub ann_index_scan_row: CostVer2Factor,
    /// special factor for ANN index without top-k: max uint64
    pub ann_index_no_topk: CostVer2Factor,
}

// `costVer2Factors.tolist`: only Go tests call it.

/// A factor of the table.
// glue: Go composite literal costusage.CostVer2Factor{Name: ..., Value: ...}
fn factor(name: &str, value: f64) -> CostVer2Factor {
    CostVer2Factor {
        name: name.to_string(),
        value,
    }
}

/// `defaultVer2Factors`.
pub static defaultVer2Factors: LazyLock<CostVer2Factors> = LazyLock::new(|| CostVer2Factors {
    tidb_temp: factor("tidb_temp_table_factor", 0.00),
    tikv_scan: factor("tikv_scan_factor", 40.70),
    tikv_desc_scan: factor("tikv_desc_scan_factor", 61.05),
    tiflash_scan: factor("tiflash_scan_factor", 11.60),
    tidb_cpu: factor("tidb_cpu_factor", 49.90),
    tikv_cpu: factor("tikv_cpu_factor", 49.90),
    tiflash_cpu: factor("tiflash_cpu_factor", 2.40),
    tidb2_kv_net: factor("tidb_kv_net_factor", 3.96),
    tidb2_flash_net: factor("tidb_flash_net_factor", 2.20),
    tiflash_mpp_net: factor("tiflash_mpp_net_factor", 1.00),
    tidb_mem: factor("tidb_mem_factor", 0.20),
    tikv_mem: factor("tikv_mem_factor", 0.20),
    tiflash_mem: factor("tiflash_mem_factor", 0.05),
    tidb_disk: factor("tidb_disk_factor", 200.00),
    tidb_request: factor("tidb_request_factor", 6000000.00),
    ann_index_start: factor("ann_index_start_factor", 0.000144),
    ann_index_scan_row: factor("ann_index_scan_factor", 1.65),
    ann_index_no_topk: factor("ann_index_no_topk_factor", u64::MAX as f64),
});

// go: getTaskCPUFactorVer2 L1
pub fn get_task_cpu_factor_ver2(
    _p: &dyn PhysicalPlan,
    task_type: TaskType,
) -> &'static CostVer2Factor {
    match task_type {
        RootTaskType => &defaultVer2Factors.tidb_cpu,   // TiDB
        MppTaskType => &defaultVer2Factors.tiflash_cpu, // TiFlash
        _ => &defaultVer2Factors.tikv_cpu,              // TiKV
    }
}

// go: getTaskMemFactorVer2 L1
pub fn get_task_mem_factor_ver2(
    _p: &dyn PhysicalPlan,
    task_type: TaskType,
) -> &'static CostVer2Factor {
    match task_type {
        RootTaskType => &defaultVer2Factors.tidb_mem,   // TiDB
        MppTaskType => &defaultVer2Factors.tiflash_mem, // TiFlash
        _ => &defaultVer2Factors.tikv_mem,              // TiKV
    }
}

/// `getTaskScanFactorVer2`. The `*PhysicalIndexScan` assertion (its `Desc`) is left out: the
/// index scan is not ported (see the module doc).
// go: getTaskScanFactorVer2 L1
pub fn get_task_scan_factor_ver2(
    p: &dyn PhysicalPlan,
    store_type: StoreType,
    task_type: TaskType,
) -> &'static CostVer2Factor {
    if is_temporary_table(get_table_info(p)) {
        return &defaultVer2Factors.tidb_temp;
    }
    if store_type == kv::TiFlash {
        return &defaultVer2Factors.tiflash_scan;
    }
    match task_type {
        MppTaskType => &defaultVer2Factors.tiflash_scan, // TiFlash
        _ => {
            // TiKV
            let mut desc = false;
            if let Some(index_scan) = p.as_any().downcast_ref::<PhysicalIndexScan>() {
                desc = index_scan.desc;
            }
            if let Some(table_scan) = p.as_any().downcast_ref::<PhysicalTableScan>() {
                desc = table_scan.desc;
            }
            if desc {
                return &defaultVer2Factors.tikv_desc_scan;
            }
            &defaultVer2Factors.tikv_scan
        }
    }
}

/// `getTaskNetFactorVer2`. The `*PhysicalExchangeReceiver` arm and the TableReader over a
/// `*PhysicalExchangeSender` (TiDB to TiFlash with the MPP protocol) are left out: the MPP
/// operators are not ported (see the module doc).
// go: getTaskNetFactorVer2 L1
pub fn get_task_net_factor_ver2(
    p: &dyn PhysicalPlan,
    _task_type: TaskType,
) -> &'static CostVer2Factor {
    if is_temporary_table(get_table_info(p)) {
        return &defaultVer2Factors.tidb_temp;
    }
    &defaultVer2Factors.tidb2_kv_net
}

// go: getTaskRequestFactorVer2 L1
pub fn get_task_request_factor_ver2(
    p: &dyn PhysicalPlan,
    _task_type: TaskType,
) -> &'static CostVer2Factor {
    if is_temporary_table(get_table_info(p)) {
        return &defaultVer2Factors.tidb_temp;
    }
    &defaultVer2Factors.tidb_request
}

// go: isTemporaryTable L1
pub fn is_temporary_table(tbl: Option<&TableInfo>) -> bool {
    tbl.is_some_and(|tbl| tbl.temp_table_type != TempTableNone)
}

/// `getTableInfo`. The index reader, index lookup, index merge and index scan arms are left
/// out: those operators are not ported (see the module doc).
// go: getTableInfo L1
pub fn get_table_info(p: &dyn PhysicalPlan) -> Option<&TableInfo> {
    match p.kind() {
        PhysicalKind::IndexReader => {
            let x = p
                .as_any()
                .downcast_ref::<PhysicalIndexReader>()
                .expect("type assertion");
            get_table_info(
                x.index_plan
                    .as_deref()
                    .expect("runtime error: invalid memory address or nil pointer dereference"),
            )
        }
        PhysicalKind::TableReader => {
            let x = p
                .as_any()
                .downcast_ref::<PhysicalTableReader>()
                .expect("type assertion");
            get_table_info(
                x.table_plan
                    .as_deref()
                    .expect("runtime error: invalid memory address or nil pointer dereference"),
            )
        }
        PhysicalKind::TableScan => {
            let x = p
                .as_any()
                .downcast_ref::<PhysicalTableScan>()
                .expect("type assertion");
            Some(&x.table)
        }
        PhysicalKind::IndexScan => {
            let x = p
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .expect("type assertion");
            Some(&x.table)
        }
        _ => {
            if p.children().is_empty() {
                return None;
            }
            get_table_info(p.children()[0].as_ref())
        }
    }
}
