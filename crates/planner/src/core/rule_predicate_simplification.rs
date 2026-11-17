//! go-file: pkg/planner/core/rule_predicate_simplification.go
//!
//! PredicateSimplification consolidates different predcicates on a column and its
//! equivalence classes (block D09.8). Initial out is for
//!  1. in-list and not equal list intersection.
//!  2. Drop OR predicates if they are empty for this pattern: P AND (P1 OR P2 ... OR Pn)
//!     Pi is removed if P & Pi is false/empty.
//!
//! Two Go comparisons of columns are by identity (`iCol == jCol` on `*Column` pointers) or by
//! the cascades `Column.Equals`; expression nodes are not shared by identity in Rust (an
//! `Expr::Column` holds its column by value), so both compare the columns with
//! `Column.Equal` (same `UniqueID`). Go's rewriter puts the schema's own `*Column` into every
//! reference of a column, so the pointers are equal exactly when the unique ids are, except
//! for columns cloned by an earlier rewrite (spec §4 item 3).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::TypeTiny;
use tidb_expression::constant_fold::maybe_over_optimized4_plan_cache;
use tidb_expression::constant_propagation::propagate_constant;
use tidb_expression::scalar_function::new_function;
use tidb_expression::util::maybe_over_optimized4_plan_cache_for_multi_expression;
use tidb_expression::{Column, Constant, Expr, ExprRef, split_cnf_items};
use tidb_parser::ast::funcs as ast;
use tidb_types::collate::compatible_collate;
use tidb_types::datum::new_int_datum;
use tidb_parser::types::ETString;
use tidb_types::field_type::new_field_type;

use crate::arena::PlanId;
use crate::base::{self, LogicalOptRule};
use crate::planctx::PlanCtx;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// PredicateSimplification consolidates different predcicates on a column and its
/// equivalence classes.
pub struct PredicateSimplification;

type PredicateType = u8;

const IN_LIST_PREDICATE: PredicateType = 0;
const NOT_EQUAL_PREDICATE: PredicateType = 1;
const EQUAL_PREDICATE: PredicateType = 2;
const LESS_THAN_PREDICATE: PredicateType = 3;
const GREATER_THAN_PREDICATE: PredicateType = 4;
const LESS_THAN_OR_EQUAL_PREDICATE: PredicateType = 5;
const GREATER_THAN_OR_EQUAL_PREDICATE: PredicateType = 6;
const OR_PREDICATE: PredicateType = 7;
const SCALAR_PREDICATE: PredicateType = 8;
const OTHER_PREDICATE: PredicateType = 9;

// go: findPredicateType L1
fn find_predicate_type(expr: &Expr) -> (Option<&Column>, PredicateType) {
    match expr {
        Expr::ScalarFunction(v) => {
            if v.func_name.l == ast::LogicOr {
                return (None, OR_PREDICATE);
            }
            let args = v.get_args();
            if args.is_empty() {
                return (None, OTHER_PREDICATE);
            }
            let Expr::Column(col) = &*args[0] else {
                return (None, OTHER_PREDICATE);
            };
            if args.len() > 1 && !matches!(&*args[1], Expr::Constant(_)) {
                return (None, OTHER_PREDICATE);
            }
            let name = v.func_name.l.as_str();
            if name == ast::NE {
                (Some(col), NOT_EQUAL_PREDICATE)
            } else if name == ast::EQ {
                (Some(col), EQUAL_PREDICATE)
            } else if name == ast::LT {
                (Some(col), LESS_THAN_PREDICATE)
            } else if name == ast::GT {
                (Some(col), GREATER_THAN_PREDICATE)
            } else if name == ast::LE {
                (Some(col), LESS_THAN_OR_EQUAL_PREDICATE)
            } else if name == ast::GE {
                (Some(col), GREATER_THAN_OR_EQUAL_PREDICATE)
            } else if name == ast::In {
                for value in &args[1..] {
                    if !matches!(&**value, Expr::Constant(_)) {
                        return (None, OTHER_PREDICATE);
                    }
                }
                (Some(col), IN_LIST_PREDICATE)
            } else {
                (None, OTHER_PREDICATE)
            }
        }
        _ => (None, OTHER_PREDICATE),
    }
}

