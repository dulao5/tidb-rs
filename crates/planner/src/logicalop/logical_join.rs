//! go-file: pkg/planner/core/operator/logicalop/logical_join.go
//!
//! `LogicalJoin`: the logical join operator (the join builder, block D09.3 part A:
//! `specs/D09.3-join-build/spec.md` §3.1). `Conds2TableDual` and `AppendAddSelectionTraceStep`
//! came earlier with the single-table predicate push down (VS1-23).
//!
//! Mapping (spec §4.1, §4.2):
//! - `JoinType` is a `#[repr(u8)]` enum with Go's values (`conflict_detector.go` depends on
//!   them); Go's `init()` assertion is a compile-time assertion.
//! - `EqualConditions` / `NAEQConditions` (`[]*expression.ScalarFunction`) are `Vec<ExprRef>`
//!   whose elements are all scalar functions (asserted where they are read).
//! - `HintInfo *utilhint.PlanHints` is `Option<Arc<PlanHints>>` (set only when a hint is used).
//! - The interface methods Go overrides are all written out (spec §4.2 item 3): the ones whose
//!   blocks are not ported return their block's `blocked(...)` instead of falling back to the
//!   `BaseLogicalPlan` default. `ExtractCorrelatedCols`, `ExtractColGroups` and
//!   `PreparePossibleProperties` are ported (their trait methods return no `Result`, and
//!   their dependencies are ready; spec §5 item 3).
//! - Cascades-only methods (`Shallow`, `ExtractJoinKeys`) are not ported (`EXC.B`).
//! - Part B (logical optimization), part C (`Decorrelate`, `ColumnSubstituteAll`) and part E
//!   (MPP hint checks) come with their tasks.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::{TypeTiny, has_not_null_flag};
use tidb_expression::constant_fold::maybe_over_optimized4_plan_cache;
use tidb_expression::expression::is_eq_cond_from_in;
use tidb_expression::scalar_function::new_function;
use tidb_expression::util::{
    build_not_null_expr, derive_relaxed_filters_from_dnf, extract_columns, extract_cor_columns,
};
use tidb_expression::{
    Column, CorrelatedColumn, Expr, ExprRef, Schema, merge_schema, sort_columns,
};
use tidb_parser::ast::funcs as ast_funcs;
use tidb_sessionctx::stmtctx::type_ctx_or_default;
use tidb_types::datum::Datum;
use tidb_types::field_name::NameSlice;
use tidb_types::field_type::new_field_type;

use crate::arena::{PlanArena, PlanId};
use crate::base::{LogicalPlan, ReplaceMap};
use crate::core::rule_util::resolve_expr_and_replace;
use crate::hint::hint::{
    PlanHints, PreferBCJoin, PreferHJBuild, PreferHJProbe, PreferHashJoin, PreferINLHJ, PreferINLJ,
    PreferINLMJ, PreferLeftAsHJBuild, PreferLeftAsHJProbe, PreferLeftAsINLHJInner,
    PreferLeftAsINLJInner, PreferLeftAsINLMJInner, PreferMergeJoin, PreferNoHashJoin,
    PreferNoIndexHashJoin, PreferNoIndexJoin, PreferNoIndexMergeJoin, PreferNoMergeJoin,
    PreferRightAsHJBuild, PreferRightAsHJProbe, PreferRightAsINLHJInner, PreferRightAsINLJInner,
    PreferRightAsINLMJInner, PreferShuffleJoin,
};
use crate::logicalop::LogicalTableDual;
use crate::logicalop::base_logical_plan::{BaseLogicalPlan, new_base_logical_plan};
use crate::logicalop::logical_schema_producer::{self, LogicalSchemaProducer};
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::PlanCtx;
use crate::property::{
    GroupNDV, MPPPartitionColumn, PhysicalProperty, StatsInfo, get_collate_id_by_name_for_partition,
};
use crate::util::logicaltrace::TraceId;
use crate::util::misc::extract_table_alias;
use crate::util::null_misc::{is_null_rejected, reset_not_null_flag};
use crate::util::optimizetrace::LogicalOptimizeOp;

/// `plancodec.TypeJoin` (D14.2).
const TYPE_JOIN: &str = "Join";

/// JoinType contains CrossJoin, InnerJoin, LeftOuterJoin, RightOuterJoin, SemiJoin, AntiJoin.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum JoinType {
    /// InnerJoin means inner join.
    #[default]
    InnerJoin = 0,
    /// LeftOuterJoin means left join.
    LeftOuterJoin = 1,
    /// RightOuterJoin means right join.
    RightOuterJoin = 2,
    /// SemiJoin means if row a in table A matches some rows in B, just output a.
    SemiJoin = 3,
    /// AntiSemiJoin means if row a in table A does not match any row in B, then output a.
    AntiSemiJoin = 4,
    /// LeftOuterSemiJoin means if row a in table A matches some rows in B, output (a, true), otherwise, output (a, false).
    LeftOuterSemiJoin = 5,
    /// AntiLeftOuterSemiJoin means if row a in table A matches some rows in B, output (a, false), otherwise, output (a, true).
    AntiLeftOuterSemiJoin = 6,
}

// NOTE: keep JoinType value unchanged, because they are used in conflict_detector.go
// go: init L2 (compile-time assertion instead of intest.Assert at init)
const _: () = assert!(
    JoinType::InnerJoin as u8 == 0
        && JoinType::LeftOuterJoin as u8 == 1
        && JoinType::RightOuterJoin as u8 == 2
        && JoinType::SemiJoin as u8 == 3
        && JoinType::AntiSemiJoin as u8 == 4
        && JoinType::LeftOuterSemiJoin as u8 == 5
        && JoinType::AntiLeftOuterSemiJoin as u8 == 6
);

impl JoinType {
    /// IsOuterJoin returns if this joiner is an outer joiner
    // go: JoinType.IsOuterJoin L1
    pub fn is_outer_join(self) -> bool {
        self == JoinType::LeftOuterJoin
            || self == JoinType::RightOuterJoin
            || self == JoinType::LeftOuterSemiJoin
            || self == JoinType::AntiLeftOuterSemiJoin
    }

    /// IsSemiJoin returns if this joiner is a semi/anti-semi joiner
    // go: JoinType.IsSemiJoin L1
    pub fn is_semi_join(self) -> bool {
        self == JoinType::SemiJoin
            || self == JoinType::AntiSemiJoin
            || self == JoinType::LeftOuterSemiJoin
            || self == JoinType::AntiLeftOuterSemiJoin
    }

