//! go-file: pkg/expression/util.go
//!
//! The parts of `util.go` (block D08.12) that earlier blocks need; the rest of the file
//! comes with D08.12.

use tidb_base::mysql::ParseToJSONFlag;

use crate::builtin_cast::with_type_mut;
use crate::context::EvalContext;
use crate::expression::{Expr, ExprRef};

/// DisableParseJSONFlag4Expr disables ParseToJSONFlag for `expr` except Column.
/// We should not *PARSE* a string as JSON under some scenarios. ParseToJSONFlag
/// is 0 for JSON column yet(as well as JSON correlated column), so we can skip
/// it. Moreover, Column.RetType refers to the infoschema, if we modify it, data
/// race may happen if another goroutine read from the infoschema at the same
/// time.
///
/// A scalar function's `RetType` is its signature's `tp` (one type, D08.1 spec §3.2), so
/// the signature sees the change as in Go: `CAST(c AS JSON)` stops parsing the string
/// (v8.5.8: `CAST('[1]' AS JSON) = '[1]'` is 0, Lucky review #94). A node shared with other
/// holders is copied first (spec §4, [`with_type_mut`]).
// go: DisableParseJSONFlag4Expr L2 (a shared node is copied before the change, D08.1 spec §4)
pub fn disable_parse_json_flag4_expr(_ctx: &dyn EvalContext, expr: &mut ExprRef) {
    if matches!(&**expr, Expr::Column(_) | Expr::CorrelatedColumn(_)) {
        return;
    }
    with_type_mut(expr, |tp| tp.set_flag(tp.get_flag() & !ParseToJSONFlag));
}

/// `logicalOps`: the functions whose result is a boolean (`wrapWithIsTrue` does not wrap
/// them when `wrapForInt`).
// glue: Go's logicalOps map (util.go) as a match over its keys
pub fn is_logical_op(name: &str) -> bool {
    use crate::ast_funcs as ast;
    matches!(
        name,
        ast::LT
            | ast::GE
            | ast::GT
            | ast::LE
            | ast::EQ
            | ast::NE
            | ast::UnaryNot
            | ast::LogicAnd
            | ast::LogicOr
            | ast::LogicXor
            | ast::In
            | ast::IsNull
            | ast::IsTruthWithoutNull
            | ast::IsFalsity
            | ast::Like
    )
}

// ---------------------------------------------------------------- constant propagation helpers

use std::sync::Arc;

use tidb_base::mysql;
use tidb_parser::gocharset::{CollationUTF8, CollationUTF8MB4};
use tidb_types::collate::new_collation_enabled;
use tidb_types::field_type::new_field_type;

use crate::ast_funcs as ast;
use crate::builtin_cast::build_cast_function_with_check;
use crate::collation::{
    Coercibility, check_and_derive_collation_from_exprs, collation_strictness,
    collation_strictness_group,
};
use crate::column::Column;
use crate::context::BuildContext;
use crate::expression::{ConstOnlyInContext, ConstStrict};
use crate::function_traits::mutableEffectsFunctions;
use crate::scalar_function::{new_function, new_function_internal};
use crate::schema::Schema;

/// ExtractColumns extracts all columns from an expression.
// go: ExtractColumns L1
pub fn extract_columns(expr: &ExprRef) -> Vec<Arc<Column>> {
    // Pre-allocate a slice to reduce allocation, 8 doesn't have special meaning.
    let mut result = Vec::with_capacity(8);
    extract_columns_impl(&mut result, expr, None);
    result
}

/// `extractColumns`: Go returns the grown slice; here it is appended to in place. A column
/// is copied into a new `Arc` (Go keeps the `*Column` of the expression; the copy holds the
/// same fields).
// go: extractColumns L2 (appends in place; the column is copied into an Arc)
fn extract_columns_impl(
    result: &mut Vec<Arc<Column>>,
    expr: &ExprRef,
    filter: Option<&dyn Fn(&Column) -> bool>,
) {
    match &**expr {
        Expr::Column(v) => {
            if filter.is_none_or(|f| f(v)) {
                result.push(Arc::new(v.clone()));
            }
        }
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                extract_columns_impl(result, arg, filter);
            }
        }
        _ => {}
    }
}

/// cowExprRef is a copy-on-write slice ref util using in `ColumnSubstitute`
/// to reduce unnecessary allocation for Expression arguments array
struct CowExprRef<'a> {
    r#ref: &'a [ExprRef],
    new: Option<Vec<ExprRef>>,
}

