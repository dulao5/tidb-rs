//! go-file: pkg/planner/core/operator/logicalop/logical_limit.go
//!
//! `LogicalLimit`: OFFSET / LIMIT.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::{Column, ExprRef};

use crate::arena::PlanId;
use crate::base::{self, LogicalPlan};
use crate::logicalop::logical_schema_producer::{self, inline_projection};
use crate::logicalop::{
    BaseLogicalPlan, LogicalSchemaProducer, LogicalTopN, new_base_logical_plan,
};
use crate::planctx::PlanCtx;
use crate::property::SortItem;
use crate::util::explain_misc::explain_partition_by;
use crate::util::logicaltrace::{TraceId, trace_id};
use crate::util::optimizetrace::LogicalOptimizeOp;

/// LogicalLimit represents offset and limit plan.
pub struct LogicalLimit {
    /// The embedded `BaseLogicalPlan`.
    pub base: BaseLogicalPlan,
    /// The embedded `LogicalSchemaProducer`.
    pub sp: LogicalSchemaProducer,
    /// PartitionBy is used for enhanced topN optimization.
    pub partition_by: Vec<SortItem>,
    /// Offset.
    pub offset: u64,
    /// Count.
    pub count: u64,
    /// PreferLimitToCop.
    pub prefer_limit_to_cop: bool,
    /// IsPartial.
    pub is_partial: bool,
}

impl LogicalLimit {
    /// Init initializes LogicalLimit (with the struct literal's fields).
    // go: LogicalLimit.Init L1 [M-SELF]
    pub fn init(offset: u64, count: u64, ctx: &PlanCtx<'_>, qb_offset: isize) -> LogicalLimit {
        LogicalLimit {
            base: new_base_logical_plan(ctx, "Limit", qb_offset),
            sp: LogicalSchemaProducer::new(None),
            partition_by: Vec::new(),
            offset,
            count,
            prefer_limit_to_cop: false,
            is_partial: false,
        }
    }

    /// GetPartitionBy returns partition by fields
    // go: LogicalLimit.GetPartitionBy L1
    pub fn get_partition_by(&self) -> &[SortItem] {
        &self.partition_by
    }

    // go: LogicalLimit.convertToTopN L1 [M-PLAN]
    fn convert_to_top_n(&self, ctx: &mut PlanCtx<'_>, opt: &LogicalOptimizeOp) -> PlanId {
        let mut topn = LogicalTopN::init(
            Vec::new(),
            self.offset,
            self.count,
            ctx,
            self.query_block_offset(),
        );
        topn.prefer_limit_to_cop = self.prefer_limit_to_cop;
        append_convert_top_n_trace_step(trace_id(self), trace_id(&topn), opt);
        ctx.arena.alloc(Box::new(topn))
    }

    /// BuildKeyInfo implements base.LogicalPlan.<4th> interface.
    // go: LogicalLimit.BuildKeyInfo L1 [M-PLAN]
    pub fn build_key_info(&mut self, arena: &mut crate::PlanArena) {
        logical_schema_producer::build_key_info(self, arena);
        if self.count == 1 {
            self.base.set_max_one_row(true);
        }
    }
}

// go-exception: EXC.B LogicalLimit.Hash64 (cascades base.HashEquals; D08.1 spec §4 item 2)
// go-exception: EXC.B LogicalLimit.Equals (cascades base.HashEquals; D08.1 spec §4 item 2)
// glue: stub of LogicalLimit.HashCode, blocked-by:D14.2 (plancodec.TypeStringToPhysicalID, util/plancodec)
/// The boundary of the stubbed `LogicalLimit.HashCode`.
pub fn limit_hash_code_boundary() -> Error {
    blocked(
        "D14.2",
        "plancodec.TypeStringToPhysicalID (LogicalLimit.HashCode)",
    )
}

impl LogicalPlan for LogicalLimit {
    crate::logical_plan_boilerplate!(Limit, producer);

    /// ExhaustPhysicalPlans implements base.LogicalPlan.<14th> interface.
    // go: LogicalLimit.ExhaustPhysicalPlans L1 [M-PLAN]
    fn exhaust_physical_plans(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        prop: &crate::property::PhysicalProperty,
    ) -> Result<(Vec<crate::PhysicalPlanRef>, bool), Error> {
        crate::core::exhaust_physical_plans::exhaust_physical_plans4_logical_limit(self, ctx, prop)
    }

    /// ExplainInfo implements Plan interface.
    // go: LogicalLimit.ExplainInfo L1
    fn explain_info(&self, ctx: &PlanCtx<'_>) -> Result<String, Error> {
        let ectx = ctx.eval_ctx();
        let mut buffer = String::new();
        if !self.get_partition_by().is_empty() {
            explain_partition_by(&*ectx, &mut buffer, self.get_partition_by(), false);
            buffer.push_str(&format!(", offset:{}, count:{}", self.offset, self.count));
        } else {
            buffer.push_str(&format!("offset:{}, count:{}", self.offset, self.count));
        }
        Ok(buffer)
    }

    /// HashCode implements LogicalPlan.<0th> interface.
    // glue: dispatch to the HashCode boundary stub above
    fn hash_code(&self) -> Result<Vec<u8>, Error> {
        Err(limit_hash_code_boundary())
    }

    /// PredicatePushDown implements base.LogicalPlan.<1st> interface.
    // go: LogicalLimit.PredicatePushDown L1
    fn predicate_push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<ExprRef>,
        opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        // Limit forbids any condition to push down.
        crate::logicalop::base_logical_plan::predicate_push_down(self, ctx, this, Vec::new(), opt)?;
        Ok((predicates, this))
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: LogicalLimit.PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let saved_used_cols = parent_used_cols.clone();
        let child = self.children()[0];
        self.children_mut()[0] = base::prune_columns(ctx, child, parent_used_cols, opt)?;
        self.sp.set_schema(None);
        inline_projection(self, ctx, &saved_used_cols, opt);
        Ok(this)
    }

    /// PushDownTopN implements the base.LogicalPlan.<5th> interface.
    // go: LogicalLimit.PushDownTopN L1 [M-PLAN]
    fn push_down_top_n(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        top_n: Option<PlanId>,
        opt: &LogicalOptimizeOp,
    ) -> PlanId {
        let converted = self.convert_to_top_n(ctx, opt);
        let child = base::push_down_top_n(ctx, self.children()[0], Some(converted), opt);
        if let Some(top_n) = top_n {
            return LogicalTopN::attach_child(ctx, top_n, child, opt);
        }
        child
    }
}

// go: appendConvertTopNTraceStep L1 [M-CTX]
fn append_convert_top_n_trace_step(p: TraceId, top_n: TraceId, opt: &LogicalOptimizeOp) {
    let reason = String::new;
    let action = || format!("{}_{} is converted into {}_{}", p.0, p.1, top_n.0, top_n.1);
    opt.append_step_to_current(top_n.1 as isize, top_n.0, reason, action);
}