    /// `String()` (`M-NAME`). Go's default arm ("unsupported join type") cannot be reached:
    /// the enum has no other value.
    // go: JoinType.String L1 [M-NAME]
    pub fn string(self) -> &'static str {
        match self {
            JoinType::InnerJoin => "inner join",
            JoinType::LeftOuterJoin => "left outer join",
            JoinType::RightOuterJoin => "right outer join",
            JoinType::SemiJoin => "semi join",
            JoinType::AntiSemiJoin => "anti semi join",
            JoinType::LeftOuterSemiJoin => "left outer semi join",
            JoinType::AntiLeftOuterSemiJoin => "anti left outer semi join",
        }
    }
}

/// LogicalJoin is the logical join plan.
#[derive(Debug)]
pub struct LogicalJoin {
    /// Embedded `BaseLogicalPlan` (via `LogicalSchemaProducer`).
    pub base: BaseLogicalPlan,
    /// Embedded `LogicalSchemaProducer`.
    pub sp: LogicalSchemaProducer,

    pub join_type: JoinType,
    pub reordered: bool,
    pub cartesian_join: bool,
    pub straight_join: bool,

    /// HintInfo stores the join algorithm hint information specified by client.
    pub hint_info: Option<Arc<PlanHints>>,
    pub prefer_join_type: u64,
    pub prefer_join_order: bool,
    pub left_prefer_join_type: u64,
    pub right_prefer_join_type: u64,

    /// Every element is a `*expression.ScalarFunction`.
    pub equal_conditions: Vec<ExprRef>,
    /// NAEQConditions means null aware equal conditions, which is used for null aware semi
    /// joins. Every element is a `*expression.ScalarFunction`.
    pub na_eq_conditions: Vec<ExprRef>,
    pub left_conditions: Vec<ExprRef>,
    pub right_conditions: Vec<ExprRef>,
    pub other_conditions: Vec<ExprRef>,

    pub left_properties: Vec<Vec<Arc<Column>>>,
    pub right_properties: Vec<Vec<Arc<Column>>>,

    /// DefaultValues is only used for left/right outer join, which is values the inner row's should be when the outer table
    /// doesn't match any inner table's row.
    /// That it's nil just means the default values is a slice of NULL.
    /// Currently, only `aggregation push down` phase will set this.
    pub default_values: Vec<Datum>,

    /// FullSchema contains all the columns that the Join can output. It's ordered as [outer schema..., inner schema...].
    /// This is useful for natural joins and "using" joins. In these cases, the join key columns from the
    /// inner side (or the right side when it's an inner join) will not be in the schema of Join.
    /// But upper operators should be able to find those "redundant" columns, and the user also can specifically select
    /// those columns, so we put the "redundant" columns here to make them be able to be found.
    ///
    /// For example:
    /// create table t1(a int, b int); create table t2(a int, b int);
    /// select * from t1 join t2 using (b);
    /// schema of the Join will be [t1.b, t1.a, t2.a]; FullSchema will be [t1.a, t1.b, t2.a, t2.b].
    ///
    /// We record all columns and keep them ordered is for correctly handling SQLs like
    /// select t1.*, t2.* from t1 join t2 using (b);
    /// (*PlanBuilder).unfoldWildStar() handles the schema for such case.
    pub full_schema: Option<Schema>,
    pub full_names: NameSlice,

    /// PreferCorrelate is set to true when this SemiJoin originated from a non-correlated
    /// IN subquery during the correlate alternative round, indicating that the CorrelateSolver
    /// should convert it back to a correlated Apply with index lookups.
    pub prefer_correlate: bool,

    /// EqualCondOutCnt indicates the estimated count of joined rows after evaluating `EqualConditions`.
    pub equal_cond_out_cnt: f64,

    /// FromDecorrelatedApply marks joins that come from decorrelating an Apply in the
    /// first logical round. It is only used to decide whether an equivalent same-order
    /// PhysicalIndexJoin candidate has already been generated.
    pub from_decorrelated_apply: bool,
}

// go-exception: EXC.B LogicalJoin.Shallow (only called by legacy cascades transformation rules; tidb-rs does not support cascades planner)
// go-exception: EXC.B LogicalJoin.ExtractJoinKeys (only called by legacy cascades transformation rules; tidb-rs does not support cascades planner)

/// The `*expression.ScalarFunction` of an element of `EqualConditions` / `NAEQConditions`.
// glue: M-IFACE (the slice's element type is *expression.ScalarFunction)
fn eq_cond_args(expr: &ExprRef) -> &[ExprRef] {
    expr.as_scalar_function()
        .expect("an equal condition is a scalar function")
        .get_args()
}

/// `expr.GetArgs()[i].(*expression.Column)` of an equal condition (Go panics when the argument
/// is not a column, as does this).
// glue: M-IFACE (type assertion to *expression.Column)
fn eq_cond_col(expr: &ExprRef, i: usize) -> Arc<Column> {
    Arc::new(
        eq_cond_args(expr)[i]
            .as_column()
            .expect("an equal condition argument is a column")
            .clone(),
    )
}

impl LogicalJoin {
    /// Init initializes LogicalJoin. Go's `Init` has a value receiver over a struct literal
    /// whose other fields the caller sets; here they start at their zero values and the
    /// caller sets them on the result (Go's literal allocates no id, so the order of the plan
    /// ids is the same).
    // go: LogicalJoin.Init L2 (the caller sets the literal's fields after Init)
    pub fn init(ctx: &mut PlanCtx<'_>, offset: isize) -> LogicalJoin {
        LogicalJoin {
            base: new_base_logical_plan(ctx, TYPE_JOIN, offset),
            sp: LogicalSchemaProducer::default(),
            join_type: JoinType::InnerJoin,
            reordered: false,
            cartesian_join: false,
            straight_join: false,
            hint_info: None,
            prefer_join_type: 0,
            prefer_join_order: false,
            left_prefer_join_type: 0,
            right_prefer_join_type: 0,
            equal_conditions: Vec::new(),
            na_eq_conditions: Vec::new(),
            left_conditions: Vec::new(),
            right_conditions: Vec::new(),
            other_conditions: Vec::new(),
            left_properties: Vec::new(),
            right_properties: Vec::new(),
            default_values: Vec::new(),
            full_schema: None,
            full_names: Vec::new(),
            prefer_correlate: false,
            equal_cond_out_cnt: 0.0,
            from_decorrelated_apply: false,
        }
    }

