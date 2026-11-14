//! go-file: pkg/expression/constant_fold.go
//!
//! Constant folding (block D08.2 part 2): `FoldConstant` and the folding of IF, IFNULL,
//! CASE and ISNULL. The special handlers only see nodes of those functions, which come with
//! their blocks (D08.4/D08.9); they are ported now so that folding is complete when they do.
//! Plus `MaybeOverOptimized4PlanCache` / `containMutableConst` (util.go, D08.12), which
//! `foldConstant` calls.

use std::sync::Arc;

use tidb_base::mysql::{self, NotNullFlag};
use tidb_chunk::Row;
use tidb_types::datum::Datum;

use crate::ast_funcs as ast;
use crate::constant::{Constant, new_one, new_zero};
use crate::context::{BuildContext, EvalContext, type_ctx};
use crate::expression::{EvalState, Expr, ExprRef};
use crate::function_traits::unFoldableFunctions;
use crate::scalar_function::new_function_base;

/// FoldConstant does constant folding optimization on an expression excluding deferred
/// ones.
// go: FoldConstant L1
pub fn fold_constant(ctx: &dyn BuildContext, expr: ExprRef) -> ExprRef {
    let (mut e, _) = fold_constant_inner(ctx, &expr);
    // keep the original coercibility, charset, collation and repertoire values after folding
    let ectx = ctx.get_eval_ctx();
    let coer = expr.coercibility();
    let tp = expr.get_type(&*ectx).into_owned();
    let repe = expr.repertoire();
    set_coercibility(&e, coer);
    if !Arc::ptr_eq(&e, &expr) {
        crate::builtin_cast::with_type_mut(&mut e, |t| {
            t.set_charset(tp.get_charset());
            t.set_collate(tp.get_collate());
        });
    }
    set_repertoire(&mut e, repe);
    e
}

/// `e.SetCoercibility(val)` (collationInfo's coer is atomic, so a shared node is changed in
/// place, as in Go).
// glue: dispatch of Expression.SetCoercibility
fn set_coercibility(e: &Expr, val: crate::collation::Coercibility) {
    match e {
        Expr::Column(c) => c.collation_info.set_coercibility(val),
        Expr::CorrelatedColumn(c) => c.column.collation_info.set_coercibility(val),
        Expr::Constant(c) => c.collation_info.set_coercibility(val),
        Expr::ScalarFunction(f) => f.set_coercibility(val),
    }
}

/// `e.SetRepertoire(r)`.
// glue: dispatch of Expression.SetRepertoire
fn set_repertoire(e: &mut ExprRef, r: crate::collation::Repertoire) {
    if e.repertoire() == r {
        return;
    }
    match Arc::make_mut(e) {
        Expr::Column(c) => c.collation_info.set_repertoire(r),
        Expr::CorrelatedColumn(c) => c.column.collation_info.set_repertoire(r),
        Expr::Constant(c) => c.collation_info.set_repertoire(r),
        Expr::ScalarFunction(f) => f.set_repertoire(r),
    }
}

/// `&Constant{Value: value, RetType: tp, DeferredExpr: de}`.
// glue: Constant struct literal of the folding functions
fn new_const(value: Datum, tp: tidb_types::etc::FieldType, deferred: Option<ExprRef>) -> ExprRef {
    let mut c = Constant::new(value, tp);
    c.deferred_expr = deferred;
    Expr::Constant(c).into_ref()
}

// go: isNullHandler L1
fn is_null_handler(ctx: &dyn BuildContext, expr: &ExprRef) -> (ExprRef, bool) {
    let sf = expr.as_scalar_function().expect("scalar function");
    let arg0 = &sf.get_args()[0];
    let ectx = ctx.get_eval_ctx();
    if let Some(const_arg) = arg0.as_constant() {
        let is_deferred_const =
            const_arg.deferred_expr.is_some() || const_arg.param_marker.is_some();
        let value = match sf.eval(&*ectx, &EvalState::new(), Row::default()) {
            Ok(v) => v,
            // Failed to fold this expr to a constant: return the original expression to let
            // the error be evaluated again (Go logs it at DEBUG).
            Err(_) => return (Arc::clone(expr), is_deferred_const),
        };
        if is_deferred_const {
            return (
                new_const(value, sf.ret_type().clone(), Some(Arc::clone(expr))),
                true,
            );
        }
        return (new_const(value, sf.ret_type().clone(), None), false);
    }
    if mysql::has_not_null_flag(arg0.get_type(&*ectx).get_flag()) {
        return (Expr::Constant(new_zero()).into_ref(), false);
    }
    (Arc::clone(expr), false)
}

