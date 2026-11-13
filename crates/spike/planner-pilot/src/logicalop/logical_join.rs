//! go-file: pkg/planner/core/operator/logicalop/logical_join.go (spike subset)

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::util::extract_columns;
use tidb_expression::{Column, ExprRef, Schema, merge_schema};

use crate::arena::{PlanArena, PlanId};
use crate::base::{LogicalPlan, ReplaceMap, prune_columns};
use crate::logicalop::{BaseLogicalPlan, LogicalSchemaProducer, logical_plan_boilerplate};
use crate::optimizetrace::LogicalOptimizeOp;
use crate::planctx::PlanCtx;
use crate::rule_util::resolve_expr_and_replace;
use crate::util::reset_not_null_flag;

/// JoinType contains CrossJoin, InnerJoin, LeftOuterJoin, RightOuterJoin, SemiJoin, AntiJoin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinType {
    /// InnerJoin means inner join.
    InnerJoin,
    /// LeftOuterJoin means left join.
    LeftOuterJoin,
    /// RightOuterJoin means right join.
    RightOuterJoin,
    /// SemiJoin means if row a in table A matches some rows in B, just output a.
    SemiJoin,
    /// AntiSemiJoin means if row a in table A does not match any row in B, then output a.
    AntiSemiJoin,
    /// LeftOuterSemiJoin means if row a in table A matches some rows in B, output (a, true), otherwise, output (a, false).
    LeftOuterSemiJoin,
    /// AntiLeftOuterSemiJoin means if row a in table A matches some rows in B, output (a, false), otherwise, output (a, true).
    AntiLeftOuterSemiJoin,
}

/// LogicalJoin is the logical join plan.
pub struct LogicalJoin {
    /// BaseLogicalPlan.
    pub base: BaseLogicalPlan,
    /// LogicalSchemaProducer.
    pub sp: LogicalSchemaProducer,
    /// JoinType.
    pub join_type: JoinType,
    /// EqualConditions (Go: `[]*expression.ScalarFunction`).
    pub equal_conditions: Vec<ExprRef>,
    /// NAEQConditions.
    pub naeq_conditions: Vec<ExprRef>,
    /// LeftConditions.
    pub left_conditions: Vec<ExprRef>,
    /// RightConditions.
    pub right_conditions: Vec<ExprRef>,
    /// OtherConditions.
    pub other_conditions: Vec<ExprRef>,
    /// FullSchema contains all the columns that the Join can output. It's ordered as
    /// [outer schema..., inner schema...].
    pub full_schema: Option<Schema>,
}

impl LogicalJoin {
    /// Init initializes LogicalJoin (children set, schema left nil: callers `SetSchema`).
    // go: pkg/planner/core/operator/logicalop/logical_join.go LogicalJoin.Init L1 [M-PLAN]
    pub fn init(join_type: JoinType, ctx: &PlanCtx<'_>, qb_offset: i32) -> LogicalJoin {
        LogicalJoin {
            base: BaseLogicalPlan::new(ctx, "Join", qb_offset),
            sp: LogicalSchemaProducer::new(None),
            join_type,
            equal_conditions: Vec::new(),
            naeq_conditions: Vec::new(),
            left_conditions: Vec::new(),
            right_conditions: Vec::new(),
            other_conditions: Vec::new(),
            full_schema: None,
        }
    }

    /// ExtractUsedCols extracts all the needed columns.
    // go: pkg/planner/core/operator/logicalop/logical_join.go (*LogicalJoin).ExtractUsedCols L1 [M-PLAN, M-IFACE]
    pub fn extract_used_cols(
        &self,
        arena: &PlanArena,
        mut parent_used_cols: Vec<Arc<Column>>,
    ) -> (Vec<Arc<Column>>, Vec<Arc<Column>>) {
        let (mut left_cols, mut right_cols) = (Vec::new(), Vec::new());
        for eq_cond in &self.equal_conditions {
            parent_used_cols.extend(extract_columns(eq_cond));
        }
        for left_cond in &self.left_conditions {
            parent_used_cols.extend(extract_columns(left_cond));
        }
        for right_cond in &self.right_conditions {
            parent_used_cols.extend(extract_columns(right_cond));
        }
        for other_cond in &self.other_conditions {
            parent_used_cols.extend(extract_columns(other_cond));
        }
        for naeq_cond in &self.naeq_conditions {
            parent_used_cols.extend(extract_columns(naeq_cond));
        }
        let l_child = self.children()[0];
        let r_child = self.children()[1];
        let l_schema = arena.get(l_child).schema(arena);
        let r_schema = arena.get(r_child).schema(arena);
        // parentused col = t2.a
        // leftChild schema = t1.a(t2.a) + and others
        // rightChild schema = t3 related + and others
        let l_full_schema = arena
            .downcast_ref::<LogicalJoin>(l_child)
            .and_then(|join| join.full_schema.as_ref());
        let r_full_schema = arena
            .downcast_ref::<LogicalJoin>(r_child)
            .and_then(|join| join.full_schema.as_ref());
        for col in parent_used_cols {
            if l_schema.contains(&col) || l_full_schema.is_some_and(|s| s.contains(&col)) {
                left_cols.push(col);
            } else if r_schema.contains(&col) || r_full_schema.is_some_and(|s| s.contains(&col)) {
                right_cols.push(col);
            }
        }
        (left_cols, right_cols)
    }

