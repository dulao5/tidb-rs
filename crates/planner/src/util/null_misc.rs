//! go-file: pkg/planner/util/null_misc.go
//!
//! The null-reject proof for outer-join simplification and `ResetNotNullFlag` (block
//! D10.10, ported with the join builder, D09.3 part A: `specs/D09.3-join-build/spec.md`
//! §2.2 and §3.7).
//!
//! Mapping:
//! - Go's `base.PlanContext` is the planning context [`PlanCtx`]; its
//!   `GetNullRejectCheckExprCtx` is [`PlanCtx::get_null_reject_check_expr_ctx`].
//! - Go's nil `*expression.Constant` with `false` is `None`.
//! - Go ignores the errors of `NewFunction` and `EvalInt` here (the sub-expression stays
//!   unproven). A `blocked` error (a builtin the Rust expression layer does not have yet) is
//!   returned instead of being treated as "unproven", so the proof never silently differs
//!   from Go's (D09.9 spec §4.4): the public functions return `Result`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::mysql::NotNullFlag;
use tidb_chunk::Row;
use tidb_expression::constant::{new_null, new_null_with_field_type};
use tidb_expression::constant_fold::{fold_constant, maybe_over_optimized4_plan_cache};
use tidb_expression::expression::EvalState;
use tidb_expression::scalar_function::new_function;
use tidb_expression::util::push_down_not;
use tidb_expression::{Constant, Expr, ExprRef, Schema};
use tidb_parser::ast::funcs as ast_funcs;
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::{BuildContext, BuildContextRef, ctx_with_handle_truncate_err_level};

use crate::planctx::PlanCtx;
use crate::util::null_misc_builtins::{
    NULL_REJECT_NULL_PRESERVING_FUNCTIONS, NullRejectTestMode, null_reject_reject_null_tests,
};

// Null-reject proof for outer-join simplification.
//
// When every column in an inner (null-producing) schema is set to SQL NULL, a
// predicate is "null-rejected" if it cannot evaluate to TRUE — proving the
// outer join can be simplified to an inner join.
//
// The proof tracks two related but distinct guarantees per sub-expression:
//
//   - nonTrue: the expression cannot be TRUE (it is FALSE or NULL).
//   - mustNull: the expression must be NULL.
//
// Both are needed because SQL uses three-valued logic. For example, NOT(expr)
// is nonTrue only when expr is mustNull (since NOT(NULL) = NULL, which is
// nonTrue; but NOT(FALSE) = TRUE, which is not). Similarly, OR(a, b) is
// nonTrue only when both sides are nonTrue, but its mustNull requires both
// sides to be mustNull.
//
// Two classification tables drive the proof for builtin scalar functions:
//
//   - nullRejectNullPreservingFunctions: functions that return NULL whenever any
//     argument is NULL. A mustNull argument propagates mustNull through these.
//   - nullRejectRejectNullTests: IS TRUE / IS FALSE style tests that convert
//     NULL to a definite boolean. These produce nonTrue when the child is
//     mustNull.
//
// The classification is intentionally conservative: unclassified builtins are
// treated as opaque (no proof is derived), which reduces optimization
// opportunities but cannot cause incorrect join simplification. When adding a
// builtin to either table, also update TestNullRejectBuiltinRegistrySnapshot
// which guards against silent registry drift.
//
// The proof works in two phases. First it reasons symbolically using the two
// proof bits above. Before giving up on a sub-expression, it also tries a
// "nullify then fold" bridge via tryFoldNullifiedConstant: replace inner-side
// columns with typed SQL NULL, and if the result becomes an immutable constant,
// classify that exact value. This recovers cases such as COALESCE/IF/IFNULL
// that may hide NULL but still collapse after nullification. The bridge stays
// conservative for plan-cache-sensitive expressions by refusing to treat
// ParamMarker/DeferredExpr values as static fold results.

/// nullRejectProof holds the two proof results for a sub-expression.
/// See the file-level comment above for the full model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NullRejectProof {
    non_true: bool,
    must_null: bool,
}

/// allConstants checks whether the expression tree consists entirely of constants.
// go: allConstants L1 [M-CTX]
fn all_constants(ctx: &dyn BuildContext, expr: &ExprRef) -> bool {
    if maybe_over_optimized4_plan_cache(ctx, std::slice::from_ref(expr)) {
        return false; // expression contains non-deterministic parameter
    }
    match &**expr {
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                if !all_constants(ctx, arg) {
                    return false;
                }
            }
            true
        }
        Expr::Constant(v) => v.param_marker.is_none() && v.deferred_expr.is_none(),
        _ => false,
    }
}

/// IsNullRejected proves whether `predicate` can be TRUE after every column in
/// `innerSchema` is replaced with SQL NULL.
///
/// `Err` only for a `blocked` boundary met while folding (see the module doc).
// go: IsNullRejected L2 [M-CTX] (returns Result: a blocked error met while folding goes up, where Go has no error to ignore)
pub fn is_null_rejected(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    predicate: &ExprRef,
    skip_plan_cache_check: bool,
) -> Result<bool, Error> {
    let _ = skip_plan_cache_check; // kept for API compatibility; the new proof does not use EvaluateExprWithNull
    let null_reject_ctx = ctx.get_null_reject_check_expr_ctx();
    // PushDownNot's nil result (a failed NewFunctionInternal) proves nothing.
    let Some(predicate) = push_down_not(&*null_reject_ctx, predicate) else {
        return Ok(false);
    };
    Ok(prove_null_rejected(ctx, inner_schema, &predicate, true)?.non_true)
}

