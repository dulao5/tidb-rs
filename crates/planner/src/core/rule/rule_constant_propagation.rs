//! go-file: pkg/planner/core/rule/rule_constant_propagation.go
//!
//! The constant propagation rule (block D09.13). Only `LogicalJoin` (D09.3) implements
//! `ConstantPropagation`; every ported operator takes the base method, which does nothing.

use tidb_base::Error;

use crate::arena::PlanId;
use crate::base::LogicalOptRule;
use crate::core::optimizer::dispatch_constant_propagation;
use crate::planctx::PlanCtx;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// ConstantPropagationSolver can support constant propagated cross-query block.
/// This is a logical optimize rule.
/// It mainly used for the sub query in FromList and propagated the constant predicate
/// from sub query to outer query.
/// In the future, it will support propagate constant in WhereClause and SelectList.
///
/// Example 1:
/// Query: select * from t, (select * from s where s.id>1) tmp where t.id=tmp.id
/// Optimized: select * from t, (select * from s where s.id>1) tmp where t.id=tmp.id and tmp.id>1
///
/// Process:
///  1. Match the Join + selection pattern and find the candidate constant predicate, such as 's.id>1'
///  2. Pull up the candidate constant predicate, above of Join node.
///     The new selection will be created with the new constant predicate. 'tmp.id>1'
///
/// Steps 1 and 2 will be called recursively
///  3. (ppdSolver in rule_predicate_push_down.go) Push down constant predicate
///     and propagate constant predicate into other side. 't.id>1'
#[derive(Clone, Copy, Debug, Default)]
pub struct ConstantPropagationSolver;

impl LogicalOptRule for ConstantPropagationSolver {
    /// Optimize implements base.LogicalOptRule.<0th> interface.
    /// **Preorder traversal** of logic tree
    /// Step1: constant propagation current plan node
    /// Step2: optimize all of child
    ///
    /// For step1, different logical plan have their own logic for constant propagation,
    /// which is mainly implemented in the interface "constantPropagation" of LogicalPlan.
    /// Currently only the Logical Join implements this function. (Used for the subquery in FROM List)
    /// In the future, the Logical Apply will implements this function. (Used for the subquery in WHERE or SELECT list)
    // go: ConstantPropagationSolver.Optimize L1 [M-PLAN]
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        // constant propagation root plan
        let new_root = dispatch_constant_propagation(ctx, p, None, 0, opt)?;
        // recursive optimize
        let children = ctx.arena.get(p).children().to_vec();
        for (i, child) in children.into_iter().enumerate() {
            self.exec_optimize(ctx, child, Some(p), i, opt)?;
        }
        match new_root {
            None => Ok((p, plan_changed)),
            Some(new_root) => Ok((new_root, plan_changed)),
        }
    }

    /// Name implements base.LogicalOptRule.<1st> interface.
    // go: ConstantPropagationSolver.Name L1
    fn name(&self) -> &'static str {
        "constant_propagation"
    }
}

impl ConstantPropagationSolver {
    /// execOptimize optimize constant propagation exclude root plan node
    // go: ConstantPropagationSolver.execOptimize L1 [M-PLAN]
    fn exec_optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        current_plan: PlanId,
        parent_plan: Option<PlanId>,
        current_child_idx: usize,
        opt: &LogicalOptimizeOp,
    ) -> Result<(), Error> {
        if parent_plan.is_none() {
            // Attention: The function 'execOptimize' could not handle the root plan, so the parent plan could not be nil.
            return Ok(());
        }
        // constant propagation
        dispatch_constant_propagation(ctx, current_plan, parent_plan, current_child_idx, opt)?;
        // recursive optimize
        let children = ctx.arena.get(current_plan).children().to_vec();
        for (i, child) in children.into_iter().enumerate() {
            self.exec_optimize(ctx, child, Some(current_plan), i, opt)?;
        }
        Ok(())
    }
}