impl CowExprRef<'_> {
    /// Set will allocate new array if changed flag true
    // go: cowExprRef.Set L1
    fn set(&mut self, i: usize, changed: bool, val: ExprRef) {
        if let Some(new) = &mut self.new {
            new[i] = val;
            return;
        }
        if !changed {
            return;
        }
        let mut new = self.r#ref.to_vec();
        new[i] = val;
        self.new = Some(new);
    }

    /// Result return the final reference
    // go: cowExprRef.Result L1
    fn result(&self) -> &[ExprRef] {
        match &self.new {
            Some(new) => new,
            None => self.r#ref,
        }
    }
}

/// SetExprColumnInOperand is used to set columns in expr as InOperand. Go sets the flag on
/// the arguments of a scalar function in place; a shared node is copied first here (D08.1
/// spec §4).
// go: SetExprColumnInOperand L2 (a shared node is copied before the change, D08.1 spec §4)
pub fn set_expr_column_in_operand(expr: &ExprRef) -> ExprRef {
    match &**expr {
        Expr::Column(v) => {
            let mut col = v.clone();
            col.in_operand = true;
            Expr::Column(col).into_ref()
        }
        Expr::ScalarFunction(_) => {
            let mut e = Arc::clone(expr);
            if let Expr::ScalarFunction(v) = Arc::make_mut(&mut e) {
                for arg in v.function.args.iter_mut() {
                    *arg = set_expr_column_in_operand(arg);
                }
                v.clean_hash_code();
            }
            e
        }
        _ => Arc::clone(expr),
    }
}

/// `Expression.SetCoercibility` on any node (the coercibility is atomic, as Go writes it on
/// the node in place).
// glue: dispatch of Expression.SetCoercibility
fn set_coercibility(e: &ExprRef, c: Coercibility) {
    match &**e {
        Expr::Column(v) => v.collation_info.set_coercibility(c),
        Expr::CorrelatedColumn(v) => v.column.collation_info.set_coercibility(c),
        Expr::Constant(v) => v.collation_info.set_coercibility(c),
        Expr::ScalarFunction(v) => v.set_coercibility(c),
    }
}

/// ColumnSubstitute substitutes the columns in filter to expressions in select fields.
/// e.g. select * from (select b as a from t) k where a < 10 => select * from (select b as a from t where b < 10) k.
/// TODO: remove this function and only use ColumnSubstituteImpl since this function swallows the error, which seems unsafe.
// go: ColumnSubstitute L1
pub fn column_substitute(
    ctx: &dyn BuildContext,
    expr: &ExprRef,
    schema: &Schema,
    new_exprs: &[ExprRef],
) -> ExprRef {
    let (_, _, res_expr) = column_substitute_impl(ctx, expr, schema, new_exprs, false);
    res_expr
}

/// ColumnSubstituteAll substitutes the columns just like ColumnSubstitute, but we don't
/// accept partial substitution.
// go: ColumnSubstituteAll L1
pub fn column_substitute_all(
    ctx: &dyn BuildContext,
    expr: &ExprRef,
    schema: &Schema,
    new_exprs: &[ExprRef],
) -> (bool, ExprRef) {
    let (_, has_fail, res_expr) = column_substitute_impl(ctx, expr, schema, new_exprs, true);
    (has_fail, res_expr)
}