/// Go's `iCol == jCol` on the `*Column` pointers (see the module doc).
// glue: identity comparison of two *Column (module doc)
fn same_column(a: Option<&Column>, b: Option<&Column>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.unique_id == b.unique_id,
        _ => false,
    }
}

impl LogicalOptRule for PredicateSimplification {
    /// Optimize implements base.LogicalOptRule.<0th> interface.
    // go: PredicateSimplification.Optimize L1
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        Ok((base::predicate_simplification(ctx, p, opt)?, plan_changed))
    }

    /// Name implements base.LogicalOptRule.<1st> interface.
    // go: PredicateSimplification.Name L1
    fn name(&self) -> &'static str {
        "predicate_simplification"
    }
}

/// updateInPredicate applies intersection of an in list with <> value. It returns updated
/// In list and a flag for a special case if an element in the inlist is not removed to keep
/// the list not empty. An `Err` is the error Go's `NewFunctionInternal` logs before
/// returning nil (an IN function this port cannot build, e.g. `blocked-by:D08.9`).
// go: updateInPredicate L2 (NewFunctionInternal's logged error is returned)
fn update_in_predicate(
    ctx: &PlanCtx<'_>,
    in_predicate: &ExprRef,
    not_eq_predicate: &ExprRef,
) -> Result<(ExprRef, bool), Error> {
    let (_, in_predicate_type) = find_predicate_type(in_predicate);
    let (_, not_eq_predicate_type) = find_predicate_type(not_eq_predicate);
    if in_predicate_type != IN_LIST_PREDICATE || not_eq_predicate_type != NOT_EQUAL_PREDICATE {
        return Ok((Arc::clone(in_predicate), true));
    }
    let Expr::ScalarFunction(v) = &**in_predicate else {
        unreachable!("an IN predicate");
    };
    let Expr::ScalarFunction(ne) = &**not_eq_predicate else {
        unreachable!("a NE predicate");
    };
    let Expr::Constant(not_eq_value) = &*ne.get_args()[1] else {
        unreachable!("a NE predicate");
    };
    // do not simplify != NULL since it is always false.
    if not_eq_value.value.is_null() {
        return Ok((Arc::clone(in_predicate), true));
    }
    let ectx = ctx.eval_ctx();
    let mut new_values = Vec::with_capacity(v.get_args().len());
    let mut last_value: Option<ExprRef> = None;
    for element in v.get_args() {
        let value_ok = matches!(&**element, Expr::Constant(_));
        let redundant_value = match &**element {
            Expr::Constant(value) => value.equal(&*ectx, &ne.get_args()[1]),
            _ => false,
        };
        if !redundant_value {
            new_values.push(Arc::clone(element));
        }
        if value_ok {
            last_value = Some(Arc::clone(element));
        }
    }
    // Special case if all IN list values are prunned. Ideally, this is False condition
    // which can be optimized with LogicalDual. But, this is already done. TODO: the false
    // optimization and its propagation through query tree will be added part of predicate simplification.
    let mut special_case = false;
    if new_values.len() < 2 {
        // lastValue is non-nil: the IN list has the column and at least one constant.
        new_values.push(last_value.expect("a constant of the IN list"));
        special_case = true;
    }
    let new_pred = new_function(
        &*ctx.expr_ctx,
        &v.func_name.l,
        v.ret_type().clone(),
        new_values,
    )?;
    Ok((new_pred, special_case))
}

/// splitCNF converts AND to list using SplitCNFItems. It is needed since simplification may
/// lead to AND at the top level. Several optimizations are based on a list of predicates and
/// AND will block those.
// go: splitCNF L1
fn split_cnf(conditions: Vec<ExprRef>) -> Vec<ExprRef> {
    let mut new_conditions = Vec::with_capacity(conditions.len());
    for cond in &conditions {
        new_conditions.extend(split_cnf_items(cond));
    }
    new_conditions
}

