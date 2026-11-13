//! go-file: pkg/planner/core/base/plan_base.go (spike: the part of `LogicalPlan` D09.7 uses)
//!
//! Mapping (N1):
//! - `base.LogicalPlan` (interface) is the [`LogicalPlan`] trait; nodes live in the
//!   [`PlanArena`] and refer to each other by [`PlanId`] (`M-PLAN`).
//! - Go's `BaseLogicalPlan.self` back-pointer and struct embedding
//!   (`LogicalProjection` → `LogicalSchemaProducer` → `BaseLogicalPlan`) become trait default
//!   methods plus two accessors, `base()` and `schema_producer()` (`M-SELF`).
//! - A method of Go's interface is called on a node in the arena by a free function of the
//!   same name (e.g. [`prune_columns`]) that detaches the node and calls the trait method
//!   with `&mut self` and `&mut PlanCtx`.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::{Column, Schema, get_used_list};

use crate::arena::{PlanArena, PlanId, with_detached};
use crate::logicalop::{BaseLogicalPlan, LogicalSchemaProducer};
use crate::optimizetrace::{LogicalOptimizeOp, logicaltrace};
use crate::planctx::PlanCtx;

/// Operator kinds, for Go's `switch x := p.(type)` (`M-IFACE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalKind {
    /// `*DataSource`.
    DataSource,
    /// `*LogicalSelection`.
    Selection,
    /// `*LogicalProjection`.
    Projection,
    /// `*LogicalLimit`.
    Limit,
    /// `*LogicalSort`.
    Sort,
    /// `*LogicalTopN`.
    TopN,
    /// `*LogicalAggregation`.
    Aggregation,
    /// `*LogicalJoin`.
    Join,
    /// `*LogicalTableDual`.
    TableDual,
}

/// The map of `ReplaceExprColumns`: `string(col.HashCode())` → column.
pub type ReplaceMap = HashMap<Vec<u8>, Arc<Column>>;

/// LogicalPlan is a tree of logical operators.
pub trait LogicalPlan: Any + Send {
    // ---- per-type boilerplate (M-IFACE / M-SELF) ----

    /// The operator kind (`M-IFACE`).
    // glue: N1
    fn kind(&self) -> LogicalKind;
    /// For type assertions.
    // glue: N1
    fn as_any(&self) -> &dyn Any;
    /// For type assertions.
    // glue: N1
    fn as_any_mut(&mut self) -> &mut dyn Any;
    /// The embedded `BaseLogicalPlan`.
    // glue: N1 (Go struct embedding)
    fn base(&self) -> &BaseLogicalPlan;
    /// The embedded `BaseLogicalPlan`.
    // glue: N1 (Go struct embedding)
    fn base_mut(&mut self) -> &mut BaseLogicalPlan;
    /// The embedded `LogicalSchemaProducer`, for operators that embed one.
    // glue: N1 (Go struct embedding)
    fn schema_producer(&self) -> Option<&LogicalSchemaProducer> {
        None
    }
    /// The embedded `LogicalSchemaProducer`, for operators that embed one.
    // glue: N1 (Go struct embedding)
    fn schema_producer_mut(&mut self) -> Option<&mut LogicalSchemaProducer> {
        None
    }

    // ---- base.Plan ----

    /// ID implements Plan ID interface (Go's plan id, not the arena index).
    // go: pkg/planner/core/operator/baseimpl/plan.go (*Plan).ID L1
    fn id(&self) -> i32 {
        self.base().id
    }