/// proveNullRejected recursively proves the two proof bits for one expression.
///
/// The proof first tries "nullify then fold": replace inner-side columns with
/// SQL NULL and fold the expression if that becomes possible. This covers
/// null-hiding wrappers such as COALESCE/IF that cannot be proven by looking at
/// the top-level builtin alone.
///
/// Example:
///
/// ```text
///     COALESCE(t2.a, 2) > 2
/// ```
///
/// becomes
///
/// ```text
///     COALESCE(NULL, 2) > 2
/// ```
///
/// then folds to
///
/// ```text
///     2 > 2
/// ```
///
/// so the predicate is nonTrue.
///
/// allowNullifiedFold is false when proving a Constant.DeferredExpr. In that
/// mode the proof remains purely symbolic so execution-time dependent values are
/// not folded during optimization.
// go: proveNullRejected L1 [M-CTX]
fn prove_null_rejected(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    expr: &ExprRef,
    allow_nullified_fold: bool,
) -> Result<NullRejectProof, Error> {
    if allow_nullified_fold
        && let Some(cons) = try_fold_nullified_constant(ctx, inner_schema, expr)?
    {
        return Ok(proof_from_constant(ctx, Some(&cons)));
    }

    match &**expr {
        Expr::Column(x) => {
            if inner_schema.contains(x) {
                // A bare inner-side column becomes NULL after outer-join null
                // extension, so it can never be TRUE by itself.
                //
                // Example:
                //   SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.a WHERE t2.b;
                // Here `t2.b` is the column `x`. For unmatched rows it becomes
                // NULL, and `WHERE NULL` filters the row out.
                return Ok(NullRejectProof {
                    non_true: true,
                    must_null: true,
                });
            }
        }
        Expr::Constant(x) => {
            if x.param_marker.is_none()
                && let Some(deferred) = &x.deferred_expr
            {
                return prove_null_rejected(ctx, inner_schema, deferred, false);
            }
            return Ok(proof_from_constant(ctx, Some(x)));
        }
        Expr::ScalarFunction(_) => {
            return prove_null_rejected_scalar_func(ctx, inner_schema, expr, allow_nullified_fold);
        }
        Expr::CorrelatedColumn(_) => {}
    }
    Ok(NullRejectProof::default())
}

/// proveNullRejectedScalarFunc handles builtins whose proof can be derived from
/// child proofs plus SQL three-valued logic.
///
/// Most builtins fall into one of two conservative buckets:
///  1. NULL-preserving builtins: any mustNull child makes the result NULL.
///  2. NULL-tests such as IS TRUE / IS FALSE: they turn NULL into a definite
///     boolean, so they only contribute nonTrue, not always mustNull.
///
/// A few builtins need bespoke rules because their truth tables are more subtle
/// than either bucket, notably AND / OR / NOT / IN / IS NULL.
// go: proveNullRejectedScalarFunc L1 [M-CTX]
fn prove_null_rejected_scalar_func(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    expr_ref: &ExprRef,
    allow_nullified_fold: bool,
) -> Result<NullRejectProof, Error> {
    let expr = expr_ref.as_scalar_function().expect("a scalar function");
    let args = expr.get_args();
    match expr.func_name.l.as_str() {
        ast_funcs::LogicAnd => {
            let lhs = prove_null_rejected(ctx, inner_schema, &args[0], allow_nullified_fold)?;
            let rhs = prove_null_rejected(ctx, inner_schema, &args[1], allow_nullified_fold)?;
            return Ok(NullRejectProof {
                non_true: lhs.non_true || rhs.non_true,
                must_null: lhs.must_null && rhs.must_null,
            });
        }
        ast_funcs::LogicOr => {
            let lhs = prove_null_rejected(ctx, inner_schema, &args[0], allow_nullified_fold)?;
            let rhs = prove_null_rejected(ctx, inner_schema, &args[1], allow_nullified_fold)?;
            return Ok(NullRejectProof {
                non_true: lhs.non_true && rhs.non_true,
                must_null: lhs.must_null && rhs.must_null,
            });
        }
        ast_funcs::UnaryNot => {
            // NOT(IS NULL(x)): when x is mustNull, IS NULL(NULL) = TRUE and
            // NOT(TRUE) = FALSE, so nonTrue holds. mustNull does not hold
            // because the result is FALSE, not NULL.
            //
            // Example:
            //   SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.a
            //   WHERE NOT(IS NULL(t2.b));
            // After null extension, the predicate becomes NOT(IS NULL(NULL)) =
            // NOT(TRUE) = FALSE, so it is null-rejected.
            if let Some(child) = args[0].as_scalar_function()
                && child.func_name.l == ast_funcs::IsNull
            {
                return Ok(NullRejectProof {
                    non_true: prove_null_rejected(
                        ctx,
                        inner_schema,
                        &child.get_args()[0],
                        allow_nullified_fold,
                    )?
                    .must_null,
                    must_null: false,
                });
            }
            // General NOT: NOT(NULL) = NULL (nonTrue), but NOT(FALSE) = TRUE
            // (not nonTrue). So nonTrue requires child.mustNull, not just
            // child.nonTrue.
            let child = prove_null_rejected(ctx, inner_schema, &args[0], allow_nullified_fold)?;
            return Ok(NullRejectProof {
                non_true: child.must_null,
                must_null: child.must_null,
            });
        }
        ast_funcs::In => {
            return prove_null_rejected_in(ctx, inner_schema, expr_ref, allow_nullified_fold);
        }
        ast_funcs::IsNull => return Ok(NullRejectProof::default()),
        ast_funcs::Week | ast_funcs::YearWeek => {
            // Only the date argument is NULL-preserving. A NULL mode argument is
            // treated as mode 0 by MySQL/TiDB, so these functions cannot be listed
            // in nullRejectNullPreservingFunctions.
            if prove_null_rejected(ctx, inner_schema, &args[0], allow_nullified_fold)?.must_null {
                return Ok(NullRejectProof {
                    non_true: true,
                    must_null: true,
                });
            }
            return Ok(NullRejectProof::default());
        }
        _ => {}
    }

    if let Some(mode) = null_reject_reject_null_tests(&expr.func_name.l) {
        let child = prove_null_rejected(ctx, inner_schema, &args[0], allow_nullified_fold)?;
        return Ok(NullRejectProof {
            non_true: child.must_null,
            must_null: child.must_null && mode == NullRejectTestMode::KeepsNull,
        });
    }

    if NULL_REJECT_NULL_PRESERVING_FUNCTIONS.contains(expr.func_name.l.as_str()) {
        for arg in args {
            if prove_null_rejected(ctx, inner_schema, arg, allow_nullified_fold)?.must_null {
                return Ok(NullRejectProof {
                    non_true: true,
                    must_null: true,
                });
            }
        }
    }
    Ok(NullRejectProof::default())
}