    /// MergeSchema merge the schema of left and right child of join.
    // go: pkg/planner/core/operator/logicalop/logical_join.go (*LogicalJoin).MergeSchema L1 [M-PLAN]
    pub fn merge_schema(&mut self, arena: &PlanArena) {
        let schema = build_logical_join_schema(self.join_type, self, arena);
        self.sp.set_schema(Some(schema));
    }
}

impl LogicalPlan for LogicalJoin {
    logical_plan_boilerplate!(Join, producer);

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: pkg/planner/core/operator/logicalop/logical_join.go (*LogicalJoin).ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for equal_expr in self.equal_conditions.iter_mut() {
            resolve_expr_and_replace(equal_expr, replace);
        }
        for left_expr in self.left_conditions.iter_mut() {
            resolve_expr_and_replace(left_expr, replace);
        }
        for right_expr in self.right_conditions.iter_mut() {
            resolve_expr_and_replace(right_expr, replace);
        }
        for other_expr in self.other_conditions.iter_mut() {
            resolve_expr_and_replace(other_expr, replace);
        }
    }

    /// PruneColumns implements the base.LogicalPlan.<2nd> interface.
    // go: pkg/planner/core/operator/logicalop/logical_join.go (*LogicalJoin).PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let (left_cols, right_cols) = self.extract_used_cols(&ctx.arena, parent_used_cols.clone());

        self.children_mut()[0] = prune_columns(ctx, self.children()[0], left_cols, opt)?;

        self.children_mut()[1] = prune_columns(ctx, self.children()[1], right_cols, opt)?;

        self.merge_schema(&ctx.arena);
        if self.join_type == JoinType::LeftOuterSemiJoin
            || self.join_type == JoinType::AntiLeftOuterSemiJoin
        {
            let join_col = self
                .schema(&ctx.arena)
                .columns
                .last()
                .cloned()
                .expect("join col");
            parent_used_cols.push(join_col);
        }
        self.inline_projection(ctx, parent_used_cols, opt);
        Ok(this)
    }
}

/// BuildLogicalJoinSchema builds the schema for join operator.
// go: pkg/planner/core/operator/logicalop/logical_join.go BuildLogicalJoinSchema L1 [M-PLAN]
pub fn build_logical_join_schema<P: LogicalPlan + ?Sized>(
    join_type: JoinType,
    join: &P,
    arena: &PlanArena,
) -> Schema {
    let left_schema = arena.get(join.children()[0]).schema(arena);
    match join_type {
        JoinType::SemiJoin | JoinType::AntiSemiJoin => return left_schema.clone_schema(),
        JoinType::LeftOuterSemiJoin | JoinType::AntiLeftOuterSemiJoin => {
            let mut new_schema = left_schema.clone_schema();
            let s = join.schema(arena);
            new_schema.append([s.columns[s.len() - 1].clone()]);
            return new_schema;
        }
        _ => {}
    }
    let right_schema = arena.get(join.children()[1]).schema(arena);
    let mut new_schema =
        merge_schema(Some(left_schema), Some(right_schema)).expect("both schemas are non-nil");
    if join_type == JoinType::LeftOuterJoin {
        // N1 (borrow B3): `new_schema.len()` cannot be evaluated while `&mut new_schema` is
        // an argument of the same call.
        let new_schema_len = new_schema.len();
        reset_not_null_flag(&mut new_schema, left_schema.len(), new_schema_len);
    } else if join_type == JoinType::RightOuterJoin {
        reset_not_null_flag(&mut new_schema, 0, left_schema.len());
    }
    new_schema
}