/// ColumnSubstituteImpl tries to substitute column expr using newExprs,
/// the newFunctionInternal is only called if its child is substituted
/// @return bool means whether the expr has changed.
/// @return bool means whether the expr should change (has the dependency in schema, while the corresponding expr has some compatibility), but finally fallback.
/// @return Expression, the original expr or the changed expr, it depends on the first @return bool.
// go: ColumnSubstituteImpl L1
pub fn column_substitute_impl(
    ctx: &dyn BuildContext,
    expr: &ExprRef,
    schema: &Schema,
    new_exprs: &[ExprRef],
    fail1_return: bool,
) -> (bool, bool, ExprRef) {
    let ev = ctx.get_eval_ctx();
    match &**expr {
        Expr::Column(v) => {
            let id = schema.column_index(v);
            if id == -1 {
                return (false, false, Arc::clone(expr));
            }
            let mut new_expr = Arc::clone(&new_exprs[id as usize]);
            if v.in_operand {
                new_expr = set_expr_column_in_operand(&new_expr);
            }
            (true, false, new_expr)
        }
        Expr::ScalarFunction(v) => {
            let mut substituted = false;
            let mut has_fail = false;
            if v.func_name.l == ast::Cast || v.func_name.l == ast::Grouping {
                let (sub, fail, new_arg) =
                    column_substitute_impl(ctx, &v.get_args()[0], schema, new_exprs, fail1_return);
                substituted = sub;
                has_fail = fail;
                if fail1_return && has_fail {
                    return (substituted, has_fail, Arc::clone(expr));
                }
                if substituted {
                    let flag = v.ret_type().get_flag();
                    let mut e = if v.func_name.l == ast::Cast {
                        // If the newArg is a ScalarFunction(cast), BuildCastFunctionWithCheck
                        // will modify the newArg.RetType, so Go deep-copies it; a node here is
                        // never changed in place while shared (D08.1 spec §4).
                        match build_cast_function_with_check(
                            ctx,
                            new_arg,
                            v.ret_type().clone(),
                            false,
                            v.is_explicit_charset(),
                        ) {
                            Ok(e) => e,
                            Err(err) => {
                                // Go logs the error and goes on with the node that has no
                                // signature; this cannot build one.
                                tidb_base::terror::log(Some(&err));
                                return (false, false, Arc::clone(expr));
                            }
                        }
                    } else {
                        // for grouping function recreation, use clone (meta included) instead of newFunction
                        let mut sf = v.clone();
                        sf.function.args[0] = new_arg;
                        sf.clean_hash_code();
                        Expr::ScalarFunction(sf).into_ref()
                    };
                    set_coercibility(&e, v.coercibility());
                    with_type_mut(&mut e, |tp| tp.set_flag(flag));
                    return (true, false, e);
                }
                return (false, false, Arc::clone(expr));
            }
            // If the collation of the column is PAD SPACE,
            // we can't propagate the constant to the length function.
            // For example, schema = ['name'], newExprs = ['a'], v = length(name).
            // We can't substitute name with 'a' in length(name) because the collation of name is PAD SPACE.
            // TODO: We will fix it here temporarily, and redesign the logic if we encounter more similar functions or situations later.
            // Fixed issue #53730
            if ctx.is_constant_propagate_check()
                && v.func_name.l == ast::Length
                && let Expr::Column(arg0) = &*v.get_args()[0]
            {
                let id = schema.column_index(arg0);
                if id != -1 && matches!(&*new_exprs[id as usize], Expr::Constant(_)) {
                    let mapped_new_column_collate =
                        schema.columns[id as usize].get_static_type().get_collate();
                    if mapped_new_column_collate == CollationUTF8MB4
                        || mapped_new_column_collate == CollationUTF8
                    {
                        return (false, false, Arc::clone(expr));
                    }
                }
            }
            // cowExprRef is a copy-on-write util, args array allocation happens only
            // when expr in args is changed
            let mut ref_expr_arr = CowExprRef {
                r#ref: v.get_args(),
                new: None,
            };
            let old_coll_et = match check_and_derive_collation_from_exprs(
                ctx,
                &v.func_name.l,
                v.ret_type().eval_type(),
                v.get_args(),
            ) {
                Ok(c) => c,
                Err(_) => {
                    // Go logs "Unexpected error happened during ColumnSubstitution".
                    return (false, false, Arc::clone(expr));
                }
            };
            let mut tmp_arg_for_coll_check: Vec<ExprRef> = Vec::new();
            for (idx, arg) in v.get_args().iter().enumerate() {
                let (mut changed, failed, new_func_expr) =
                    column_substitute_impl(ctx, arg, schema, new_exprs, fail1_return);
                if fail1_return && failed {
                    return (changed, failed, Arc::clone(expr));
                }
                let old_changed = changed;
                if new_collation_enabled() && changed {
                    // Make sure the collation used by the ScalarFunction isn't changed and its
                    // result collation is not weaker than the collation used by the ScalarFunction.
                    changed = false;
                    tmp_arg_for_coll_check.clear();
                    tmp_arg_for_coll_check.extend(ref_expr_arr.result().iter().cloned());
                    tmp_arg_for_coll_check[idx] = Arc::clone(&new_func_expr);
                    let new_coll_et = match check_and_derive_collation_from_exprs(
                        ctx,
                        &v.func_name.l,
                        v.ret_type().eval_type(),
                        &tmp_arg_for_coll_check,
                    ) {
                        Ok(c) => c,
                        Err(_) => return (false, failed, Arc::clone(expr)),
                    };
                    if old_coll_et.collation == new_coll_et.collation {
                        if new_func_expr.get_type(&*ev).get_collate()
                            == arg.get_type(&*ev).get_collate()
                            && new_func_expr.coercibility() == arg.coercibility()
                        {
                            // It's safe to use the new expression, otherwise some cases in projection push-down will be wrong.
                            changed = true;
                        } else {
                            changed = check_collation_strictness(
                                &old_coll_et.collation,
                                new_func_expr.get_type(&*ev).get_collate(),
                            );
                        }
                    }
                }
                has_fail = has_fail || failed || old_changed != changed;
                if fail1_return && old_changed != changed {
                    // Only when the oldChanged is true and changed is false, we will get here.
                    // And this means there some dependency in this arg can be substituted with
                    // given expressions, while it has some collation compatibility, finally we
                    // fall back to use the origin args. (commonly used in projection elimination
                    // in which fallback usage is unacceptable)
                    return (changed, true, Arc::clone(expr));
                }
                ref_expr_arr.set(idx, changed, new_func_expr);
                if changed {
                    substituted = true;
                }
            }
            if substituted {
                return match new_function(
                    ctx,
                    &v.func_name.l,
                    v.ret_type().clone(),
                    ref_expr_arr.result().to_vec(),
                ) {
                    Ok(new_func) => (true, has_fail, new_func),
                    Err(_) => (true, true, Arc::clone(expr)),
                };
            }
            (false, false, Arc::clone(expr))
        }
        _ => (false, false, Arc::clone(expr)),
    }
}