// go: ifFoldHandler L1
fn if_fold_handler(ctx: &dyn BuildContext, expr: &ExprRef) -> (ExprRef, bool) {
    let sf = expr.as_scalar_function().expect("scalar function");
    let args = sf.get_args();
    let (folded_arg0, _) = fold_constant_inner(ctx, &args[0]);
    if let Some(const_arg) = folded_arg0.as_constant() {
        let ectx = ctx.get_eval_ctx();
        let arg0 = match const_arg.eval_int(&*ectx, &EvalState::new(), Row::default()) {
            Ok(v) => v,
            Err(_) => return (Arc::clone(expr), false),
        };
        if matches!(arg0, Some(v) if v != 0) {
            return fold_constant_inner(ctx, &args[1]);
        }
        return fold_constant_inner(ctx, &args[2]);
    }
    // if the condition is not const, which branch is unknown to run, so directly return.
    (Arc::clone(expr), false)
}

/// Go also sets IFNULL's own result charset/collation to the second argument's (issue
/// 51765) on the node it then drops; the change is visible to other holders of that node,
/// which tidb-rs does not reproduce (the folded result is what callers use).
// go: ifNullFoldHandler L2 (the charset/collation change of the dropped IFNULL node is not made)
fn if_null_fold_handler(ctx: &dyn BuildContext, expr: &ExprRef) -> (ExprRef, bool) {
    let sf = expr.as_scalar_function().expect("scalar function");
    let args = sf.get_args();
    let (folded_arg0, is_deferred) = fold_constant_inner(ctx, &args[0]);
    if let Some(const_arg) = folded_arg0.as_constant() {
        // Only check constArg.Value here. Because deferred expression is evaluated to
        // constArg.Value after foldConstant(args[0]), it's not needed to be checked.
        if const_arg.value.is_null() {
            return fold_constant_inner(ctx, &args[1]);
        }
        return (folded_arg0, is_deferred);
    }
    // if the condition is not const, which branch is unknown to run, so directly return.
    (Arc::clone(expr), false)
}

/// Go writes the folded conditions back into the CASE node's arguments in place; this
/// builds a new node with them.
// go: caseWhenHandler L2 (the folded conditions go into a copy of the node)
fn case_when_handler(ctx: &dyn BuildContext, expr: &ExprRef) -> (ExprRef, bool) {
    let mut expr = Arc::clone(expr);
    let l = expr
        .as_scalar_function()
        .expect("scalar function")
        .get_args()
        .len();
    let ectx = ctx.get_eval_ctx();
    let mut is_deferred_const = false;
    let mut i = 0;
    while i + 1 < l {
        let arg = Arc::clone(&expr.as_scalar_function().unwrap().get_args()[i]);
        let (folded, is_deferred) = fold_constant_inner(ctx, &arg);
        if let Expr::ScalarFunction(sf) = Arc::make_mut(&mut expr) {
            sf.function.args[i] = Arc::clone(&folded);
        }
        is_deferred_const = is_deferred_const || is_deferred;
        if folded.as_constant().is_none() {
            // for no-const, here should return directly, because the following branches are
            // unknown to be run or not
            return (expr, false);
        }
        // If the condition is const and true, and the previous conditions has no expr, then
        // the folded execution body is returned, otherwise the arguments of the casewhen are
        // folded and replaced.
        let val = match folded.eval_int(&*ectx, &EvalState::new(), Row::default()) {
            Ok(v) => v,
            Err(_) => return (expr, false),
        };
        if matches!(val, Some(v) if v != 0) {
            let body = Arc::clone(&expr.as_scalar_function().unwrap().get_args()[i + 1]);
            let (mut folded_expr, is_deferred) = fold_constant_inner(ctx, &body);
            is_deferred_const = is_deferred_const || is_deferred;
            if folded_expr.as_constant().is_some() {
                let dec = expr.get_type(&*ectx).get_decimal();
                crate::builtin_cast::with_type_mut(&mut folded_expr, |t| t.set_decimal(dec));
            }
            return (folded_expr, is_deferred_const);
        }
        i += 2;
    }
    // If the number of arguments in casewhen is odd, and the previous conditions is false,
    // then the folded else execution body is returned. otherwise the execution body of the
    // else are folded and replaced.
    if l % 2 == 1 {
        let body = Arc::clone(&expr.as_scalar_function().unwrap().get_args()[l - 1]);
        let (mut folded_expr, is_deferred) = fold_constant_inner(ctx, &body);
        is_deferred_const = is_deferred_const || is_deferred;
        if folded_expr.as_constant().is_some() {
            let dec = expr.get_type(&*ectx).get_decimal();
            crate::builtin_cast::with_type_mut(&mut folded_expr, |t| t.set_decimal(dec));
        }
        return (folded_expr, is_deferred_const);
    }
    (expr, is_deferred_const)
}