/// applyPredicateSimplification. An `Err` is a function this port cannot build yet (see
/// `update_in_predicate`).
// go: applyPredicateSimplification L1
pub fn apply_predicate_simplification(
    sctx: &PlanCtx<'_>,
    predicates: &[ExprRef],
) -> Result<Vec<ExprRef>, Error> {
    let mut simplified_predicate = merge_in_and_not_eq_lists(sctx, predicates.to_vec())?;
    prune_empty_or_branches(sctx, &mut simplified_predicate)?;
    simplified_predicate = split_cnf(simplified_predicate);
    Ok(simplified_predicate)
}

// go: mergeInAndNotEQLists L1
fn merge_in_and_not_eq_lists(
    sctx: &PlanCtx<'_>,
    mut predicates: Vec<ExprRef>,
) -> Result<Vec<ExprRef>, Error> {
    if predicates.len() <= 1 {
        return Ok(predicates);
    }
    let mut special_case;
    let mut remove_values: Vec<usize> = Vec::with_capacity(predicates.len());
    for i in 0..predicates.len() {
        for j in i + 1..predicates.len() {
            let ith_predicate = Arc::clone(&predicates[i]);
            let jth_predicate = Arc::clone(&predicates[j]);
            let (i_col, i_type) = find_predicate_type(&ith_predicate);
            let (j_col, j_type) = find_predicate_type(&jth_predicate);
            let maybe_over_optimized4_plan_cache =
                maybe_over_optimized4_plan_cache_for_multi_expression(
                    &*sctx.expr_ctx,
                    &[Arc::clone(&ith_predicate), Arc::clone(&jth_predicate)],
                );
            if same_column(i_col, j_col) {
                if i_type == NOT_EQUAL_PREDICATE && j_type == IN_LIST_PREDICATE {
                    (predicates[j], special_case) =
                        update_in_predicate(sctx, &jth_predicate, &ith_predicate)?;
                    if maybe_over_optimized4_plan_cache {
                        sctx.vars
                            .stmt_ctx()
                            .plan_cache_tracker
                            .set_skip_plan_cache("NE/INList simplification is triggered");
                    }
                    if !special_case {
                        remove_values.push(i);
                    }
                } else if i_type == IN_LIST_PREDICATE && j_type == NOT_EQUAL_PREDICATE {
                    (predicates[i], special_case) =
                        update_in_predicate(sctx, &ith_predicate, &jth_predicate)?;
                    if maybe_over_optimized4_plan_cache {
                        sctx.vars
                            .stmt_ctx()
                            .plan_cache_tracker
                            .set_skip_plan_cache("NE/INList simplification is triggered");
                    }
                    if !special_case {
                        remove_values.push(j);
                    }
                }
            }
        }
    }
    let mut new_values = Vec::with_capacity(predicates.len());
    for (i, value) in predicates.into_iter().enumerate() {
        if !remove_values.contains(&i) {
            new_values.push(value);
        }
    }
    Ok(new_values)
}

/// Check for constant false condition.
// go: unsatisfiableExpression L1
fn unsatisfiable_expression(ctx: &PlanCtx<'_>, p: &Expr) -> bool {
    if let Expr::Constant(const_expr) = p {
        let (b, err) = const_expr.value.to_bool(&ctx.vars.stmt_ctx().type_ctx());
        if err.is_ok() && b == 0 {
            return true;
        }
    }
    false
}