    /// BuildKeyInfo implements the base.LogicalPlan.<4th> interface. The children's schemas
    /// are read from the arena; the own schema must be set (`MergeSchema`).
    // go: LogicalJoin.BuildKeyInfo L1 [M-PLAN]
    pub fn build_key_info(&mut self, arena: &mut PlanArena) {
        logical_schema_producer::build_key_info(self, arena);
        let child0 = self.children()[0];
        let child1 = self.children()[1];
        match self.join_type {
            JoinType::SemiJoin
            | JoinType::LeftOuterSemiJoin
            | JoinType::AntiSemiJoin
            | JoinType::AntiLeftOuterSemiJoin => {
                let keys = arena.get(child0).schema(arena).clone_schema().keys;
                self.schema_mut(arena).keys = keys;
            }
            JoinType::InnerJoin | JoinType::LeftOuterJoin | JoinType::RightOuterJoin => {
                // If there is no equal conditions, then cartesian product can't be prevented and unique key information will destroy.
                if self.equal_conditions.is_empty() {
                    return;
                }
                let mut l_ok = false;
                let mut r_ok = false;
                // Such as 'select * from t1 join t2 where t1.a = t2.a and t1.b = t2.b'.
                // If one sides (a, b) is a unique key, then the unique key information is remained.
                // But we don't consider this situation currently.
                // Only key made by one column is considered now.
                let l_keys = arena.get(child0).schema(arena).keys.clone();
                let r_keys = arena.get(child1).schema(arena).keys.clone();
                for expr in &self.equal_conditions {
                    let args = eq_cond_args(expr);
                    let ln = args[0].as_column().expect("column");
                    let rn = args[1].as_column().expect("column");
                    for key in &l_keys {
                        if key.len() == 1 && key[0].equal(&Expr::Column(ln.clone())) {
                            l_ok = true;
                            break;
                        }
                    }
                    for key in &r_keys {
                        if key.len() == 1 && key[0].equal(&Expr::Column(rn.clone())) {
                            r_ok = true;
                            break;
                        }
                    }
                }
                // For inner join, if one side of one equal condition is unique key,
                // another side's unique key information will all be reserved.
                // If it's an outer join, NULL value will fill some position, which will destroy the unique key information.
                if l_ok && self.join_type != JoinType::LeftOuterJoin {
                    self.schema_mut(arena).keys.extend(r_keys.iter().cloned());
                }
                if r_ok && self.join_type != JoinType::RightOuterJoin {
                    self.schema_mut(arena).keys.extend(l_keys.iter().cloned());
                }
            }
        }
    }

    /// ExtractFD implements the base.LogicalPlan.<22th> interface.
    // glue: stub of LogicalJoin.ExtractFD, blocked-by:D09.14 (functional dependencies, pkg/planner/funcdep; only read when tidb_enable_new_only_full_group_by_check is ON)
    pub fn extract_fd(&self) -> Result<(), Error> {
        Err(blocked(
            "D09.14",
            "LogicalJoin.ExtractFD (functional dependencies)",
        ))
    }

    /// IsNAAJ checks if the join is a non-adjacent-join.
    // go: LogicalJoin.IsNAAJ L1
    pub fn is_naaj(&self) -> bool {
        !self.na_eq_conditions.is_empty()
    }

    /// ExtractFDForSemiJoin extracts FD for semi join.
    // glue: stub of LogicalJoin.ExtractFDForSemiJoin, blocked-by:D09.14 (functional dependencies, pkg/planner/funcdep)
    pub fn extract_fd_for_semi_join(&self, filters_from_apply: &[ExprRef]) -> Result<(), Error> {
        let _ = filters_from_apply;
        Err(blocked(
            "D09.14",
            "LogicalJoin.ExtractFDForSemiJoin (functional dependencies)",
        ))
    }

    /// ExtractFDForInnerJoin extracts FD for inner join.
    // glue: stub of LogicalJoin.ExtractFDForInnerJoin, blocked-by:D09.14 (functional dependencies, pkg/planner/funcdep)
    pub fn extract_fd_for_inner_join(&self, filters_from_apply: &[ExprRef]) -> Result<(), Error> {
        let _ = filters_from_apply;
        Err(blocked(
            "D09.14",
            "LogicalJoin.ExtractFDForInnerJoin (functional dependencies)",
        ))
    }

    /// ExtractFDForOuterJoin extracts FD for outer join.
    // glue: stub of LogicalJoin.ExtractFDForOuterJoin, blocked-by:D09.14 (functional dependencies, pkg/planner/funcdep)
    pub fn extract_fd_for_outer_join(&self, filters_from_apply: &[ExprRef]) -> Result<(), Error> {
        let _ = filters_from_apply;
        Err(blocked(
            "D09.14",
            "LogicalJoin.ExtractFDForOuterJoin (functional dependencies)",
        ))
    }

    /// GetJoinKeys extracts join keys(columns) from EqualConditions. It returns left join keys, right
    /// join keys and an `isNullEQ` array which means the `joinKey[i]` is a `NullEQ` function. The `hasNullEQ`
    /// means whether there is a `NullEQ` of a join key.
    // go: LogicalJoin.GetJoinKeys L1
    #[allow(clippy::type_complexity)]
    pub fn get_join_keys(&self) -> (Vec<Arc<Column>>, Vec<Arc<Column>>, Vec<bool>, bool) {
        let mut left_keys = Vec::new();
        let mut right_keys = Vec::new();
        let mut is_null_eq = Vec::new();
        let mut has_null_eq = false;
        for expr in &self.equal_conditions {
            left_keys.push(eq_cond_col(expr, 0));
            right_keys.push(eq_cond_col(expr, 1));
            let name = &expr
                .as_scalar_function()
                .expect("scalar function")
                .func_name
                .l;
            is_null_eq.push(name == ast_funcs::NullEQ);
            has_null_eq = has_null_eq || name == ast_funcs::NullEQ;
        }
        (left_keys, right_keys, is_null_eq, has_null_eq)
    }

    /// GetNAJoinKeys extracts join keys(columns) from NAEqualCondition.
    // go: LogicalJoin.GetNAJoinKeys L1
    pub fn get_na_join_keys(&self) -> (Vec<Arc<Column>>, Vec<Arc<Column>>) {
        let mut left_keys = Vec::new();
        let mut right_keys = Vec::new();
        for expr in &self.na_eq_conditions {
            left_keys.push(eq_cond_col(expr, 0));
            right_keys.push(eq_cond_col(expr, 1));
        }
        (left_keys, right_keys)
    }

    /// GetPotentialPartitionKeys return potential partition keys for join, the potential partition keys are
    /// the join keys of EqualConditions
    // go: LogicalJoin.GetPotentialPartitionKeys L1
    pub fn get_potential_partition_keys(
        &self,
    ) -> (Vec<MPPPartitionColumn>, Vec<MPPPartitionColumn>) {
        let mut left_keys = Vec::new();
        let mut right_keys = Vec::new();
        for expr in &self.equal_conditions {
            let (_, coll) = expr
                .as_scalar_function()
                .expect("scalar function")
                .charset_and_collation();
            let collate_id = get_collate_id_by_name_for_partition(coll);
            left_keys.push(MPPPartitionColumn {
                col: eq_cond_col(expr, 0),
                collate_id,
            });
            right_keys.push(MPPPartitionColumn {
                col: eq_cond_col(expr, 1),
                collate_id,
            });
        }
        (left_keys, right_keys)
    }