/// proveNullRejectedIn handles IN(value, list...).
/// IN returns NULL (not FALSE) when value is NULL, or when all list-element
/// comparisons yield NULL, making the whole predicate nonTrue in either case.
// go: proveNullRejectedIn L1 [M-CTX]
fn prove_null_rejected_in(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    expr: &ExprRef,
    allow_nullified_fold: bool,
) -> Result<NullRejectProof, Error> {
    let args = expr.as_scalar_function().expect("IN").get_args();
    if args.is_empty() {
        return Ok(NullRejectProof::default());
    }
    let value_proof = prove_null_rejected(ctx, inner_schema, &args[0], allow_nullified_fold)?;
    if value_proof.must_null {
        return Ok(NullRejectProof {
            non_true: true,
            must_null: true,
        });
    }
    let mut all_list_must_null = true;
    for arg in &args[1..] {
        if !prove_null_rejected(ctx, inner_schema, arg, allow_nullified_fold)?.must_null {
            all_list_must_null = false;
            break;
        }
    }
    if all_list_must_null {
        return Ok(NullRejectProof {
            non_true: true,
            must_null: true,
        });
    }
    Ok(NullRejectProof::default())
}

/// tryFoldNullifiedConstant tries to materialize the expression after replacing
/// inner-side columns with SQL NULL.
///
/// This is the bridge between symbolic proof and exact constant evaluation:
/// whenever nullification turns the expression into a foldable constant, we can
/// delegate the final truth-value classification to proofFromConstant.
// go: tryFoldNullifiedConstant L1 [M-CTX]
fn try_fold_nullified_constant(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    expr: &ExprRef,
) -> Result<Option<Arc<Constant>>, Error> {
    if let Some(cons) = try_fold_static_constant(ctx, expr) {
        return Ok(Some(cons));
    }
    match &**expr {
        Expr::Column(x) => {
            if inner_schema.contains(x) {
                // Keep the original type/flags so constant folding still dispatches
                // through the same builtin signature after nullification.
                let mut ret_type = (*x.ret_type).clone();
                ret_type.del_flag(NotNullFlag);
                return Ok(Some(Arc::new(new_null_with_field_type(ret_type))));
            }
        }
        Expr::Constant(x) => {
            if x.param_marker.is_none() && x.deferred_expr.is_none() {
                return Ok(Some(Arc::new(x.clone())));
            }
        }
        Expr::ScalarFunction(_) => {
            return try_fold_nullified_scalar_func(ctx, inner_schema, expr);
        }
        Expr::CorrelatedColumn(_) => {}
    }
    Ok(None)
}

// go: tryFoldStaticConstant L1 [M-CTX]
fn try_fold_static_constant(ctx: &PlanCtx<'_>, expr: &ExprRef) -> Option<Arc<Constant>> {
    let null_reject_ctx = null_reject_fold_ctx(ctx);
    if !all_constants(&*null_reject_ctx, expr) {
        return None;
    }
    let folded = fold_constant(&*null_reject_ctx, Arc::clone(expr));
    let Expr::Constant(cons) = &*folded else {
        return None;
    };
    if cons.param_marker.is_some() || cons.deferred_expr.is_some() {
        return None;
    }
    Some(Arc::new(cons.clone()))
}