/// A special folding function of [`special_fold_handler`].
type FoldHandler = fn(&dyn BuildContext, &ExprRef) -> (ExprRef, bool);

/// `specialFoldHandler[name]`: the map `init` fills, as a `match` on the name.
// go: init L2 (the package map specialFoldHandler is a match)
fn special_fold_handler(name: &str) -> Option<FoldHandler> {
    Some(match name {
        ast::If => if_fold_handler,
        ast::Ifnull => if_null_fold_handler,
        ast::Case => case_when_handler,
        ast::IsNull => is_null_handler,
        _ => return None,
    })
}

/// MaybeOverOptimized4PlanCache check if this expression is optimized while it may be used
/// by the plan cache (it contains a parameter marker or a deferred constant).
// go: pkg/expression/util.go MaybeOverOptimized4PlanCache L1
pub fn maybe_over_optimized4_plan_cache(ctx: &dyn BuildContext, exprs: &[ExprRef]) -> bool {
    // If we do not enable plan cache, all the optimization can work correctly.
    if !ctx.is_use_cache() {
        return false;
    }
    contain_mutable_const(&*ctx.get_eval_ctx(), exprs)
}

// go: pkg/expression/util.go containMutableConst L1
#[allow(clippy::only_used_in_recursion, clippy::collapsible_match)]
fn contain_mutable_const(ctx: &dyn EvalContext, exprs: &[ExprRef]) -> bool {
    for expr in exprs {
        match &**expr {
            Expr::Constant(v) => {
                if v.param_marker.is_some() || v.deferred_expr.is_some() {
                    return true;
                }
            }
            Expr::ScalarFunction(v) => {
                if contain_mutable_const(ctx, v.get_args()) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// tidb-rs has no extension functions (`extensionFuncSig`). The constants of the null-reject
/// branch get a copy of the node's type; Go shares the node's `*FieldType` (M-FT: nothing in
/// v8.5.8 changes it afterwards, D08.2 spec §4 item 8).
// go: foldConstant L2 (no extension functions in tidb-rs; see the doc comment)
#[allow(clippy::collapsible_if)]
fn fold_constant_inner(ctx: &dyn BuildContext, expr: &ExprRef) -> (ExprRef, bool) {
    match &**expr {
        Expr::ScalarFunction(x) => {
            if unFoldableFunctions.contains(&x.func_name.l.as_str()) {
                return (Arc::clone(expr), false);
            }
            if let Some(function) = special_fold_handler(&x.func_name.l) {
                if !maybe_over_optimized4_plan_cache(ctx, std::slice::from_ref(expr)) {
                    return function(ctx, expr);
                }
            }

            let args = x.get_args();
            let mut has_null_arg = false;
            let mut arg_is_const = vec![false; args.len()];
            let mut all_const_arg = true;
            let mut is_deferred_const = false;
            for (i, arg) in args.iter().enumerate() {
                match arg.as_constant() {
                    Some(c) => {
                        is_deferred_const = is_deferred_const
                            || c.deferred_expr.is_some()
                            || c.param_marker.is_some();
                        arg_is_const[i] = true;
                        has_null_arg = has_null_arg || c.value.is_null();
                    }
                    None => all_const_arg = false,
                }
            }
            if !all_const_arg {
                // NullEQ and ConcatWS are excluded, because they could have different value
                // when the non-constant value is 1 or NULL.
                if !has_null_arg
                    || !ctx.is_in_null_reject_check()
                    || x.func_name.l == ast::NullEQ
                    || x.func_name.l == ast::ConcatWS
                {
                    return (Arc::clone(expr), is_deferred_const);
                }
                let const_args: Vec<ExprRef> = args
                    .iter()
                    .enumerate()
                    .map(|(i, arg)| {
                        if arg_is_const[i] {
                            Arc::clone(arg)
                        } else {
                            Expr::Constant(new_one()).into_ref()
                        }
                    })
                    .collect();
                let ectx = ctx.get_eval_ctx();
                let dummy_scalar_func = match new_function_base(
                    ctx,
                    &x.func_name.l,
                    expr.get_type(&*ectx).into_owned(),
                    const_args,
                ) {
                    Ok(f) => f,
                    Err(_) => return (Arc::clone(expr), is_deferred_const),
                };
                let value = match dummy_scalar_func.eval(&*ectx, &EvalState::new(), Row::default())
                {
                    Ok(v) => v,
                    Err(_) => return (Arc::clone(expr), is_deferred_const),
                };
                if value.is_null() {
                    // This Constant is created to compose the result expression of
                    // EvaluateExprWithNull when InNullRejectCheck is true. We just check
                    // whether the result expression is null or false and then let it die.
                    // Basically, the constant is used once briefly and will not be retained
                    // for a long time. Hence setting DeferredExpr of Constant to nil is ok.
                    return (new_const(value, x.ret_type().clone(), None), false);
                }
                let (is_true, err) = value.to_bool(&type_ctx(&*ectx));
                if err.is_ok() && is_true == 0 {
                    // This Constant is created to compose the result expression of
                    // EvaluateExprWithNull when InNullRejectCheck is true (see above).
                    return (new_const(value, x.ret_type().clone(), None), false);
                }
                return (Arc::clone(expr), is_deferred_const);
            }
            let ectx = ctx.get_eval_ctx();
            let value = x.eval(&*ectx, &EvalState::new(), Row::default());
            let mut ret_type = x.ret_type().clone();
            let value = match value {
                Ok(v) => v,
                Err(_) => return (Arc::clone(expr), is_deferred_const),
            };
            if !has_null_arg {
                // set right not null flag for constant value
                if value.is_null() {
                    ret_type.del_flag(NotNullFlag);
                } else {
                    ret_type.add_flag(NotNullFlag);
                }
            }
            if is_deferred_const {
                return (new_const(value, ret_type, Some(Arc::clone(expr))), true);
            }
            (new_const(value, ret_type, None), false)
        }
        Expr::Constant(x) => {
            let ectx = ctx.get_eval_ctx();
            if let Some(pm) = &x.param_marker {
                let val = match pm.get_user_var(&*ectx) {
                    Ok(v) => v,
                    // Go logs a warning ("fail to get param").
                    Err(_) => return (Arc::clone(expr), true),
                };
                let mut c = Constant::new(val, x.ret_type.clone());
                c.deferred_expr = x.deferred_expr.clone();
                c.param_marker = x.param_marker;
                return (Expr::Constant(c).into_ref(), true);
            } else if let Some(de) = &x.deferred_expr {
                let value = match de.eval(&*ectx, &EvalState::new(), Row::default()) {
                    Ok(v) => v,
                    Err(_) => return (Arc::clone(expr), true),
                };
                return (
                    new_const(value, x.ret_type.clone(), x.deferred_expr.clone()),
                    true,
                );
            }
            (Arc::clone(expr), false)
        }
        _ => (Arc::clone(expr), false),
    }
}