    /// AttachOnConds extracts on conditions for join and set the `EqualConditions`, `LeftConditions`, `RightConditions` and
    /// `OtherConditions` by the result of extract. The children are read from the arena (the
    /// join itself is not in it, or is detached).
    // go: LogicalJoin.AttachOnConds L1 [M-CTX]
    pub fn attach_on_conds(
        &mut self,
        ctx: &PlanCtx<'_>,
        on_conds: &[ExprRef],
    ) -> Result<(), Error> {
        let (eq, left, right, other) =
            self.extract_on_condition_of_children(ctx, on_conds, false, false)?;
        self.append_join_conds(eq, left, right, other);
        Ok(())
    }

    /// AppendJoinConds appends new join conditions.
    // go: LogicalJoin.AppendJoinConds L1
    pub fn append_join_conds(
        &mut self,
        mut eq: Vec<ExprRef>,
        mut left: Vec<ExprRef>,
        mut right: Vec<ExprRef>,
        mut other: Vec<ExprRef>,
    ) {
        eq.append(&mut self.equal_conditions);
        self.equal_conditions = eq;
        left.append(&mut self.left_conditions);
        self.left_conditions = left;
        right.append(&mut self.right_conditions);
        self.right_conditions = right;
        other.append(&mut self.other_conditions);
        self.other_conditions = other;
    }

    /// MergeSchema merge the schema of left and right child of join.
    // go: LogicalJoin.MergeSchema L1 [M-PLAN]
    pub fn merge_schema(&mut self, arena: &PlanArena) {
        let schema = build_logical_join_schema(self.join_type, &*self, arena);
        self.sp.set_schema(Some(schema));
    }

    // go: LogicalJoin.getGroupNDVs L1
    fn get_group_ndvs(
        &self,
        col_groups: &[Vec<Arc<Column>>],
        child_stats: &[Arc<StatsInfo>],
    ) -> Vec<GroupNDV> {
        let mut outer_idx: isize = -1;
        if self.join_type == JoinType::LeftOuterJoin
            || self.join_type == JoinType::LeftOuterSemiJoin
            || self.join_type == JoinType::AntiLeftOuterSemiJoin
        {
            outer_idx = 0;
        } else if self.join_type == JoinType::RightOuterJoin {
            outer_idx = 1;
        }
        if outer_idx >= 0 && !col_groups.is_empty() {
            return child_stats[outer_idx as usize].group_ndvs.clone();
        }
        Vec::new()
    }

    /// PreferAny checks whether the join type is in the joinFlags.
    // go: LogicalJoin.PreferAny L1
    pub fn prefer_any(&self, join_flags: &[u64]) -> bool {
        for &flag in join_flags {
            if self.prefer_join_type & flag > 0 {
                return true;
            }
        }
        false
    }