/// checkCollationStrictness check collation strictness-ship between `coll` and `newFuncColl`
/// return true iff `newFuncColl` is not weaker than `coll`
// go: checkCollationStrictness L1
fn check_collation_strictness(coll: &str, new_func_coll: &str) -> bool {
    if let (Some(coll_group_id), Some(new_func_coll_group_id)) = (
        collation_strictness_group(coll),
        collation_strictness_group(new_func_coll),
    ) {
        if coll_group_id == new_func_coll_group_id {
            return true;
        }
        for &id in collation_strictness(coll_group_id).unwrap_or(&[]) {
            if new_func_coll_group_id == id {
                return true;
            }
        }
    }
    false
}

/// BuildNotNullExpr wraps up `not(isnull())` for given expression. `None` is Go's nil
/// Expression (`NewFunctionInternal` failed).
// go: BuildNotNullExpr L1
pub fn build_not_null_expr(ctx: &dyn BuildContext, expr: &ExprRef) -> Option<ExprRef> {
    let is_null = new_function_internal(
        ctx,
        ast::IsNull,
        new_field_type(mysql::TypeTiny),
        vec![Arc::clone(expr)],
    )?;
    new_function_internal(
        ctx,
        ast::UnaryNot,
        new_field_type(mysql::TypeTiny),
        vec![is_null],
    )
}

/// IsMutableEffectsExpr checks if expr contains function which is mutable or has side effects.
// go: IsMutableEffectsExpr L1
pub fn is_mutable_effects_expr(expr: &Expr) -> bool {
    match expr {
        Expr::ScalarFunction(x) => {
            if mutableEffectsFunctions.contains(&x.func_name.l.as_str()) {
                return true;
            }
            for arg in x.get_args() {
                if is_mutable_effects_expr(arg) {
                    return true;
                }
            }
        }
        Expr::Constant(x) => {
            if let Some(de) = &x.deferred_expr {
                return is_mutable_effects_expr(de);
            }
        }
        _ => {}
    }
    false
}

/// RemoveDupExprs removes identical exprs. Not that if expr contains functions which
/// are mutable or have side effects, we cannot remove it even if it has duplicates;
/// if the plan is going to be cached, we cannot remove expressions containing `?` neither.
// go: RemoveDupExprs L1
pub fn remove_dup_exprs(exprs: Vec<ExprRef>) -> Vec<ExprRef> {
    if exprs.len() <= 1 {
        return exprs;
    }
    let mut res = Vec::with_capacity(exprs.len());
    let mut exists = std::collections::HashSet::with_capacity(exprs.len());
    for expr in exprs {
        let key = expr.hash_code().to_vec();
        if !exists.contains(&key) || is_mutable_effects_expr(&expr) {
            exists.insert(key);
            res.push(expr);
        }
    }
    res
}

/// ConstExprConsiderPlanCache indicates whether the expression can be considered as a
/// constant expression considering planCache. If the expression is in plan cache, it should
/// have a const level `ConstStrict` because it can be shared across statements. If the
/// expression is not in plan cache, `ConstOnlyInContext` is enough because it is only used
/// in one statement.
// go: ConstExprConsiderPlanCache L1
pub fn const_expr_consider_plan_cache(expr: &Expr, in_plan_cache: bool) -> bool {
    let level = expr.const_level();
    if level == ConstStrict {
        true
    } else if level == ConstOnlyInContext {
        !in_plan_cache
    } else {
        false
    }
}