/// tryFoldNullifiedScalarFunc handles scalar functions after inner-side columns
/// are nullified.
///
/// Generic NULL-preserving builtins can use the registry plus full constant
/// folding. COALESCE/IFNULL/IF must be handled specially because they may hide a
/// NULL and still collapse to a constant after nullification.
///
/// Example:
///
/// ```text
///     COALESCE(t2.a, 2) > 2
/// ```
///
/// becomes
///
/// ```text
///     COALESCE(NULL, 2) > 2
/// ```
///
/// then
///
/// ```text
///     2 > 2
/// ```
///
/// so the predicate is provably nonTrue even though COALESCE itself is not
/// NULL-preserving.
// go: tryFoldNullifiedScalarFunc L1 [M-CTX]
fn try_fold_nullified_scalar_func(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    expr_ref: &ExprRef,
) -> Result<Option<Arc<Constant>>, Error> {
    let expr = expr_ref.as_scalar_function().expect("a scalar function");
    match expr.func_name.l.as_str() {
        ast_funcs::Coalesce | ast_funcs::Ifnull => {
            return try_fold_nullified_coalesce_like(ctx, inner_schema, expr_ref);
        }
        ast_funcs::If => return try_fold_nullified_if(ctx, inner_schema, expr_ref),
        ast_funcs::Week | ast_funcs::YearWeek => {
            let cons = try_fold_nullified_constant(ctx, inner_schema, &expr.get_args()[0])?;
            if let Some(cons) = cons
                && cons.value.is_null()
            {
                return Ok(Some(Arc::new(new_null())));
            }
            return Ok(None);
        }
        _ => {}
    }

    let mut args: Vec<ExprRef> = Vec::with_capacity(expr.get_args().len());
    let mut all_constant_args = true;
    let mut has_null_arg = false;
    for arg in expr.get_args() {
        let Some(cons) = try_fold_nullified_constant(ctx, inner_schema, arg)? else {
            all_constant_args = false;
            continue;
        };
        has_null_arg = has_null_arg || cons.value.is_null();
        args.push(Arc::new(Expr::Constant((*cons).clone())));
    }
    if NULL_REJECT_NULL_PRESERVING_FUNCTIONS.contains(expr.func_name.l.as_str()) && has_null_arg {
        return Ok(Some(Arc::new(new_null()))); // null-preserving: any NULL arg makes the result NULL
    }
    if !all_constant_args {
        return Ok(None);
    }
    fold_nullified_function(ctx, expr_ref, args)
}

/// tryFoldNullifiedCoalesceLike nullifies every argument and then returns the
/// first non-NULL folded argument, exactly matching COALESCE/IFNULL semantics.
///
/// We need this special path because COALESCE/IFNULL are explicitly not
/// NULL-preserving: a NULL child does not force the final result to be NULL.
// go: tryFoldNullifiedCoalesceLike L1 [M-CTX]
fn try_fold_nullified_coalesce_like(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    expr: &ExprRef,
) -> Result<Option<Arc<Constant>>, Error> {
    for arg in expr.as_scalar_function().expect("COALESCE").get_args() {
        let Some(cons) = try_fold_nullified_constant(ctx, inner_schema, arg)? else {
            return Ok(None);
        };
        if !cons.value.is_null() {
            return Ok(Some(cons));
        }
    }
    Ok(Some(Arc::new(new_null())))
}

/// tryFoldNullifiedIf evaluates the condition after nullification and then only
/// folds the taken branch.
///
/// IF also needs a special path: after inner columns become NULL, the condition
/// may collapse to a constant and reveal that only one branch matters.
// go: tryFoldNullifiedIf L2 [M-CTX] (a blocked EvalInt error goes up; Go ignores the error)
fn try_fold_nullified_if(
    ctx: &PlanCtx<'_>,
    inner_schema: &Schema,
    expr: &ExprRef,
) -> Result<Option<Arc<Constant>>, Error> {
    let args = expr.as_scalar_function().expect("IF").get_args();
    let Some(cond) = try_fold_nullified_constant(ctx, inner_schema, &args[0])? else {
        return Ok(None);
    };
    let eval_ctx = null_reject_fold_ctx(ctx).get_eval_ctx();
    let cond_val = match cond.eval_int(&*eval_ctx, &EvalState::new(), Row::default()) {
        Ok(v) => v,
        Err(e) if blocked_by(&e).is_some() => return Err(e),
        Err(_) => return Ok(None),
    };
    // Go's (condVal, isNull): `None` is NULL.
    if let Some(cond_val) = cond_val
        && cond_val != 0
    {
        return try_fold_nullified_constant(ctx, inner_schema, &args[1]);
    }
    try_fold_nullified_constant(ctx, inner_schema, &args[2])
}

// go: foldNullifiedFunction L2 [M-CTX] (a blocked NewFunction error goes up; Go ignores the error)
fn fold_nullified_function(
    ctx: &PlanCtx<'_>,
    expr: &ExprRef,
    args: Vec<ExprRef>,
) -> Result<Option<Arc<Constant>>, Error> {
    let null_reject_ctx = null_reject_fold_ctx(ctx);
    let sf = expr.as_scalar_function().expect("a scalar function");
    let folded = match new_function(
        &*null_reject_ctx,
        &sf.func_name.l,
        sf.ret_type().clone(),
        args,
    ) {
        Ok(f) => f,
        Err(e) if blocked_by(&e).is_some() => return Err(e),
        Err(_) => return Ok(None),
    };
    let folded = fold_constant(&*null_reject_ctx, folded);
    let Expr::Constant(cons) = &*folded else {
        return Ok(None);
    };
    if cons.param_marker.is_some() || cons.deferred_expr.is_some() {
        return Ok(None);
    }
    Ok(Some(Arc::new(cons.clone())))
}

/// proofFromConstant classifies the exact folded constant result.
///
/// NULL means both nonTrue and mustNull. Any exact FALSE-ish constant means
/// nonTrue only. TRUE or non-foldable values contribute no proof.
// go: proofFromConstant L1 [M-CTX]
fn proof_from_constant(ctx: &PlanCtx<'_>, cons: Option<&Constant>) -> NullRejectProof {
    let Some(cons) = cons else {
        return NullRejectProof::default();
    };
    if cons.param_marker.is_some() || cons.deferred_expr.is_some() {
        return NullRejectProof::default();
    }
    if cons.value.is_null() {
        return NullRejectProof {
            non_true: true,
            must_null: true,
        };
    }
    let tc = null_reject_fold_ctx(ctx).get_eval_ctx().type_ctx();
    let (is_true, err) = cons.value.to_bool(&tc);
    if err.is_ok() && is_true == 0 {
        return NullRejectProof {
            non_true: true,
            must_null: false,
        };
    }
    NullRejectProof::default()
}