    /// TP implements Plan interface.
    // go: pkg/planner/core/operator/baseimpl/plan.go (*Plan).TP L1
    fn tp(&self) -> &'static str {
        self.base().tp
    }

    /// Children implements LogicalPlan Children interface.
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go (*BaseLogicalPlan).Children L1 [M-PLAN]
    fn children(&self) -> &[PlanId] {
        &self.base().children
    }

    /// `p.Children()[i] = ...` (Go writes through the returned slice).
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go (*BaseLogicalPlan).Children L1 [M-PLAN]
    fn children_mut(&mut self) -> &mut [PlanId] {
        &mut self.base_mut().children
    }

    /// SetChildren implements LogicalPlan SetChildren interface.
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go (*BaseLogicalPlan).SetChildren L1 [M-PLAN]
    fn set_children(&mut self, children: Vec<PlanId>) {
        self.base_mut().children = children;
    }

    /// Schema implements Plan Schema interface. `BaseLogicalPlan` returns its child's schema;
    /// `LogicalSchemaProducer` its own (lazily cloned from the only child).
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go (*BaseLogicalPlan).Schema L1 [M-PLAN, M-SELF]
    fn schema<'a>(&'a self, arena: &'a PlanArena) -> &'a Schema {
        match self.schema_producer() {
            Some(sp) => sp.schema(self.children(), arena),
            None => arena.get(self.children()[0]).schema(arena),
        }
    }

    /// `p.Schema()` for writing (Go writes through the returned `*Schema`).
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go (*BaseLogicalPlan).Schema L1 [M-PLAN, M-SELF]
    fn schema_mut<'a>(&'a mut self, arena: &'a mut PlanArena) -> &'a mut Schema {
        if self.schema_producer().is_some() {
            self.schema_producer()
                .expect("producer")
                .schema(self.children(), arena);
            return self
                .schema_producer_mut()
                .expect("producer")
                .schema_cell_mut();
        }
        let child = self.children()[0];
        arena.schema_mut(child)
    }

    // ---- base.LogicalPlan ----

    /// PruneColumns prunes the unused columns, and return the new logical plan if changed,
    /// otherwise it's same.
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go (*BaseLogicalPlan).PruneColumns L1 [M-PLAN, M-SELF]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        if self.children().is_empty() {
            return Ok(this);
        }
        self.children_mut()[0] = prune_columns(ctx, self.children()[0], parent_used_cols, opt)?;
        Ok(this)
    }

    /// ReplaceExprColumns replace all the column reference in the plan's expression node.
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go (*BaseLogicalPlan).ReplaceExprColumns L1
    fn replace_expr_columns(&mut self, _replace: &ReplaceMap) {}

    /// InlineProjection prunes unneeded columns inline an executor (a method of
    /// `LogicalSchemaProducer`; it calls back `s.Self()` for the trace, hence a default
    /// method here).
    // go: pkg/planner/core/operator/logicalop/logical_schema_producer.go (*LogicalSchemaProducer).InlineProjection L1 [M-PLAN, M-SELF]
    fn inline_projection(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) {
        let mut pruned_columns = Vec::new();
        let ectx = ctx.eval_ctx();
        let mut used = get_used_list(&*ectx, &parent_used_cols, self.schema(&ctx.arena));
        if parent_used_cols.is_empty() {
            // When this operator output no columns, we return its smallest column for safety.
            let mut min_col_len = isize::MAX;
            let mut chosen_pos = 0;
            for (i, col) in self.schema(&ctx.arena).columns.iter().enumerate() {
                let flen = col.get_type().get_flen();
                if flen < min_col_len {
                    chosen_pos = i;
                    min_col_len = flen;
                }
            }
            // It should be always true.
            if !used.is_empty() {
                used[chosen_pos] = true;
            }
        }
        for i in (0..used.len()).rev() {
            if !used[i] {
                pruned_columns.push(self.schema(&ctx.arena).columns[i].clone());
                self.schema_mut(&mut ctx.arena).columns.remove(i);
            }
        }
        logicaltrace::append_column_prune_trace_step(self, &pruned_columns, opt);
    }
}

/// `p.PruneColumns(parentUsedCols, opt)` on a node in the arena: detaches `p` and calls
/// [`LogicalPlan::prune_columns`].
// glue: N1 (interface dispatch, see module doc)
pub fn prune_columns(
    ctx: &mut PlanCtx<'_>,
    p: PlanId,
    parent_used_cols: Vec<Arc<Column>>,
    opt: &mut LogicalOptimizeOp,
) -> Result<PlanId, Error> {
    with_detached(ctx, p, |node, ctx| {
        node.prune_columns(ctx, p, parent_used_cols, opt)
    })
}

/// LogicalOptRule means a logical optimizing rule, which contains de-correlate, ppd, column
/// pruning, etc.
pub trait LogicalOptRule {
    /// Optimize returns the new root, whether the plan changed, and an error.
    // glue: interface method of base.LogicalOptRule
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error>;
    /// Name of the rule.
    // glue: interface method of base.LogicalOptRule
    fn name(&self) -> &'static str;
}
