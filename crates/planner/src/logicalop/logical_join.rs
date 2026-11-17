//! go-file: pkg/planner/core/operator/logicalop/logical_join.go
//!
//! The join operator is block D09.3. Predicate push down of the single-table operators
//! calls two of its helpers, `Conds2TableDual` and `AppendAddSelectionTraceStep`; they are
//! ported here with VS1-23 (listed as additions in `specs/D09.14-logical-operators/spec.md`
//! §2). The rest of the file comes with D09.3.

use tidb_expression::constant_fold::maybe_over_optimized4_plan_cache;
use tidb_expression::{Expr, ExprRef, Schema};
use tidb_sessionctx::stmtctx::type_ctx_or_default;

use crate::arena::PlanId;
use crate::logicalop::LogicalTableDual;
use crate::planctx::PlanCtx;
use crate::util::logicaltrace::TraceId;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// AppendAddSelectionTraceStep appends a trace step for adding a selection operator.
// go: AppendAddSelectionTraceStep L1 [M-CTX]
pub fn append_add_selection_trace_step(
    p: TraceId,
    child: TraceId,
    sel: TraceId,
    opt: &LogicalOptimizeOp,
) {
    let reason = String::new;
    let action = || {
        format!(
            "add {}_{} to connect {}_{} and {}_{}",
            sel.0, sel.1, p.0, p.1, child.0, child.1
        )
    };
    opt.append_step_to_current(sel.1 as isize, sel.0, reason, action);
}

/// Conds2TableDual builds a LogicalTableDual if cond is constant false or null. Go passes
/// the plan `p` for its context, query block offset and schema; the dual shares `p`'s
/// schema in Go, here it gets a copy (`schema`, M-FT audit in the VS1 plan: the shared
/// schema is never written while both are in the tree). The dual is stored in the arena.
// go: Conds2TableDual L1 [M-CTX, M-PLAN]
pub fn conds2_table_dual(
    ctx: &mut PlanCtx<'_>,
    schema: Schema,
    qb_offset: isize,
    conds: &[ExprRef],
) -> Option<PlanId> {
    if conds.len() != 1 {
        return None;
    }
    let Expr::Constant(con) = &*conds[0] else {
        return None;
    };
    if maybe_over_optimized4_plan_cache(&*ctx.expr_ctx, std::slice::from_ref(&conds[0])) {
        return None;
    }
    let tc = type_ctx_or_default(Some(ctx.vars.stmt_ctx()));
    let (is_true, err) = con.value.to_bool(&tc);
    if (err.is_ok() && is_true == 0) || con.value.is_null() {
        let mut dual = LogicalTableDual::init(ctx, qb_offset);
        dual.sp.set_schema(Some(schema));
        return Some(ctx.arena.alloc(Box::new(dual)));
    }
    None
}