    /// ExtractOnCondition divide conditions in CNF of join node into 4 groups.
    /// These conditions can be where conditions, join conditions, or collection of both.
    /// If deriveLeft/deriveRight is set, we would try to derive more conditions for left/right plan.
    ///
    /// Returns `(eqCond, leftCond, rightCond, otherCond)`; `eqCond`'s elements are scalar
    /// functions. `Err` only for a `blocked` boundary (the null-reject proof, or building the
    /// `eq` / `nulleq` function, where Go's `NewFunctionInternal` returns nil and the type
    /// assertion panics).
    // go: LogicalJoin.ExtractOnCondition L2 (an error building eq/nulleq is returned; Go's nil result would panic at the type assertion)
    #[allow(clippy::type_complexity)]
    pub fn extract_on_condition(
        &mut self,
        ctx: &PlanCtx<'_>,
        conditions: &[ExprRef],
        left_schema: &Schema,
        right_schema: &Schema,
        derive_left: bool,
        derive_right: bool,
    ) -> Result<(Vec<ExprRef>, Vec<ExprRef>, Vec<ExprRef>, Vec<ExprRef>), Error> {
        let mut eq_cond: Vec<ExprRef> = Vec::new();
        let mut left_cond: Vec<ExprRef> = Vec::new();
        let mut right_cond: Vec<ExprRef> = Vec::new();
        let mut other_cond: Vec<ExprRef> = Vec::new();
        'conds: for expr in conditions {
            // For queries like `select a in (select a from s where s.b = t.b) from t`,
            // if subquery is empty caused by `s.b = t.b`, the result should always be
            // false even if t.a is null or s.a is null. To make this join "empty aware",
            // we should differentiate `t.a = s.a` from other column equal conditions, so
            // we put it into OtherConditions instead of EqualConditions of join.
            if is_eq_cond_from_in(expr) {
                other_cond.push(Arc::clone(expr));
                continue;
            }
            if let Some(binop) = expr.as_scalar_function()
                && binop.get_args().len() == 2
                && let (Some(a0), Some(a1)) = (
                    binop.get_args()[0].as_column(),
                    binop.get_args()[1].as_column(),
                )
            {
                let (mut arg0, mut arg1) = (&binop.get_args()[0], &binop.get_args()[1]);
                let mut left_col = left_schema.retrieve_column(a0);
                let mut right_col = right_schema.retrieve_column(a1);
                if left_col.is_none() || right_col.is_none() {
                    left_col = left_schema.retrieve_column(a1);
                    right_col = right_schema.retrieve_column(a0);
                    std::mem::swap(&mut arg0, &mut arg1);
                }
                if let (Some(left_col), Some(right_col)) = (left_col, right_col) {
                    // Go appends BuildNotNullExpr's result as is; it is nil only when
                    // NewFunctionInternal fails, which `not(isnull(col))` does not.
                    if derive_left
                        && is_null_rejected(ctx, left_schema, expr, true)?
                        && !has_not_null_flag(left_col.ret_type.get_flag())
                        && let Some(not_null_expr) = build_not_null_expr(
                            &*ctx.expr_ctx,
                            &Arc::new(Expr::Column((**left_col).clone())),
                        )
                    {
                        left_cond.push(not_null_expr);
                    }
                    if derive_right
                        && is_null_rejected(ctx, right_schema, expr, true)?
                        && !has_not_null_flag(right_col.ret_type.get_flag())
                        && let Some(not_null_expr) = build_not_null_expr(
                            &*ctx.expr_ctx,
                            &Arc::new(Expr::Column((**right_col).clone())),
                        )
                    {
                        right_cond.push(not_null_expr);
                    }
                    if binop.func_name.l == ast_funcs::EQ || binop.func_name.l == ast_funcs::NullEQ
                    {
                        let cond = new_function(
                            &*ctx.expr_ctx,
                            &binop.func_name.l,
                            new_field_type(TypeTiny),
                            vec![Arc::clone(arg0), Arc::clone(arg1)],
                        )?;
                        assert!(cond.as_scalar_function().is_some(), "eq condition");
                        eq_cond.push(cond);
                        continue 'conds;
                    }
                }
            }
            let columns = extract_columns(expr);
            // `columns` may be empty, if the condition is like `correlated_column op constant`, or `constant`,
            // push this kind of constant condition down according to join type.
            if columns.is_empty() {
                self.push_down_const_expr(
                    expr,
                    &mut left_cond,
                    &mut right_cond,
                    derive_left || derive_right,
                );
                continue;
            }
            let (mut all_from_left, mut all_from_right) = (true, true);
            for col in &columns {
                if !left_schema.contains(col) {
                    all_from_left = false;
                }
                if !right_schema.contains(col) {
                    all_from_right = false;
                }
            }
            if all_from_right {
                right_cond.push(Arc::clone(expr));
            } else if all_from_left {
                left_cond.push(Arc::clone(expr));
            } else {
                // Relax expr to two supersets: leftRelaxedCond and rightRelaxedCond, the expression now is
                // `expr AND leftRelaxedCond AND rightRelaxedCond`. Motivation is to push filters down to
                // children as much as possible.
                if derive_left
                    && let Some(left_relaxed_cond) =
                        derive_relaxed_filters_from_dnf(&*ctx.expr_ctx, expr, left_schema)
                {
                    left_cond.push(left_relaxed_cond);
                }
                if derive_right
                    && let Some(right_relaxed_cond) =
                        derive_relaxed_filters_from_dnf(&*ctx.expr_ctx, expr, right_schema)
                {
                    right_cond.push(right_relaxed_cond);
                }
                other_cond.push(Arc::clone(expr));
            }
        }
        Ok((eq_cond, left_cond, right_cond, other_cond))
    }

    /// pushDownConstExpr checks if the condition is from filter condition, if true, push it down to both
    /// children of join, whatever the join type is; if false, push it down to inner child of outer join,
    /// and both children of non-outer-join.
    // go: LogicalJoin.pushDownConstExpr L1
    fn push_down_const_expr(
        &mut self,
        expr: &ExprRef,
        left_cond: &mut Vec<ExprRef>,
        right_cond: &mut Vec<ExprRef>,
        filter_cond: bool,
    ) {
        match self.join_type {
            JoinType::LeftOuterJoin
            | JoinType::LeftOuterSemiJoin
            | JoinType::AntiLeftOuterSemiJoin => {
                if filter_cond {
                    left_cond.push(Arc::clone(expr));
                    // Append the expr to right join condition instead of `rightCond`, to make it able to be
                    // pushed down to children of join.
                    self.right_conditions.push(Arc::clone(expr));
                } else {
                    right_cond.push(Arc::clone(expr));
                }
            }
            JoinType::RightOuterJoin => {
                if filter_cond {
                    right_cond.push(Arc::clone(expr));
                    self.left_conditions.push(Arc::clone(expr));
                } else {
                    left_cond.push(Arc::clone(expr));
                }
            }
            JoinType::SemiJoin | JoinType::InnerJoin => {
                left_cond.push(Arc::clone(expr));
                right_cond.push(Arc::clone(expr));
            }
            JoinType::AntiSemiJoin => {
                if filter_cond {
                    left_cond.push(Arc::clone(expr));
                }
                right_cond.push(Arc::clone(expr));
            }
        }
    }

    /// extractOnCondition over the children's schemas (read from the arena).
    // go: LogicalJoin.extractOnCondition L1 [M-PLAN]
    #[allow(clippy::type_complexity)]
    fn extract_on_condition_of_children(
        &mut self,
        ctx: &PlanCtx<'_>,
        conditions: &[ExprRef],
        derive_left: bool,
        derive_right: bool,
    ) -> Result<(Vec<ExprRef>, Vec<ExprRef>, Vec<ExprRef>, Vec<ExprRef>), Error> {
        let left_schema = ctx.arena.get(self.children()[0]).schema(&ctx.arena);
        let right_schema = ctx.arena.get(self.children()[1]).schema(&ctx.arena);
        self.extract_on_condition(
            ctx,
            conditions,
            left_schema,
            right_schema,
            derive_left,
            derive_right,
        )
    }

    /// SetPreferredJoinTypeAndOrder sets the preferred join type and order for the LogicalJoin.
    ///
    /// L2: Go's `hintInfo` is the builder's shared `*PlanHints`, whose `Matched` flags the
    /// `IfPrefer*` calls set (read by the "hint is not used" warnings at the end of the build).
    /// The Rust builder keeps no hint stack (`TableHints()` is an empty `PlanHints`, the hint
    /// list is not parsed: `blocked-by:D02.2`), so the hints are passed by value and kept in
    /// `hint_info` when used.
    // go: LogicalJoin.SetPreferredJoinTypeAndOrder L2 (the hints are passed by value, see above)
    pub fn set_preferred_join_type_and_order(
        &mut self,
        ctx: &PlanCtx<'_>,
        hint_info: Option<PlanHints>,
    ) {
        let Some(mut hint_info) = hint_info else {
            return;
        };

        let lhs_alias = extract_table_alias(
            ctx.arena.get(self.children()[0]),
            &ctx.arena,
            self.query_block_offset(),
            &ctx.vars.current_db,
        );
        let rhs_alias = extract_table_alias(
            ctx.arena.get(self.children()[1]),
            &ctx.arena,
            self.query_block_offset(),
            &ctx.vars.current_db,
        );
        let lhs = [lhs_alias.as_ref()];
        let rhs = [rhs_alias.as_ref()];
        if hint_info.if_prefer_merge_join(&lhs) {
            self.prefer_join_type |= PreferMergeJoin;
            self.left_prefer_join_type |= PreferMergeJoin;
        }
        if hint_info.if_prefer_merge_join(&rhs) {
            self.prefer_join_type |= PreferMergeJoin;
            self.right_prefer_join_type |= PreferMergeJoin;
        }
        if hint_info.if_prefer_no_merge_join(&lhs) {
            self.prefer_join_type |= PreferNoMergeJoin;
            self.left_prefer_join_type |= PreferNoMergeJoin;
        }
        if hint_info.if_prefer_no_merge_join(&rhs) {
            self.prefer_join_type |= PreferNoMergeJoin;
            self.right_prefer_join_type |= PreferNoMergeJoin;
        }
        if hint_info.if_prefer_broadcast_join(&lhs) {
            self.prefer_join_type |= PreferBCJoin;
            self.left_prefer_join_type |= PreferBCJoin;
        }
        if hint_info.if_prefer_broadcast_join(&rhs) {
            self.prefer_join_type |= PreferBCJoin;
            self.right_prefer_join_type |= PreferBCJoin;
        }
        if hint_info.if_prefer_shuffle_join(&lhs) {
            self.prefer_join_type |= PreferShuffleJoin;
            self.left_prefer_join_type |= PreferShuffleJoin;
        }
        if hint_info.if_prefer_shuffle_join(&rhs) {
            self.prefer_join_type |= PreferShuffleJoin;
            self.right_prefer_join_type |= PreferShuffleJoin;
        }
        if hint_info.if_prefer_hash_join(&lhs) {
            self.prefer_join_type |= PreferHashJoin;
            self.left_prefer_join_type |= PreferHashJoin;
        }
        if hint_info.if_prefer_hash_join(&rhs) {
            self.prefer_join_type |= PreferHashJoin;
            self.right_prefer_join_type |= PreferHashJoin;
        }
        if hint_info.if_prefer_no_hash_join(&lhs) {
            self.prefer_join_type |= PreferNoHashJoin;
            self.left_prefer_join_type |= PreferNoHashJoin;
        }
        if hint_info.if_prefer_no_hash_join(&rhs) {
            self.prefer_join_type |= PreferNoHashJoin;
            self.right_prefer_join_type |= PreferNoHashJoin;
        }
        if hint_info.if_prefer_inlj(&lhs) {
            self.prefer_join_type |= PreferLeftAsINLJInner;
            self.left_prefer_join_type |= PreferINLJ;
        }
        if hint_info.if_prefer_inlj(&rhs) {
            self.prefer_join_type |= PreferRightAsINLJInner;
            self.right_prefer_join_type |= PreferINLJ;
        }
        if hint_info.if_prefer_inlhj(&lhs) {
            self.prefer_join_type |= PreferLeftAsINLHJInner;
            self.left_prefer_join_type |= PreferINLHJ;
        }
        if hint_info.if_prefer_inlhj(&rhs) {
            self.prefer_join_type |= PreferRightAsINLHJInner;
            self.right_prefer_join_type |= PreferINLHJ;
        }
        if hint_info.if_prefer_inlmj(&lhs) {
            self.prefer_join_type |= PreferLeftAsINLMJInner;
            self.left_prefer_join_type |= PreferINLMJ;
        }
        if hint_info.if_prefer_inlmj(&rhs) {
            self.prefer_join_type |= PreferRightAsINLMJInner;
            self.right_prefer_join_type |= PreferINLMJ;
        }
        if hint_info.if_prefer_no_index_join(&lhs) {
            self.prefer_join_type |= PreferNoIndexJoin;
            self.left_prefer_join_type |= PreferNoIndexJoin;
        }
        if hint_info.if_prefer_no_index_join(&rhs) {
            self.prefer_join_type |= PreferNoIndexJoin;
            self.right_prefer_join_type |= PreferNoIndexJoin;
        }
        if hint_info.if_prefer_no_index_hash_join(&lhs) {
            self.prefer_join_type |= PreferNoIndexHashJoin;
            self.left_prefer_join_type |= PreferNoIndexHashJoin;
        }
        if hint_info.if_prefer_no_index_hash_join(&rhs) {
            self.prefer_join_type |= PreferNoIndexHashJoin;
            self.right_prefer_join_type |= PreferNoIndexHashJoin;
        }
        if hint_info.if_prefer_no_index_merge_join(&lhs) {
            self.prefer_join_type |= PreferNoIndexMergeJoin;
            self.left_prefer_join_type |= PreferNoIndexMergeJoin;
        }
        if hint_info.if_prefer_no_index_merge_join(&rhs) {
            self.prefer_join_type |= PreferNoIndexMergeJoin;
            self.right_prefer_join_type |= PreferNoIndexMergeJoin;
        }
        if hint_info.if_prefer_hj_build(&lhs) {
            self.prefer_join_type |= PreferLeftAsHJBuild;
            self.left_prefer_join_type |= PreferHJBuild;
        }
        if hint_info.if_prefer_hj_build(&rhs) {
            self.prefer_join_type |= PreferRightAsHJBuild;
            self.right_prefer_join_type |= PreferHJBuild;
        }
        if hint_info.if_prefer_hj_probe(&lhs) {
            self.prefer_join_type |= PreferLeftAsHJProbe;
            self.left_prefer_join_type |= PreferHJProbe;
        }
        if hint_info.if_prefer_hj_probe(&rhs) {
            self.prefer_join_type |= PreferRightAsHJProbe;
            self.right_prefer_join_type |= PreferHJProbe;
        }
        let mut has_conflict = false;
        if !ctx.vars.enable_advanced_join_hint || ctx.vars.stmt_ctx().stmt_hints.straight_join_order
        {
            if contain_different_join_types(self.prefer_join_type) {
                has_conflict = true;
            }
        } else if ctx.vars.enable_advanced_join_hint
            && (contain_different_join_types(self.left_prefer_join_type)
                || contain_different_join_types(self.right_prefer_join_type))
        {
            has_conflict = true;
        }
        if has_conflict {
            ctx.vars
                .stmt_ctx()
                .set_hint_warning("Join hints are conflict, you can only specify one type of join");
            self.prefer_join_type = 0;
        }
        // set the join order
        if !hint_info.leading_join_order.is_empty() {
            let mut leading = std::mem::take(&mut hint_info.leading_join_order);
            self.prefer_join_order = PlanHints::match_table_name(
                &[lhs_alias.as_ref(), rhs_alias.as_ref()],
                &mut leading,
            );
            hint_info.leading_join_order = leading;
        }
        // set hintInfo for further usage if this hint info can be used.
        if self.prefer_join_type != 0 || self.prefer_join_order {
            self.hint_info = Some(Arc::new(hint_info));
        }
    }

    /// SetPreferredJoinType generates hint information for the logicalJoin based on the hint information of its left and right children.
    // go: LogicalJoin.SetPreferredJoinType L1 [M-CTX]
    pub fn set_preferred_join_type(&mut self, ctx: &PlanCtx<'_>) {
        if self.left_prefer_join_type == 0 && self.right_prefer_join_type == 0 {
            return;
        }
        self.prefer_join_type =
            set_preferred_join_type_from_one_side(self.left_prefer_join_type, true)
                | set_preferred_join_type_from_one_side(self.right_prefer_join_type, false);
        if contain_different_join_types(self.prefer_join_type) {
            ctx.vars.stmt_ctx().set_hint_warning(
                "Join hints conflict after join reorder phase, you can only specify one type of join",
            );
            self.prefer_join_type = 0;
        }
    }
}

