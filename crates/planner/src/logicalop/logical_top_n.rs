//! go-file: pkg/planner/core/operator/logicalop/logical_top_n.go
//!
//! `LogicalTopN`: ORDER BY with LIMIT, made by TopN push down. No schema of its own.

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::util::extract_cor_columns;
use tidb_expression::{Column, CorrelatedColumn};

use crate::arena::PlanId;
use crate::base::{self, LogicalKind, LogicalPlan, ReplaceMap};
use crate::core::rule_util::resolve_expr_and_replace;
use crate::logicalop::logical_plans_misc::prune_by_items;
use crate::logicalop::{BaseLogicalPlan, LogicalLimit, LogicalTableDual, new_base_logical_plan};
use crate::planctx::PlanCtx;
use crate::property::SortItem;
use crate::util::ByItems;
use crate::util::explain_misc::{explain_by_items, explain_partition_by};
use crate::util::logicaltrace::{TraceId, trace_id};
use crate::util::optimizetrace::LogicalOptimizeOp;

/// LogicalTopN represents a top-n plan.
pub struct LogicalTopN {
    /// The embedded `BaseLogicalPlan`.
    pub base: BaseLogicalPlan,
    /// ByItems.
    pub by_items: Vec<ByItems>,
    /// PartitionBy is used for extended TopN to consider K heaps. Used by rule_derive_topn_from_window
    pub partition_by: Vec<SortItem>,
    /// Offset.
    pub offset: u64,
    /// Count.
    pub count: u64,
    /// PreferLimitToCop.
    pub prefer_limit_to_cop: bool,
}

impl LogicalTopN {
    /// Init initializes LogicalTopN (with the struct literal's main fields).
    // go: LogicalTopN.Init L1 [M-SELF]
    pub fn init(
        by_items: Vec<ByItems>,
        offset: u64,
        count: u64,
        ctx: &PlanCtx<'_>,
        qb_offset: isize,
    ) -> LogicalTopN {
        LogicalTopN {
            base: new_base_logical_plan(ctx, "TopN", qb_offset),
            by_items,
            partition_by: Vec::new(),
            offset,
            count,
            prefer_limit_to_cop: false,
        }
    }

    /// GetPartitionBy returns partition by fields
    // go: LogicalTopN.GetPartitionBy L1
    pub fn get_partition_by(&self) -> &[SortItem] {
        &self.partition_by
    }

    /// IsLimit checks if TopN is a limit plan.
    // go: LogicalTopN.IsLimit L1
    pub fn is_limit(&self) -> bool {
        self.by_items.is_empty()
    }

    /// AttachChild set p as topn's child, for difference with LogicalPlan.SetChild().
    /// AttachChild will tracer the children change while SetChild doesn't. `lt` is the
    /// TopN in the arena, `p` the new child (in the arena); returns the new root.
    // glue: N1 (dispatch of LogicalTopN.AttachChild to a child stored in the arena)
    pub fn attach_child(
        ctx: &mut PlanCtx<'_>,
        lt: PlanId,
        p: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> PlanId {
        crate::with_detached(ctx, p, |node, ctx| {
            Self::attach_child_to(ctx, lt, p, node, opt)
        })
    }

    /// AttachChild set p as topn's child, for difference with LogicalPlan.SetChild().
    /// AttachChild will tracer the children change while SetChild doesn't. `lt` is the
    /// TopN in the arena; `p_node` is the new child `p`, detached (Go passes the plan, which
    /// is often the caller itself); returns the new root.
    // go: LogicalTopN.AttachChild L1 [M-PLAN]
    pub fn attach_child_to<P: LogicalPlan + ?Sized>(
        ctx: &mut PlanCtx<'_>,
        lt: PlanId,
        p: PlanId,
        p_node: &mut P,
        opt: &LogicalOptimizeOp,
    ) -> PlanId {
        let (offset, count) = {
            let t = ctx
                .arena
                .downcast_ref::<LogicalTopN>(lt)
                .expect("LogicalTopN");
            (t.offset, t.count)
        };
        // Remove this TopN if its child is a TableDual.
        if p_node.kind() == LogicalKind::TableDual {
            let dual = p_node
                .as_any_mut()
                .downcast_mut::<LogicalTableDual>()
                .expect("LogicalTableDual");
            let num_dual_rows = dual.row_count as u64;
            if num_dual_rows < offset {
                dual.row_count = 0;
                return p;
            }
            dual.row_count = (num_dual_rows - offset).min(count) as isize;
            return p;
        }
        let p_tid = trace_id(p_node);
        let t = ctx
            .arena
            .downcast_ref::<LogicalTopN>(lt)
            .expect("LogicalTopN");
        if t.is_limit() {
            let mut limit = LogicalLimit::init(t.offset, t.count, ctx, t.query_block_offset());
            limit.prefer_limit_to_cop = t.prefer_limit_to_cop;
            limit.partition_by = t.get_partition_by().to_vec();
            limit.set_children(vec![p]);
            let limit_tid = trace_id(&limit);
            let limit = ctx.arena.alloc(Box::new(limit));
            append_top_n_push_down_trace_step(limit_tid, p_tid, opt);
            return limit;
        }
        // Then lt must be topN.
        let t = ctx
            .arena
            .downcast_mut::<LogicalTopN>(lt)
            .expect("LogicalTopN");
        t.set_children(vec![p]);
        let lt_tid = trace_id(t);
        append_top_n_push_down_trace_step(lt_tid, p_tid, opt);
        lt
    }

    /// BuildKeyInfo implements base.LogicalPlan.<4th> interface.
    // go: LogicalTopN.BuildKeyInfo L1
    pub fn build_key_info(&mut self, arena: &crate::PlanArena) {
        crate::logicalop::base_logical_plan::build_key_info(self, arena);
        if self.count == 1 {
            self.base.set_max_one_row(true);
        }
    }
}

impl LogicalPlan for LogicalTopN {
    crate::logical_plan_boilerplate!(TopN);