// go: unsatisfiable L2 (col1.Equals(col2), the cascades Column.Equals, compares the unique ids: module doc)
fn unsatisfiable(ctx: &PlanCtx<'_>, p1: &ExprRef, p2: &ExprRef) -> bool {
    let (col1, p1_type) = find_predicate_type(p1);
    let (col2, p2_type) = find_predicate_type(p2);
    let Some(col1) = col1 else {
        return false;
    };
    if !same_column(Some(col1), col2) {
        return false;
    }
    let (equal_pred, other_pred) = if p1_type == EQUAL_PREDICATE {
        (p1, p2)
    } else if p2_type == EQUAL_PREDICATE {
        (p2, p1)
    } else {
        return false;
    };
    // Copy constant from equal predicate into other predicate.
    let (Expr::ScalarFunction(equal_value), Expr::ScalarFunction(other_value)) =
        (&**equal_pred, &**other_pred)
    else {
        return false;
    };
    let (Expr::Constant(equal_value_const), Expr::Constant(other_value_const)) =
        (&*equal_value.get_args()[1], &*other_value.get_args()[1])
    else {
        return false;
    };
    let eval_ctx = ctx.eval_ctx();
    // We have checked the equivalence between col1 and col2, so we can safely use col1's collation.
    let col_collate = col1.get_type().get_collate().to_string();
    // Different connection collations can affect the results here, leading to different simplified results and ultimately impacting the execution outcomes.
    // Observing MySQL v8.0.31, this area does not perform string simplification, so we can directly skip it.
    // If the collation is not compatible, we cannot simplify the expression.
    if let Some(equal_value_type) = equal_value_const.get_type(&*eval_ctx)
        && equal_value_type.eval_type() == ETString
        && !compatible_collate(equal_value_type.get_collate(), &col_collate)
    {
        return false;
    }
    if let Some(other_value_type) = other_value_const.get_type(&*eval_ctx)
        && other_value_type.eval_type() == ETString
        && !compatible_collate(other_value_type.get_collate(), &col_collate)
    {
        return false;
    }
    let Ok(new_pred) = new_function(
        &*ctx.expr_ctx,
        &other_value.func_name.l,
        other_value.ret_type().clone(),
        vec![
            Arc::clone(&equal_value.get_args()[1]),
            Arc::clone(&other_value.get_args()[1]),
        ],
    ) else {
        return false;
    };
    let new_pred_list = propagate_constant(&ctx.expr_ctx, &[new_pred]);
    unsatisfiable_expression(ctx, &new_pred_list[0])
}

// go: comparisonPred L1
fn comparison_pred(pred_type: PredicateType) -> PredicateType {
    if pred_type == EQUAL_PREDICATE
        || pred_type == LESS_THAN_PREDICATE
        || pred_type == GREATER_THAN_PREDICATE
        || pred_type == LESS_THAN_OR_EQUAL_PREDICATE
        || pred_type == GREATER_THAN_OR_EQUAL_PREDICATE
    {
        return SCALAR_PREDICATE;
    }
    pred_type
}