impl LogicalPlan for LogicalJoin {
    crate::logical_plan_boilerplate!(Join, producer);

    /// ExplainInfo implements Plan interface.
    // glue: stub of LogicalJoin.ExplainInfo, blocked-by:D10.9 (expression.SortedExplainExpressionList and the %v of the equal conditions)
    fn explain_info(&self, _ctx: &PlanCtx<'_>) -> Result<String, Error> {
        Err(blocked(
            "D10.9",
            "LogicalJoin.ExplainInfo (SortedExplainExpressionList)",
        ))
    }

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: LogicalJoin.ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for equal_expr in &mut self.equal_conditions {
            resolve_expr_and_replace(equal_expr, replace);
            assert!(equal_expr.as_scalar_function().is_some(), "eq condition");
        }
        for left_expr in &mut self.left_conditions {
            resolve_expr_and_replace(left_expr, replace);
        }
        for right_expr in &mut self.right_conditions {
            resolve_expr_and_replace(right_expr, replace);
        }
        for other_expr in &mut self.other_conditions {
            resolve_expr_and_replace(other_expr, replace);
        }
    }

    /// PredicatePushDown implements the base.LogicalPlan.<1st> interface.
    // glue: stub of LogicalJoin.PredicatePushDown, blocked-by:D09.8 (predicate push down of a join, with the join builder's part B)
    fn predicate_push_down(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _predicates: Vec<ExprRef>,
        _opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        Err(blocked(
            "D09.8",
            "LogicalJoin.PredicatePushDown (comes with the join builder's part B)",
        ))
    }

    /// PruneColumns implements the base.LogicalPlan.<2nd> interface.
    // glue: stub of LogicalJoin.PruneColumns, blocked-by:D09.7 (column pruning of a join, with the join builder's part B)
    fn prune_columns(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _parent_used_cols: Vec<Arc<Column>>,
        _opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        Err(blocked(
            "D09.7",
            "LogicalJoin.PruneColumns (comes with the join builder's part B)",
        ))
    }

    /// PushDownTopN implements the base.LogicalPlan.<5th> interface.
    // glue: stub of LogicalJoin.PushDownTopN, blocked-by:D09.11 (TopN push down through a join, with the join builder's part B)
    fn push_down_top_n(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _top_n: Option<PlanId>,
        _opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        Err(blocked(
            "D09.11",
            "LogicalJoin.PushDownTopN (comes with the join builder's part B)",
        ))
    }

    /// ConstantPropagation implements the base.LogicalPlan.<8th> interface.
    // glue: stub of LogicalJoin.ConstantPropagation, blocked-by:D09.8 (constant propagation through a join, with the join builder's part B)
    fn constant_propagation(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _parent: Option<PlanId>,
        _current_child_idx: usize,
        _opt: &LogicalOptimizeOp,
    ) -> Result<Option<PlanId>, Error> {
        Err(blocked(
            "D09.8",
            "LogicalJoin.ConstantPropagation (comes with the join builder's part B)",
        ))
    }

    /// ExhaustPhysicalPlans implements the base.LogicalPlan.<14th> interface.
    // glue: stub of LogicalJoin.ExhaustPhysicalPlans, blocked-by:D10.3 (ExhaustPhysicalPlans4LogicalJoin and the physical join operators, D10.1)
    fn exhaust_physical_plans(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _prop: &PhysicalProperty,
    ) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
        Err(blocked(
            "D10.3",
            "LogicalJoin.ExhaustPhysicalPlans (physical join enumeration)",
        ))
    }

    /// DeriveStats implements the base.LogicalPlan.<11th> interface.
    // glue: stub of LogicalJoin.DeriveStats, blocked-by:D10.6 (cardinality.EstimateFullJoinRowCount)
    fn derive_stats(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _child_stats: &[Arc<StatsInfo>],
        _col_groups: &[Vec<Arc<Column>>],
    ) -> Result<Arc<StatsInfo>, Error> {
        let _ = Self::get_group_ndvs;
        Err(blocked(
            "D10.6",
            "LogicalJoin.DeriveStats (cardinality.EstimateFullJoinRowCount)",
        ))
    }

    /// ExtractColGroups implements the base.LogicalPlan.<12th> interface.
    // go: LogicalJoin.ExtractColGroups L1 [M-PLAN]
    fn extract_col_groups(
        &self,
        arena: &PlanArena,
        col_groups: &[Vec<Arc<Column>>],
    ) -> Vec<Vec<Arc<Column>>> {
        let (left_join_keys, right_join_keys, _, _) = self.get_join_keys();
        let mut extracted = Vec::with_capacity(2 + col_groups.len());
        if left_join_keys.len() > 1
            && (self.join_type == JoinType::InnerJoin
                || self.join_type == JoinType::LeftOuterJoin
                || self.join_type == JoinType::RightOuterJoin)
        {
            extracted.push(sort_columns(&left_join_keys));
            extracted.push(sort_columns(&right_join_keys));
        }
        let mut outer_schema: Option<&Schema> = None;
        if self.join_type == JoinType::LeftOuterJoin
            || self.join_type == JoinType::LeftOuterSemiJoin
            || self.join_type == JoinType::AntiLeftOuterSemiJoin
        {
            outer_schema = Some(arena.get(self.children()[0]).schema(arena));
        } else if self.join_type == JoinType::RightOuterJoin {
            outer_schema = Some(arena.get(self.children()[1]).schema(arena));
        }
        let Some(outer_schema) = outer_schema else {
            return extracted;
        };
        if col_groups.is_empty() {
            return extracted;
        }
        let (_, offsets) = outer_schema.extract_col_groups(col_groups);
        let Some(offsets) = offsets else {
            return extracted;
        };
        if offsets.is_empty() {
            return extracted;
        }
        for offset in offsets {
            extracted.push(col_groups[offset].clone());
        }
        extracted
    }

    /// PreparePossibleProperties implements base.LogicalPlan.<13th> interface.
    // go: LogicalJoin.PreparePossibleProperties L1
    fn prepare_possible_properties(
        &mut self,
        _arena: &PlanArena,
        children_properties: Vec<Vec<Vec<Arc<Column>>>>,
    ) -> Vec<Vec<Arc<Column>>> {
        let mut left_properties = children_properties[0].clone();
        let mut right_properties = children_properties[1].clone();
        // TODO: We should consider properties propagation.
        self.left_properties = left_properties.clone();
        self.right_properties = right_properties.clone();
        if self.join_type == JoinType::LeftOuterJoin
            || self.join_type == JoinType::LeftOuterSemiJoin
        {
            right_properties = Vec::new();
        } else if self.join_type == JoinType::RightOuterJoin {
            left_properties = Vec::new();
        }
        let mut result_properties =
            Vec::with_capacity(left_properties.len() + right_properties.len());
        for cols in left_properties {
            result_properties.push(cols);
        }
        for cols in right_properties {
            result_properties.push(cols);
        }
        result_properties
    }

    /// ExtractCorrelatedCols implements the base.LogicalPlan.<15th> interface.
    // go: LogicalJoin.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(
            self.equal_conditions.len()
                + self.left_conditions.len()
                + self.right_conditions.len()
                + self.other_conditions.len(),
        );
        for fun in &self.equal_conditions {
            cor_cols.extend(extract_cor_columns(fun));
        }
        for fun in &self.left_conditions {
            cor_cols.extend(extract_cor_columns(fun));
        }
        for fun in &self.right_conditions {
            cor_cols.extend(extract_cor_columns(fun));
        }
        for fun in &self.other_conditions {
            cor_cols.extend(extract_cor_columns(fun));
        }
        cor_cols
    }

    /// ConvertOuterToInnerJoin implements base.LogicalPlan.<24th> interface.
    // glue: stub of LogicalJoin.ConvertOuterToInnerJoin, blocked-by:D09.8 (outer join simplification, with the join builder's part B; no flag enables the rule in v8.5.8)
    fn convert_outer_to_inner_join(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _predicates: Vec<ExprRef>,
    ) -> Result<PlanId, Error> {
        Err(blocked(
            "D09.8",
            "LogicalJoin.ConvertOuterToInnerJoin (comes with the join builder's part B)",
        ))
    }
}