    /// DeriveStats implements base.LogicalPlan.<11th> interface.
    // go: LogicalTopN.DeriveStats L1
    fn derive_stats(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        child_stats: &[Arc<crate::property::StatsInfo>],
        _col_groups: &[Vec<Arc<Column>>],
    ) -> Result<Arc<crate::property::StatsInfo>, Error> {
        if let Some(s) = self.base.plan.stats_info() {
            return Ok(Arc::clone(s));
        }
        let stats = Arc::new(crate::util::misc::derive_limit_stats(
            &child_stats[0],
            self.count as f64,
        ));
        self.base.plan.set_stats(Some(Arc::clone(&stats)));
        Ok(stats)
    }

    /// PreparePossibleProperties implements base.LogicalPlan.<13th> interface.
    // go: LogicalTopN.PreparePossibleProperties L1
    fn prepare_possible_properties(
        &self,
        _arena: &crate::PlanArena,
        _children_properties: Vec<Vec<Vec<Arc<Column>>>>,
    ) -> Vec<Vec<Arc<Column>>> {
        let prop_cols =
            crate::logicalop::logical_sort::get_possible_property_from_by_items(&self.by_items);
        if prop_cols.is_empty() {
            return Vec::new();
        }
        vec![prop_cols]
    }

    /// ExhaustPhysicalPlans implements base.LogicalPlan.<14th> interface.
    // go: LogicalTopN.ExhaustPhysicalPlans L1 [M-PLAN]
    fn exhaust_physical_plans(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        prop: &crate::property::PhysicalProperty,
    ) -> Result<(Vec<crate::PhysicalPlanRef>, bool), Error> {
        crate::core::exhaust_physical_plans::exhaust_physical_plans4_logical_top_n(self, ctx, prop)
    }

    /// ExplainInfo implements Plan interface.
    // go: LogicalTopN.ExplainInfo L1
    fn explain_info(&self, ctx: &PlanCtx<'_>) -> Result<String, Error> {
        let ectx = ctx.eval_ctx();
        let mut buffer = String::new();
        explain_partition_by(&*ectx, &mut buffer, self.get_partition_by(), false);
        if !self.get_partition_by().is_empty() && !self.by_items.is_empty() {
            buffer.push_str("order by ");
        }
        explain_by_items(&*ectx, &mut buffer, &self.by_items)?;
        buffer.push_str(&format!(", offset:{}, count:{}", self.offset, self.count));
        Ok(buffer)
    }

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: LogicalTopN.ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for by_item in self.by_items.iter_mut() {
            resolve_expr_and_replace(&mut by_item.expr, replace);
        }
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    /// If any expression can view as a constant in execution stage, such as correlated column, constant,
    /// we do prune them. Note that we can't prune the expressions contain non-deterministic functions, such as rand().
    // go: LogicalTopN.PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let child = self.children()[0];
        let (by_items, cols) = prune_by_items(self, ctx, &self.by_items, opt);
        self.by_items = by_items;
        parent_used_cols.extend(cols);
        self.children_mut()[0] = base::prune_columns(ctx, child, parent_used_cols, opt)?;
        Ok(this)
    }

    /// ExtractCorrelatedCols implements base.LogicalPlan.<15th> interface.
    // go: LogicalTopN.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(self.by_items.len());
        for item in &self.by_items {
            cor_cols.extend(extract_cor_columns(&item.expr));
        }
        cor_cols
    }
}

// go: appendTopNPushDownTraceStep L1 [M-CTX]
fn append_top_n_push_down_trace_step(parent: TraceId, child: TraceId, opt: &LogicalOptimizeOp) {
    let action = || {
        format!(
            "{}_{} is added as {}_{}'s parent",
            parent.0, parent.1, child.0, child.1
        )
    };
    let reason = || format!("{} is pushed down", parent.0);
    opt.append_step_to_current(parent.1 as isize, parent.0, reason, action);
}
