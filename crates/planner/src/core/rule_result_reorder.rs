//! go-file: pkg/planner/core/rule_result_reorder.go
//!
//! The rule that stabilizes query results (`tidb_enable_stable_result_mode`, block D09.13):
//! the optimizer driver adds its flag (`FlagStabilizeResults`) in `adjustOptimizationFlags`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::{Column, Expr};

use crate::arena::PlanId;
use crate::base::{LogicalKind, LogicalOptRule};
use crate::logicalop::{DataSource, LogicalSort};
use crate::planctx::PlanCtx;
use crate::util::ByItems;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// ResultReorder reorder query results.
/// NOTE: it's not a common rule for all queries, it's specially implemented for a few customers.
///
/// Results of some queries are not ordered, for example:
///
/// ```text
/// create table t (a int); insert into t values (1), (2); select a from t;
/// ```
///
/// In the case above, the result can be `1 2` or `2 1`, which is not ordered.
/// This rule reorders results by modifying or injecting a Sort operator:
///  1. iterate the plan from the root, and ignore all input-order operators (Sel/Proj/Limit);
///  2. when meeting the first non-input-order operator,
///     2.1. if it's a Sort, update it by appending all output columns into its order-by list,
///     2.2. otherwise, inject a new Sort upon this operator.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResultReorder;

impl LogicalOptRule for ResultReorder {
    /// Optimize implements base.LogicalOptRule.<0th> interface.
    // go: ResultReorder.Optimize L1
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        lp: PlanId,
        _opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        let mut lp = lp;
        let ordered = self.complete_sort(ctx, lp);
        if !ordered {
            lp = self.inject_sort(ctx, lp);
        }
        Ok((lp, plan_changed))
    }

    /// Name implements base.LogicalOptRule.<1st> interface.
    // go: ResultReorder.Name L1
    fn name(&self) -> &'static str {
        "result_reorder"
    }
}

impl ResultReorder {
    // go: ResultReorder.completeSort L1 [M-PLAN]
    fn complete_sort(&self, ctx: &mut PlanCtx<'_>, lp: PlanId) -> bool {
        if self.is_input_order_keeper(ctx, lp) {
            let children = ctx.arena.get(lp).children();
            if children.is_empty() {
                return true;
            }
            let child = children[0];
            return self.complete_sort(ctx, child);
        } else if ctx.arena.get(lp).kind() == LogicalKind::Sort {
            // sort results by all output columns
            let mut cols: Vec<Arc<Column>> = ctx.arena.get(lp).schema(&ctx.arena).columns.clone();
            let child = ctx.arena.get(lp).children()[0];
            if let Some(handle_col) = self.extract_handle_col(ctx, child) {
                cols = vec![handle_col]; // sort results by the handle column if we can get it
            }
            let sort = ctx
                .arena
                .downcast_mut::<LogicalSort>(lp)
                .expect("a LogicalSort");
            for col in cols {
                let mut exist = false;
                for by_item in &sort.by_items {
                    if col.equal_column(&by_item.expr) {
                        exist = true;
                        break;
                    }
                }
                if !exist {
                    sort.by_items.push(ByItems {
                        expr: Arc::new(Expr::Column(Column::clone(&col))),
                        desc: false,
                    });
                }
            }
            return true;
        }
        false
    }

    // go: ResultReorder.injectSort L1 [M-PLAN]
    fn inject_sort(&self, ctx: &mut PlanCtx<'_>, lp: PlanId) -> PlanId {
        if self.is_input_order_keeper(ctx, lp) {
            let child = ctx.arena.get(lp).children()[0];
            let new_child = self.inject_sort(ctx, child);
            ctx.arena.get_mut(lp).set_children(vec![new_child]);
            return lp;
        }

        let node = ctx.arena.get(lp);
        let mut cols: Vec<Arc<Column>> = node.schema(&ctx.arena).columns.clone();
        let mut by_items = Vec::with_capacity(cols.len());
        if let Some(handle_col) = self.extract_handle_col(ctx, lp) {
            cols = vec![handle_col];
        }
        for col in cols {
            by_items.push(ByItems {
                expr: Arc::new(Expr::Column(Column::clone(&col))),
                desc: false,
            });
        }
        let qb_offset = ctx.arena.get(lp).query_block_offset();
        let mut sort = LogicalSort::init(by_items, ctx, qb_offset);
        sort.base.set_children(vec![lp]);
        ctx.arena.alloc(Box::new(sort))
    }

    // go: ResultReorder.isInputOrderKeeper L1 [M-IFACE]
    fn is_input_order_keeper(&self, ctx: &PlanCtx<'_>, lp: PlanId) -> bool {
        matches!(
            ctx.arena.get(lp).kind(),
            LogicalKind::Selection
                | LogicalKind::Projection
                | LogicalKind::Limit
                | LogicalKind::TableDual
        )
    }

    /// extractHandleCols does the best effort to get the handle column.
    // go: ResultReorder.extractHandleCol L1 [M-IFACE]
    fn extract_handle_col(&self, ctx: &PlanCtx<'_>, lp: PlanId) -> Option<Arc<Column>> {
        let node = ctx.arena.get(lp);
        match node.kind() {
            LogicalKind::Selection | LogicalKind::Limit => {
                // fail to extract handle column from the child, just return nil.
                let handle_col = self.extract_handle_col(ctx, node.children()[0])?;
                if node.schema(&ctx.arena).contains(&handle_col) {
                    // some Projection Operator might be inlined, so check the column again here
                    return Some(handle_col);
                }
            }
            LogicalKind::DataSource => {
                let x = node
                    .as_any()
                    .downcast_ref::<DataSource>()
                    .expect("a DataSource");
                if x.table_info.is_common_handle {
                    // Currently we deliberately don't support common handle case for simplicity.
                    return None;
                }
                if let Some(handle_col) = x.get_pk_is_handle_col(ctx) {
                    return Some(handle_col);
                }
            }
            _ => {}
        }
        None
    }
}