/// containDifferentJoinTypes checks whether `PreferJoinType` contains different
/// join types.
// go: containDifferentJoinTypes L1 [M-INT]
pub fn contain_different_join_types(mut prefer_join_type: u64) -> bool {
    prefer_join_type &= !PreferNoHashJoin;
    prefer_join_type &= !PreferNoMergeJoin;
    prefer_join_type &= !PreferNoIndexJoin;
    prefer_join_type &= !PreferNoIndexHashJoin;
    prefer_join_type &= !PreferNoIndexMergeJoin;

    let inl_mask = PreferRightAsINLJInner ^ PreferLeftAsINLJInner;
    let inlhj_mask = PreferRightAsINLHJInner ^ PreferLeftAsINLHJInner;
    let inlmj_mask = PreferRightAsINLMJInner ^ PreferLeftAsINLMJInner;
    let hj_right_build_mask = PreferRightAsHJBuild ^ PreferLeftAsHJProbe;
    let hj_left_build_mask = PreferLeftAsHJBuild ^ PreferRightAsHJProbe;

    let mpp_mask = PreferShuffleJoin ^ PreferBCJoin;
    let mask = inl_mask ^ inlhj_mask ^ inlmj_mask ^ hj_right_build_mask ^ hj_left_build_mask;
    let ones_count = (prefer_join_type & !mask & !mpp_mask).count_ones();
    if ones_count > 1 || ones_count == 1 && prefer_join_type & mask > 0 {
        return true;
    }

    let mut cnt = 0;
    if prefer_join_type & inl_mask > 0 {
        cnt += 1;
    }
    if prefer_join_type & inlhj_mask > 0 {
        cnt += 1;
    }
    if prefer_join_type & inlmj_mask > 0 {
        cnt += 1;
    }
    if prefer_join_type & hj_left_build_mask > 0 {
        cnt += 1;
    }
    if prefer_join_type & hj_right_build_mask > 0 {
        cnt += 1;
    }
    cnt > 1
}