/// updateOrPredicate simplifies OR predicates by dropping OR predicates if they are empty.
/// It is applied for this pattern: P AND (P1 OR P2 ... OR Pn)
/// Pi is removed if P & Pi is false/empty.
///
/// The second bool parameter in the return indicates whether the OR expression has changed.
/// If this is a parameter with a param marker, the plan cache needs to be skipped to avoid issues.
// go: updateOrPredicate L1
fn update_or_predicate(
    ctx: &PlanCtx<'_>,
    or_predicate_list: &ExprRef,
    scalar_predicate_ptr: &ExprRef,
) -> (ExprRef, bool) {
    let (_, or_predicate_type) = find_predicate_type(or_predicate_list);
    let (_, scalar_predicate_type) = find_predicate_type(scalar_predicate_ptr);
    let scalar_predicate_type = comparison_pred(scalar_predicate_type);
    if or_predicate_type != OR_PREDICATE || scalar_predicate_type != SCALAR_PREDICATE {
        return (Arc::clone(or_predicate_list), false);
    }
    let Expr::ScalarFunction(v) = &**or_predicate_list else {
        unreachable!("an OR predicate");
    };
    let mut first_condition = Arc::clone(&v.get_args()[0]);
    let mut second_condition = Arc::clone(&v.get_args()[1]);
    let (_, first_condition_type) = find_predicate_type(&first_condition);
    let (_, second_condition_type) = find_predicate_type(&second_condition);
    let mut empty_first = false;
    let mut empty_second = false;
    let (mut is_first_condition_changed, mut is_second_condition_changed) = (false, false);
    if comparison_pred(first_condition_type) == SCALAR_PREDICATE {
        empty_first = unsatisfiable(ctx, &first_condition, scalar_predicate_ptr);
    } else if first_condition_type == OR_PREDICATE {
        (first_condition, is_first_condition_changed) =
            update_or_predicate(ctx, &first_condition, scalar_predicate_ptr);
    }
    if comparison_pred(second_condition_type) == SCALAR_PREDICATE {
        empty_second = unsatisfiable(ctx, &second_condition, scalar_predicate_ptr);
    } else if second_condition_type == OR_PREDICATE {
        (second_condition, is_second_condition_changed) =
            update_or_predicate(ctx, &second_condition, scalar_predicate_ptr);
    }
    empty_first = empty_first || unsatisfiable_expression(ctx, &first_condition);
    empty_second = empty_second || unsatisfiable_expression(ctx, &second_condition);
    if empty_first && !empty_second {
        return (second_condition, true);
    } else if !empty_first && empty_second {
        return (first_condition, true);
    } else if empty_first && empty_second {
        return (
            Expr::Constant(Constant::new(new_int_datum(0), new_field_type(TypeTiny))).into_ref(),
            true,
        );
    }
    let Ok(new_pred) = new_function(
        &*ctx.expr_ctx,
        ast::LogicOr,
        v.ret_type().clone(),
        vec![first_condition, second_condition],
    ) else {
        return (Arc::clone(or_predicate_list), false);
    };
    (new_pred, is_first_condition_changed || is_second_condition_changed)
}

/// pruneEmptyORBranches applies iteratively updateOrPredicate for each pair of OR predicate
/// and another scalar predicate.
// go: pruneEmptyORBranches L1
fn prune_empty_or_branches(sctx: &PlanCtx<'_>, predicates: &mut [ExprRef]) -> Result<(), Error> {
    if predicates.len() <= 1 {
        return Ok(());
    }
    let expr_ctx = &*sctx.expr_ctx;
    for i in 0..predicates.len() {
        for j in i + 1..predicates.len() {
            let ith_predicate = Arc::clone(&predicates[i]);
            let jth_predicate = Arc::clone(&predicates[j]);
            let (_, i_type) = find_predicate_type(&ith_predicate);
            let (_, j_type) = find_predicate_type(&jth_predicate);
            let i_type = comparison_pred(i_type);
            let j_type = comparison_pred(j_type);
            let is_changed;
            if i_type == SCALAR_PREDICATE && j_type == OR_PREDICATE {
                let maybe_over_optimized4_plan_cache = maybe_over_optimized4_plan_cache(
                    expr_ctx,
                    std::slice::from_ref(&jth_predicate),
                );
                (predicates[j], is_changed) =
                    update_or_predicate(sctx, &jth_predicate, &ith_predicate);
                if maybe_over_optimized4_plan_cache && is_changed {
                    sctx.vars
                        .stmt_ctx()
                        .plan_cache_tracker
                        .set_skip_plan_cache("OR predicate simplification is triggered");
                }
            } else if i_type == OR_PREDICATE && j_type == SCALAR_PREDICATE {
                let maybe_over_optimized4_plan_cache = maybe_over_optimized4_plan_cache(
                    expr_ctx,
                    std::slice::from_ref(&ith_predicate),
                );
                (predicates[i], is_changed) =
                    update_or_predicate(sctx, &ith_predicate, &jth_predicate);
                if maybe_over_optimized4_plan_cache && is_changed {
                    sctx.vars
                        .stmt_ctx()
                        .plan_cache_tracker
                        .set_skip_plan_cache("OR predicate simplification is triggered");
                }
            }
        }
    }
    Ok(())
}