// go: nullRejectFoldCtx L1 [M-CTX]
fn null_reject_fold_ctx(ctx: &PlanCtx<'_>) -> BuildContextRef {
    ctx_with_handle_truncate_err_level(ctx.get_null_reject_check_expr_ctx(), errctx::LevelIgnore)
}

/// ResetNotNullFlag resets the not null flag of [start, end] columns in the schema.
///
/// Go copies the column and its `FieldType` before clearing the flag (the shared originals
/// are untouched); the copy keeps the column's `UniqueID` (spec §4.8 item 1).
// go: ResetNotNullFlag L1 [M-FT]
pub fn reset_not_null_flag(schema: &mut Schema, start: usize, end: usize) {
    for i in start..end {
        let mut col = (*schema.columns[i]).clone();
        let mut new_field_type = (*col.ret_type).clone();
        new_field_type.del_flag(NotNullFlag);
        col.ret_type = Arc::new(new_field_type);
        schema.columns[i] = Arc::new(col);
    }
}

#[cfg(test)]
mod tests {
    //! Port of Go's `TestIsNullRejectedProofModes` (`pkg/planner/util/null_misc_test.go`):
    //! the same expressions and expected proofs. A case whose builtin the Rust expression
    //! layer does not have yet must fail with `blocked`, never with a different answer; those
    //! cases are listed in `BLOCKED` so that a newly ported builtin shows up here.

    use super::*;
    use tidb_base::mysql::{
        TypeDatetime, TypeJSON, TypeLonglong, TypeTiny, TypeVarString, UnsignedFlag,
    };
    use tidb_expression::Column;
    use tidb_expression::builtin_cast::build_cast_function;
    use tidb_expression::constant::{new_one, new_zero};
    use tidb_sessionctx::exprstatic as es;
    use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
    use tidb_types::datum::{
        Datum, new_int_datum, new_json_datum, new_string_datum, new_uint_datum,
    };
    use tidb_types::etc::FieldType;
    use tidb_types::field_type::new_field_type;
    use tidb_types::json::parse_binary_json_from_string;

    struct TestAcc;
    impl GlobalVarAccessor for TestAcc {
        fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
            Ok(())
        }
        fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    fn col(id: i64, tp: FieldType) -> ExprRef {
        Expr::Column(Column {
            unique_id: id,
            id,
            index: id as isize,
            ret_type: Arc::new(tp),
            ..Column::default()
        })
        .into_ref()
    }

    fn uint_ft(tp: u8) -> FieldType {
        let mut ft = new_field_type(tp);
        ft.add_flag(UnsignedFlag);
        ft
    }

    fn cons(value: Datum, tp: FieldType) -> ExprRef {
        Expr::Constant(Constant::new(value, tp)).into_ref()
    }
    fn str_c(s: &str) -> ExprRef {
        cons(new_string_datum(s), new_field_type(TypeVarString))
    }
    fn int_c(v: i64) -> ExprRef {
        cons(new_int_datum(v), new_field_type(TypeLonglong))
    }
    fn json_c(s: &str) -> ExprRef {
        let j = parse_binary_json_from_string(s.as_bytes()).unwrap();
        cons(new_json_datum(j), new_field_type(TypeJSON))
    }
    fn one() -> ExprRef {
        Expr::Constant(new_one()).into_ref()
    }
    fn zero() -> ExprRef {
        Expr::Constant(new_zero()).into_ref()
    }
    fn deferred(ctx: &dyn BuildContext, e: ExprRef) -> ExprRef {
        let tp = e.get_type(&*ctx.get_eval_ctx()).into_owned();
        let mut c = Constant::new(Datum::default(), tp);
        c.deferred_expr = Some(e);
        Expr::Constant(c).into_ref()
    }

    /// Cases whose builtin is not in the Rust expression layer yet (they must be `blocked`):
    /// `trim`, `format`, `field`, `quote`, `unhex` (string builtins, D08.6), `week` /
    /// `yearweek` (time builtins, D08.7), `in`, `if`, `aes_encrypt` (D08.9), and the JSON
    /// builtins (D08.10).
    const BLOCKED: &[&str] = &[
        "null_preserving_wrapper_propagates_must_null",
        "in_with_all_list_items_null_rejected",
        "in_with_non_null_candidate_is_not_proven",
        "format_with_null_locale_is_not_null_rejected",
        "field_with_null_input_can_make_not_predicate_true",
        "quote_with_null_input_can_make_not_predicate_true",
        "if_condition_folded_after_nullification_stays_provable",
        "aes_encrypt_ignores_nullable_iv_in_ecb_mode",
        "aes_decrypt_ignores_nullable_iv_in_ecb_mode",
        "json_set_nullable_value_becomes_json_null",
        "json_insert_nullable_value_becomes_json_null",
        "json_replace_nullable_value_becomes_json_null",
        "json_array_append_nullable_value_becomes_json_null",
        "json_array_insert_nullable_value_becomes_json_null",
        "json_merge_patch_nullable_argument_can_still_return_document",
        "json_search_nullable_escape_falls_back_to_default_escape",
        "week_nullable_mode_uses_default_mode_zero",
        "yearweek_nullable_mode_uses_default_mode_zero",
        "week_nullable_date_rejects_null_even_with_outer_mode",
        "yearweek_nullable_date_rejects_null_even_with_outer_mode",
    ];