// go: setPreferredJoinTypeFromOneSide L1 [M-INT]
fn set_preferred_join_type_from_one_side(mut prefer_join_type: u64, is_left: bool) -> u64 {
    let mut res_join_type = 0;
    if prefer_join_type == 0 {
        return res_join_type;
    }
    if prefer_join_type & PreferINLJ > 0 {
        prefer_join_type &= !PreferINLJ;
        if is_left {
            res_join_type |= PreferLeftAsINLJInner;
        } else {
            res_join_type |= PreferRightAsINLJInner;
        }
    }
    if prefer_join_type & PreferINLHJ > 0 {
        prefer_join_type &= !PreferINLHJ;
        if is_left {
            res_join_type |= PreferLeftAsINLHJInner;
        } else {
            res_join_type |= PreferRightAsINLHJInner;
        }
    }
    if prefer_join_type & PreferINLMJ > 0 {
        prefer_join_type &= !PreferINLMJ;
        if is_left {
            res_join_type |= PreferLeftAsINLMJInner;
        } else {
            res_join_type |= PreferRightAsINLMJInner;
        }
    }
    if prefer_join_type & PreferHJBuild > 0 {
        prefer_join_type &= !PreferHJBuild;
        if is_left {
            res_join_type |= PreferLeftAsHJBuild;
        } else {
            res_join_type |= PreferRightAsHJBuild;
        }
    }
    if prefer_join_type & PreferHJProbe > 0 {
        prefer_join_type &= !PreferHJProbe;
        if is_left {
            res_join_type |= PreferLeftAsHJProbe;
        } else {
            res_join_type |= PreferRightAsHJProbe;
        }
    }
    res_join_type |= prefer_join_type;
    res_join_type
}

/// BuildLogicalJoinSchema builds the schema for join operator. `join`'s children are read
/// from the arena; for a semi join with an auxiliary column `join`'s own schema must be set.
// go: BuildLogicalJoinSchema L1 [M-PLAN]
pub fn build_logical_join_schema(
    join_type: JoinType,
    join: &dyn LogicalPlan,
    arena: &PlanArena,
) -> Schema {
    let left_schema = arena.get(join.children()[0]).schema(arena);
    match join_type {
        JoinType::SemiJoin | JoinType::AntiSemiJoin => {
            return left_schema.clone_schema();
        }
        JoinType::LeftOuterSemiJoin | JoinType::AntiLeftOuterSemiJoin => {
            let mut new_schema = left_schema.clone_schema();
            let join_schema = join.schema(arena);
            new_schema.append([Arc::clone(&join_schema.columns[join_schema.len() - 1])]);
            return new_schema;
        }
        _ => {}
    }
    let mut new_schema = merge_schema(
        Some(left_schema),
        Some(arena.get(join.children()[1]).schema(arena)),
    )
    .expect("both schemas are given");
    if join_type == JoinType::LeftOuterJoin {
        let n = new_schema.len();
        reset_not_null_flag(&mut new_schema, left_schema.len(), n);
    } else if join_type == JoinType::RightOuterJoin {
        reset_not_null_flag(&mut new_schema, 0, left_schema.len());
    }
    new_schema
}

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