    #[test]
    fn is_null_rejected_proof_modes() {
        let mut vars = SessionVars::new(Box::new(TestAcc));
        let expr_ctx = es::new_expr_context(vec![
            es::with_eval_ctx(es::new_eval_context(vec![])),
            es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
            es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
            es::with_block_encryption_mode("aes-128-ecb".to_owned()),
        ]);
        let ctx = PlanCtx::new(&mut vars, expr_ctx);
        let ectx: &dyn BuildContext = &*ctx.expr_ctx;
        let tiny = || new_field_type(TypeTiny);
        let f = |name: &str, tp: FieldType, args: Vec<ExprRef>| new_function(ectx, name, tp, args);

        let inner_a = col(1, new_field_type(TypeLonglong));
        let inner_b = col(2, new_field_type(TypeLonglong));
        let outer_c = col(3, new_field_type(TypeLonglong));
        let inner_s = col(4, new_field_type(TypeVarString));
        let inner_unsigned_d = col(5, uint_ft(TypeLonglong));
        let inner_date = col(6, new_field_type(TypeDatetime));
        let inner_schema = Schema {
            columns: [&inner_a, &inner_b, &inner_s, &inner_unsigned_d, &inner_date]
                .iter()
                .map(|e| Arc::new(e.as_column().unwrap().clone()))
                .collect(),
            ..Schema::default()
        };

        // Each case is built lazily: an unported builtin fails while building.
        type Build<'a> = Box<dyn Fn() -> Result<ExprRef, Error> + 'a>;
        let gt_inner_a_zero = || f(ast_funcs::GT, tiny(), vec![Arc::clone(&inner_a), zero()]);
        let not_null = |e: Result<ExprRef, Error>| -> Result<ExprRef, Error> {
            let is_null = f(ast_funcs::IsNull, tiny(), vec![e?])?;
            f(ast_funcs::UnaryNot, tiny(), vec![is_null])
        };
        let coalesce_inner_a_two = || {
            f(
                ast_funcs::Coalesce,
                new_field_type(TypeLonglong),
                vec![Arc::clone(&inner_a), int_c(2)],
            )
        };
        let cases: Vec<(&str, Build<'_>, bool)> = vec![
            (
                "or_needs_both_sides_non_true",
                Box::new(|| {
                    let eq = f(ast_funcs::EQ, tiny(), vec![Arc::clone(&inner_a), zero()])?;
                    let gt_c = f(ast_funcs::GT, tiny(), vec![Arc::clone(&outer_c), zero()])?;
                    let and = f(ast_funcs::LogicAnd, tiny(), vec![eq, gt_c])?;
                    f(ast_funcs::LogicOr, tiny(), vec![gt_inner_a_zero()?, and])
                }),
                true,
            ),
            (
                "not_uses_must_null",
                Box::new(|| f(ast_funcs::UnaryNot, tiny(), vec![gt_inner_a_zero()?])),
                true,
            ),
            (
                "is_null_accepts_null",
                Box::new(|| f(ast_funcs::IsNull, tiny(), vec![Arc::clone(&inner_a)])),
                false,
            ),
            (
                "is_true_rejects_null",
                Box::new(|| {
                    f(
                        ast_funcs::IsTruthWithNull,
                        tiny(),
                        vec![Arc::clone(&inner_a)],
                    )
                }),
                true,
            ),
            (
                "not_is_null_rejects_null",
                Box::new(|| not_null(Ok(Arc::clone(&inner_a)))),
                true,
            ),
            (
                "null_preserving_wrapper_propagates_must_null",
                Box::new(|| {
                    let cast = build_cast_function(
                        ectx,
                        Arc::clone(&inner_a),
                        new_field_type(TypeVarString),
                    )?;
                    let trim = f(ast_funcs::Trim, new_field_type(TypeVarString), vec![cast])?;
                    f(ast_funcs::Like, tiny(), vec![trim, str_c("1%"), int_c(92)])
                }),
                true,
            ),
            (
                "coalesce_constant_fallback_can_still_be_non_true",
                Box::new(|| {
                    f(
                        ast_funcs::GT,
                        tiny(),
                        vec![coalesce_inner_a_two()?, int_c(2)],
                    )
                }),
                true,
            ),
            (
                "null_hiding_wrapper_stays_conservative",
                Box::new(|| {
                    let c = f(
                        ast_funcs::Coalesce,
                        new_field_type(TypeLonglong),
                        vec![Arc::clone(&inner_a), one()],
                    )?;
                    f(ast_funcs::GT, tiny(), vec![c, zero()])
                }),
                false,
            ),
            (
                "in_with_all_list_items_null_rejected",
                Box::new(|| {
                    f(
                        ast_funcs::In,
                        tiny(),
                        vec![one(), Arc::clone(&inner_a), Arc::clone(&inner_b)],
                    )
                }),
                true,
            ),
            (
                "in_with_non_null_candidate_is_not_proven",
                Box::new(|| {
                    f(
                        ast_funcs::In,
                        tiny(),
                        vec![one(), Arc::clone(&inner_a), one()],
                    )
                }),
                false,
            ),
            (
                "null_safe_eq_with_non_null_constant_rejects_null",
                Box::new(|| f(ast_funcs::NullEQ, tiny(), vec![Arc::clone(&inner_a), one()])),
                true,
            ),
            (
                "format_with_null_locale_is_not_null_rejected",
                Box::new(|| {
                    let fmt = f(
                        ast_funcs::Format,
                        new_field_type(TypeVarString),
                        vec![int_c(12345), int_c(0), Arc::clone(&inner_s)],
                    )?;
                    f(ast_funcs::EQ, tiny(), vec![fmt, str_c("12,345")])
                }),
                false,
            ),
            (
                "field_with_null_input_can_make_not_predicate_true",
                Box::new(|| {
                    let field = f(
                        ast_funcs::Field,
                        new_field_type(TypeLonglong),
                        vec![Arc::clone(&inner_a), one()],
                    )?;
                    let gt = f(ast_funcs::GT, tiny(), vec![field, zero()])?;
                    f(ast_funcs::UnaryNot, tiny(), vec![gt])
                }),
                false,
            ),
            (
                "quote_with_null_input_can_make_not_predicate_true",
                Box::new(|| {
                    let quote = f(
                        ast_funcs::Quote,
                        new_field_type(TypeVarString),
                        vec![Arc::clone(&inner_s)],
                    )?;
                    let like = f(ast_funcs::Like, tiny(), vec![quote, str_c("A%"), int_c(92)])?;
                    f(ast_funcs::UnaryNot, tiny(), vec![like])
                }),
                false,
            ),
            (
                "comparison_over_non_true_and_is_not_null_rejected",
                Box::new(|| {
                    let null = Expr::Constant(new_null()).into_ref();
                    let or = f(ast_funcs::LogicOr, tiny(), vec![Arc::clone(&inner_a), null])?;
                    let ne = f(
                        ast_funcs::NE,
                        tiny(),
                        vec![Arc::clone(&outer_c), Arc::clone(&outer_c)],
                    )?;
                    let and = f(ast_funcs::LogicAnd, tiny(), vec![or, ne])?;
                    f(ast_funcs::GE, tiny(), vec![one(), and])
                }),
                false,
            ),
            (
                "if_condition_folded_after_nullification_stays_provable",
                Box::new(|| {
                    let is_null = f(ast_funcs::IsNull, tiny(), vec![Arc::clone(&inner_a)])?;
                    let iff = f(
                        ast_funcs::If,
                        new_field_type(TypeLonglong),
                        vec![is_null, zero(), Arc::clone(&outer_c)],
                    )?;
                    f(ast_funcs::GT, tiny(), vec![iff, zero()])
                }),
                true,
            ),
            (
                "truncate_with_unsigned_nullable_scale_is_not_null_preserving",
                Box::new(|| {
                    let t = f(
                        ast_funcs::Truncate,
                        uint_ft(TypeLonglong),
                        vec![
                            cons(new_uint_datum(123), uint_ft(TypeLonglong)),
                            Arc::clone(&inner_unsigned_d),
                        ],
                    )?;
                    f(ast_funcs::GT, tiny(), vec![t, zero()])
                }),
                false,
            ),
            (
                "aes_encrypt_ignores_nullable_iv_in_ecb_mode",
                Box::new(|| {
                    not_null(f(
                        ast_funcs::AesEncrypt,
                        new_field_type(TypeVarString),
                        vec![str_c("pingcap"), str_c("123"), Arc::clone(&inner_s)],
                    ))
                }),
                false,
            ),
            (
                "aes_decrypt_ignores_nullable_iv_in_ecb_mode",
                Box::new(|| {
                    let unhex = f(
                        ast_funcs::Unhex,
                        new_field_type(TypeVarString),
                        vec![str_c("996E0CA8688D7AD20819B90B273E01C6")],
                    )?;
                    not_null(f(
                        ast_funcs::AesDecrypt,
                        new_field_type(TypeVarString),
                        vec![unhex, str_c("123"), Arc::clone(&inner_s)],
                    ))
                }),
                false,
            ),
            (
                "json_set_nullable_value_becomes_json_null",
                Box::new(|| {
                    not_null(f(
                        ast_funcs::JSONSet,
                        new_field_type(TypeJSON),
                        vec![json_c("{}"), str_c("$.a"), Arc::clone(&inner_s)],
                    ))
                }),
                false,
            ),
            (
                "json_insert_nullable_value_becomes_json_null",
                Box::new(|| {
                    not_null(f(
                        ast_funcs::JSONInsert,
                        new_field_type(TypeJSON),
                        vec![json_c("{}"), str_c("$.a"), Arc::clone(&inner_s)],
                    ))
                }),
                false,
            ),
            (
                "json_replace_nullable_value_becomes_json_null",
                Box::new(|| {
                    not_null(f(
                        ast_funcs::JSONReplace,
                        new_field_type(TypeJSON),
                        vec![json_c(r#"{"a": 1}"#), str_c("$.a"), Arc::clone(&inner_s)],
                    ))
                }),
                false,
            ),
            (
                "json_array_append_nullable_value_becomes_json_null",
                Box::new(|| {
                    not_null(f(
                        ast_funcs::JSONArrayAppend,
                        new_field_type(TypeJSON),
                        vec![json_c("[]"), str_c("$"), Arc::clone(&inner_s)],
                    ))
                }),
                false,
            ),
            (
                "json_array_insert_nullable_value_becomes_json_null",
                Box::new(|| {
                    not_null(f(
                        ast_funcs::JSONArrayInsert,
                        new_field_type(TypeJSON),
                        vec![json_c("[]"), str_c("$[0]"), Arc::clone(&inner_s)],
                    ))
                }),
                false,
            ),
            (
                "json_merge_patch_nullable_argument_can_still_return_document",
                Box::new(|| {
                    let cast =
                        build_cast_function(ectx, Arc::clone(&inner_s), new_field_type(TypeJSON))?;
                    not_null(f(
                        ast_funcs::JSONMergePatch,
                        new_field_type(TypeJSON),
                        vec![
                            cast,
                            json_c("null"),
                            json_c(r#"{"a": 1}"#),
                            json_c("[1, 2, 3]"),
                        ],
                    ))
                }),
                false,
            ),
            (
                "json_search_nullable_escape_falls_back_to_default_escape",
                Box::new(|| {
                    not_null(f(
                        ast_funcs::JSONSearch,
                        new_field_type(TypeJSON),
                        vec![
                            json_c(r#"["abc"]"#),
                            str_c("one"),
                            str_c("abc"),
                            Arc::clone(&inner_s),
                        ],
                    ))
                }),
                false,
            ),
            (
                "week_nullable_mode_uses_default_mode_zero",
                Box::new(|| {
                    let w = f(
                        ast_funcs::Week,
                        new_field_type(TypeLonglong),
                        vec![str_c("2024-01-08"), Arc::clone(&inner_a)],
                    )?;
                    f(ast_funcs::GE, tiny(), vec![w, zero()])
                }),
                false,
            ),
            (
                "yearweek_nullable_mode_uses_default_mode_zero",
                Box::new(|| {
                    let w = f(
                        ast_funcs::YearWeek,
                        new_field_type(TypeLonglong),
                        vec![str_c("2024-01-08"), Arc::clone(&inner_a)],
                    )?;
                    f(ast_funcs::GE, tiny(), vec![w, zero()])
                }),
                false,
            ),
            (
                "week_nullable_date_rejects_null_even_with_outer_mode",
                Box::new(|| {
                    let w = f(
                        ast_funcs::Week,
                        new_field_type(TypeLonglong),
                        vec![Arc::clone(&inner_date), Arc::clone(&outer_c)],
                    )?;
                    f(ast_funcs::GE, tiny(), vec![w, zero()])
                }),
                true,
            ),
            (
                "yearweek_nullable_date_rejects_null_even_with_outer_mode",
                Box::new(|| {
                    let w = f(
                        ast_funcs::YearWeek,
                        new_field_type(TypeLonglong),
                        vec![Arc::clone(&inner_date), Arc::clone(&outer_c)],
                    )?;
                    f(ast_funcs::GE, tiny(), vec![w, zero()])
                }),
                true,
            ),
            (
                "deferred_expr_uses_symbolic_null_reject_proof",
                Box::new(|| Ok(deferred(ectx, gt_inner_a_zero()?))),
                true,
            ),
            (
                "deferred_expr_skips_nullified_fold",
                Box::new(|| {
                    let gt = f(
                        ast_funcs::GT,
                        tiny(),
                        vec![coalesce_inner_a_two()?, int_c(2)],
                    )?;
                    Ok(deferred(ectx, gt))
                }),
                false,
            ),
            (
                "deferred_expr_does_not_classify_placeholder_null",
                Box::new(|| Ok(deferred(ectx, one()))),
                false,
            ),
        ];

        let mut blocked_cases = Vec::new();
        for (name, build, expected) in cases {
            let res = build().and_then(|e| is_null_rejected(&ctx, &inner_schema, &e, true));
            match res {
                Ok(got) => assert_eq!(got, expected, "{name}"),
                Err(e) => {
                    assert!(blocked_by(&e).is_some(), "{name}: {e}");
                    blocked_cases.push(name);
                }
            }
        }
        assert_eq!(
            blocked_cases, BLOCKED,
            "cases stopped at an unported builtin"
        );
    }

    /// The tables of null-preserving and null-rejecting builtins name only registered
    /// builtins (Go's `TestNullRejectBuiltinRegistrySnapshot`, without the hash of Go's
    /// registry, which the Rust registry does not reproduce).
    #[test]
    fn null_reject_tables_name_registered_builtins() {
        let names = tidb_expression::builtin_registry::registered_builtin_function_names();
        for name in NULL_REJECT_NULL_PRESERVING_FUNCTIONS.iter() {
            if *name == ast_funcs::Cast {
                continue;
            }
            assert!(names.contains(name), "{name}");
        }
        for name in [
            ast_funcs::IsTruthWithoutNull,
            ast_funcs::IsTruthWithNull,
            ast_funcs::IsFalsity,
        ] {
            assert!(null_reject_reject_null_tests(name).is_some(), "{name}");
            assert!(names.contains(&name), "{name}");
        }
    }

    /// ResetNotNullFlag copies the columns it changes (the source schema keeps NOT NULL).
    #[test]
    fn reset_not_null_flag_copies_columns() {
        let mut ft = new_field_type(TypeLonglong);
        ft.add_flag(NotNullFlag);
        let c = Arc::new(Column {
            unique_id: 7,
            ret_type: Arc::new(ft),
            ..Column::default()
        });
        let orig = Schema {
            columns: vec![Arc::clone(&c), Arc::clone(&c)],
            ..Schema::default()
        };
        let mut s = orig.clone_schema();
        reset_not_null_flag(&mut s, 1, 2);
        let flags: Vec<bool> = s
            .columns
            .iter()
            .map(|c| tidb_base::mysql::has_not_null_flag(c.ret_type.get_flag()))
            .collect();
        assert_eq!(flags, vec![true, false]);
        assert_eq!(s.columns[1].unique_id, 7);
        assert!(tidb_base::mysql::has_not_null_flag(c.ret_type.get_flag()));
    }
}
